mod camera;
mod ha;
pub mod onvif;
mod update;

use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use serde_json::{Map, Value, json};
use tiny_http::{Header, Method, Request, Response, Server};
use wts_common::{Config, RTSP_PORT, SNAPSHOT, version};

const PANEL: &str = include_str!("../panel.html");
const LOG_UNITS: [&str; 3] = ["wts-kiosk", "wts-server", "wts-update"];

pub struct State {
    pub data: PathBuf,
    pub port: u16,
    cfg: Mutex<Config>,
    kiosk: Mutex<Kiosk>,
    camera: Arc<camera::Supervisor>,
    ha: Arc<ha::Logins>,
    updater: update::Updater,
    onvif_port: AtomicU16,
}

#[derive(Default)]
struct Kiosk {
    seen: Option<Instant>,
    /// seconds the kiosk process runs, as it reports
    up: u64,
    tab: String,
    error: String,
    safe: bool,
}

impl State {
    pub fn new(data: PathBuf, port: u16) -> Arc<State> {
        let cfg = Config::from_json(&fs::read_to_string(data.join("config.json")).unwrap_or_default());
        Arc::new(State {
            ha: Arc::new(ha::Logins::new(data.join("ha_tokens.json"))),
            updater: update::Updater::new(data.join("update.log"), data.join("update.env")),
            data,
            port,
            cfg: Mutex::new(cfg),
            kiosk: Mutex::new(Kiosk::default()),
            camera: Arc::new(camera::Supervisor::new()),
            onvif_port: AtomicU16::new(port),
        })
    }

    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    /// Atomic write: a power cut never leaves a half-written config.json.
    fn save(&self, cfg: &Config) -> Result<(), String> {
        fs::create_dir_all(&self.data).map_err(|e| format!("{}: {e}", self.data.display()))?;
        let tmp = self.data.join("config.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(cfg).expect("config serializes"))
            .and_then(|()| fs::rename(&tmp, self.data.join("config.json")))
            .map_err(|e| format!("config.json: {e}"))
    }

    /// Validates and stores a change; returns the config before and after.
    fn change(&self, f: impl FnOnce(&Config) -> Result<Config, String>) -> Result<(Config, Config), String> {
        let mut cfg = self.cfg.lock().unwrap();
        let new = f(&cfg)?;
        if new != *cfg {
            self.save(&new)?;
        }
        Ok((std::mem::replace(&mut *cfg, new.clone()), new))
    }

    fn kiosk_online(&self) -> bool {
        self.kiosk.lock().unwrap().seen.is_some_and(|t| t.elapsed() < Duration::from_secs(10))
    }

    fn onvif_ctx(&self, ip: String, port: u16) -> onvif::Ctx {
        onvif::Ctx { ip, port, camera: self.config().camera, hostname: hostname() }
    }
}

/// Local time as HH:MM:SS for log lines in the panel.
pub fn clock() -> String {
    #[cfg(unix)]
    {
        // SAFETY: time and localtime_r only write into the zeroed tm passed in
        let tm = unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            tm
        };
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
    #[cfg(not(unix))]
    {
        let s = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86400;
        format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }
}

pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "walltablet".into())
}

/// The local address the kernel would use to reach `peer` (no packet is sent).
pub fn local_ip_for(peer: std::net::IpAddr) -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

pub fn run(data: PathBuf, port: u16) -> Result<(), String> {
    let st = State::new(data, port);
    st.save(&st.config())?;
    serve(&st, port)?;
    match serve(&st, 80) {
        // some NVRs only try port 80 when a camera is added by IP
        Ok(_) => st.onvif_port.store(80, Ordering::Relaxed),
        Err(e) => println!("Port 80 nicht verfügbar ({e}), ONVIF nur auf {port}"),
    }
    st.camera.apply(st.config().camera);
    let s = st.clone();
    thread::spawn(move || s.updater.run_loop(&s));
    let s = st.clone();
    thread::spawn(move || onvif::discovery(|ip| s.onvif_ctx(ip, s.onvif_port.load(Ordering::Relaxed))));
    println!("WallTabletSuite {} auf http://0.0.0.0:{port}", version());
    loop {
        thread::park();
    }
}

/// Serves the panel and API on `port` (0 = any free port); returns the port.
pub fn serve(st: &Arc<State>, port: u16) -> Result<u16, String> {
    let server = Server::http(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    let actual = server.server_addr().to_ip().map_or(port, |a| a.port());
    let st = st.clone();
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let st = st.clone();
            thread::spawn(move || handle(&st, req));
        }
    });
    Ok(actual)
}

struct Reply {
    code: u16,
    ctype: &'static str,
    body: Vec<u8>,
}

fn reply_json(code: u16, v: &Value) -> Reply {
    Reply { code, ctype: "application/json; charset=utf-8", body: v.to_string().into_bytes() }
}

fn handle(st: &Arc<State>, mut req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = match path.trim_end_matches('/') {
        "" => "/",
        p => p,
    };
    let r = route(st, &mut req, path, &parse_query(query)).unwrap_or_else(|e| reply_json(400, &json!({"error": e})));
    let resp = Response::from_data(r.body)
        .with_status_code(r.code)
        .with_header(Header::from_bytes("Content-Type", r.ctype).unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    let _ = req.respond(resp);
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect()
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h * 16 + l);
                i += 2;
            }
            (b'+', ..) => out.push(b' '),
            (c, ..) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn body(req: &mut Request) -> Result<Vec<u8>, String> {
    let mut b = Vec::new();
    req.as_reader().take(1_000_001).read_to_end(&mut b).map_err(|e| e.to_string())?;
    if b.len() > 1_000_000 {
        return Err("Anfrage zu groß".into());
    }
    Ok(b)
}

fn body_json(req: &mut Request) -> Result<Map<String, Value>, String> {
    let b = body(req)?;
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&b) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("JSON-Objekt erwartet".into()),
        Err(_) => Err("Kein gültiges JSON".into()),
    }
}

/// Host and port the client used to reach us (Host header), e.g. for URLs in ONVIF answers.
fn host_of(req: &Request) -> Option<(String, Option<u16>)> {
    let h = req.headers().iter().find(|h| h.field.equiv("Host"))?.value.as_str().trim().to_string();
    if let Some(rest) = h.strip_prefix('[') {
        let (ip, tail) = rest.split_once(']')?;
        return Some((format!("[{ip}]"), tail.strip_prefix(':').and_then(|p| p.parse().ok())));
    }
    match h.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok())),
        None => Some((h, None)),
    }
}

fn route(st: &Arc<State>, req: &mut Request, path: &str, q: &HashMap<String, String>) -> Result<Reply, String> {
    let method = req.method().clone();
    if method == Method::Post && path.starts_with("/onvif/") {
        // ONVIF also reports this as the IPv4 address (GetNetworkInterfaces), so a hostname in Host is replaced
        // by the local socket IP like WS-Discovery advertises
        let local = || req.remote_addr().and_then(|a| local_ip_for(a.ip())).unwrap_or_else(|| "127.0.0.1".into());
        let (ip, port) = match host_of(req) {
            Some((host, port)) if host.parse::<std::net::Ipv4Addr>().is_ok() => (host, port.unwrap_or(80)),
            Some((_, port)) => (local(), port.unwrap_or(80)),
            None => (local(), st.port),
        };
        let (code, xml) = onvif::handle(&body(req)?, &st.onvif_ctx(ip, port));
        return Ok(Reply { code, ctype: "application/soap+xml; charset=utf-8", body: xml.into_bytes() });
    }
    let ok = |v: Value| Ok(reply_json(200, &v));
    match (method, path) {
        (Method::Get, "/") => Ok(Reply { code: 200, ctype: "text/html; charset=utf-8", body: PANEL.as_bytes().to_vec() }),
        (Method::Get, "/api/status") => ok(status(st)),
        (Method::Get, "/api/health") => {
            // update.sh: the new version counts as good once the server answers and the kiosk runs normally
            // for 20 s: a kiosk that crashes right after its first poll must not pass
            let good = st.kiosk_online() && {
                let k = st.kiosk.lock().unwrap();
                !k.safe && k.up >= 20
            };
            Ok(reply_json(if good { 200 } else { 503 }, &json!({"ok": good})))
        }
        (Method::Get, "/api/kiosk") => ok(kiosk(st, req, q)),
        (Method::Get | Method::Post, "/api/lock") => {
            let mut locked = true;
            if *req.method() == Method::Post {
                match body_json(req)?.get("locked") {
                    None => {}
                    Some(Value::Bool(b)) => locked = *b,
                    Some(_) => return Err("locked muss true oder false sein".into()),
                }
            }
            ok(set_locked(st, locked)?)
        }
        (Method::Get | Method::Post, "/api/unlock") => {
            if *req.method() == Method::Post {
                body(req)?;
            }
            ok(set_locked(st, false)?)
        }
        (Method::Get, "/api/config") => ok(config_view(st, req)),
        (Method::Post, "/api/config") => {
            let data = body_json(req)?;
            let (old, new) = st.change(|c| c.apply(&data))?;
            if new.camera != old.camera {
                let strip = |c: &wts_common::Camera| wts_common::Camera { controls: Default::default(), ..c.clone() };
                let only_controls = strip(&new.camera) == strip(&old.camera);
                // image controls go to the running camera, everything else restarts it
                if !(only_controls && st.camera.set_controls(&new.camera)) {
                    st.camera.apply(new.camera.clone());
                }
            }
            ok(json!({"ok": true}))
        }
        (Method::Post, "/api/update/check") => {
            body(req)?;
            ok(st.updater.check(&st.config().update))
        }
        (Method::Post, "/api/update/install") => {
            body(req)?;
            st.updater.install(&st.config().update)?;
            ok(json!({"ok": true}))
        }
        (Method::Get, "/snapshot.jpg") => {
            match fs::read(SNAPSHOT) {
                Ok(jpeg) if snapshot_fresh() =>Ok(Reply { code: 200, ctype: "image/jpeg", body: jpeg }),
                _ => Ok(reply_json(503, &json!({"error": "kein Kamerabild"}))),
            }
        }
        (Method::Get, "/api/camera/probe") => ok(st.camera.probe()),
        (Method::Get, "/api/camera/diag") => ok(json!({"lines": camera_diag(&st.camera)})),
        (Method::Get, "/api/logs") => {
            let unit = q.get("unit").map_or("wts-kiosk", String::as_str);
            if !LOG_UNITS.contains(&unit) {
                return Err(format!("unit: {}", LOG_UNITS.join(", ")));
            }
            ok(json!({"lines": journal(unit)}))
        }
        _ => Ok(reply_json(404, &json!({"error": "nicht gefunden"}))),
    }
}

/// The camera rewrites the snapshot every second while frames arrive.
fn snapshot_fresh() -> bool {
    fs::metadata(SNAPSHOT)
        .and_then(|m| m.modified())
        .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < Duration::from_secs(10))
}

fn set_locked(st: &State, locked: bool) -> Result<Value, String> {
    st.change(|c| Ok(Config { locked, ..c.clone() }))?;
    Ok(json!({"locked": locked}))
}

fn status(st: &State) -> Value {
    let cfg = st.config();
    let (tab, safe) = {
        let k = st.kiosk.lock().unwrap();
        (k.tab.clone(), k.safe)
    };
    let detail = if safe {
        "sicherer Modus nach Abstürzen".to_string()
    } else if cfg.locked {
        "gesperrt".into()
    } else if tab.is_empty() {
        String::new()
    } else {
        format!("Tab {tab}")
    };
    let online = st.kiosk_online();
    let mut devices = vec![json!({"name": hostname(), "kind": "Tablet",
        "state": if !online { "offline" } else if safe { "error" } else { "ok" }, "detail": detail})];
    devices.push(if !cfg.camera.enabled {
        json!({"name": "Kamera", "kind": "Kamera", "state": "ok", "detail": "aus"})
    } else if st.camera.running() && !snapshot_fresh() {
        // the process lives but no frames come (e.g. a stalled libcamera pipeline), or it just started
        json!({"name": "Kamera", "kind": "Kamera", "state": "warn", "detail": "keine Bilder"})
    } else if st.camera.running() {
        json!({"name": "Kamera", "kind": "Kamera", "state": "ok", "detail": format!("RTSP, {} Client(s)", st.camera.clients())})
    } else {
        json!({"name": "Kamera", "kind": "Kamera", "state": "error", "detail": st.camera.last_log().unwrap_or_else(|| "läuft nicht".into())})
    });
    json!({"version": version(), "locked": cfg.locked, "devices": devices})
}

fn kiosk(st: &State, req: &Request, q: &HashMap<String, String>) -> Value {
    {
        let mut k = st.kiosk.lock().unwrap();
        k.seen = Some(Instant::now());
        k.tab = q.get("tab").map(|t| t.chars().take(40).collect()).unwrap_or_default();
        k.error = q.get("error").map(|t| t.chars().take(300).collect()).unwrap_or_default();
        k.safe = q.get("safe").is_some_and(|s| s == "1");
        k.up = q.get("up").and_then(|u| u.parse().ok()).unwrap_or(0);
    }
    let cfg = st.config();
    // HA tokens only for the kiosk on this machine: the API has no login
    let local = req.remote_addr().is_some_and(|a: &SocketAddr| a.ip().is_loopback());
    let login = local && !cfg.ha.username.is_empty() && !cfg.ha.password.is_empty();
    let tabs: Vec<Value> = cfg.tabs.iter().map(|t| {
        let mut v = json!({"name": t.name, "url": t.url, "ha": t.ha});
        if t.ha {
            v["ha_theme"] = json!(cfg.ha.theme);
            if login
                && let Some(tokens) = st.ha.get(&wts_common::origin(&t.url), &cfg.ha.username, &cfg.ha.password) {
                    v["ha_tokens"] = tokens;
                }
        }
        v
    }).collect();
    json!({"locked": cfg.locked, "rotation": cfg.rotation, "tabs": tabs})
}

fn config_view(st: &State, req: &Request) -> Value {
    let host = host_of(req).map_or_else(|| "localhost".into(), |h| h.0);
    let mut c = serde_json::to_value(st.config()).expect("config serializes");
    let has_password = c["ha"]["password"].as_str().is_some_and(|p| !p.is_empty());
    c["ha"].as_object_mut().unwrap().remove("password"); // never sent out, the API has no login
    let kiosk = {
        let k = st.kiosk.lock().unwrap(); // released before the camera calls below, never hold both
        json!({"online": k.seen.is_some_and(|t| t.elapsed() < Duration::from_secs(10)), "tab": k.tab,
               "error": k.error, "safe": k.safe, "up": k.up})
    };
    json!({
        "config": c,
        "version": version(),
        "ha": {"has_password": has_password, "state": st.ha.state()},
        "kiosk": kiosk,
        "camera": {"running": st.camera.running(), "clients": st.camera.clients(), "log": st.camera.log(),
                   "rtsp": format!("rtsp://{host}:{RTSP_PORT}/stream"), "rtsp_sub": format!("rtsp://{host}:{RTSP_PORT}/sub"),
                   "onvif": format!("http://{host}:{}/onvif/device_service", st.port)},
        "update": st.updater.view(),
    })
}

/// Why libcamera finds (no) camera: fixed, read-only commands and files, as text to copy from the panel.
/// Output of a diagnostic command, given up after `secs`. A process stuck in a camera driver (D state)
/// survives even SIGKILL and keeps its pipes open, so the waiting thread is abandoned instead of joined.
fn run_limited(args: &[&str], secs: u64) -> String {
    use std::process::{Command, Stdio};
    let child = match Command::new(args[0]).args(&args[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return format!("{}: {e}", args[0]),
    };
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(Ok(o)) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Ok(Err(e)) => format!("{}: {e}", args[0]),
        Err(_) => {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = pid;
            format!("{}: keine Antwort nach {secs} s, abgebrochen (hängt vermutlich im Treiber)", args[0])
        }
    }
}

fn camera_diag(cam: &camera::Supervisor) -> Vec<String> {
    let run = |args: &[&str]| run_limited(args, 10);
    let read = |p: &str| fs::read_to_string(p).unwrap_or_default();
    let mut out = vec!["== System".to_string()];
    let os = read("/etc/os-release");
    let name = os.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).unwrap_or("?").trim_matches('"');
    out.push(format!("{name}, Kernel {}, WallTabletSuite {}", read("/proc/sys/kernel/osrelease").trim(), version()));

    out.push("== Video-Geräte".into());
    let mut dev: Vec<String> = fs::read_dir("/dev").into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| ["video", "media", "v4l-subdev"].iter().any(|p| n.starts_with(p)))
        .collect();
    dev.sort();
    out.push(if dev.is_empty() { "keine".into() } else { dev.join(" ") });

    // sensors, their power chip and the IPU as the firmware lists them (status 15 = present, enabled)
    out.push("== ACPI (Sensoren, IPU)".into());
    for e in fs::read_dir("/sys/bus/acpi/devices").into_iter().flatten().flatten() {
        let p = e.path();
        let hid = read(&format!("{}/hid", p.display())).trim().to_string();
        if ["OVTI", "INT33BE", "INT347", "INT343", "SONY", "HIMX", "GCTI", "TPS68"].iter().any(|x| hid.starts_with(x)) {
            out.push(format!("{} {hid} status={}", e.file_name().to_string_lossy(), read(&format!("{}/status", p.display())).trim()));
        }
    }

    out.push("== Kernel-Module".into());
    let mods: Vec<String> = read("/proc/modules").lines().filter_map(|l| l.split(' ').next()).map(str::to_string)
        .filter(|m| ["ipu", "cio2", "int3472", "tps68470", "ov", "imx", "dw97", "v4l2", "videobuf"].iter().any(|x| m.contains(x)))
        .collect();
    out.push(if mods.is_empty() { "keine passenden".into() } else { mods.join(" ") });

    out.push("== Kernel-Log".into());
    out.extend(run(&["journalctl", "-k", "-b", "--no-pager", "-o", "cat", "-n", "60",
        "-g", "(?i)ipu3|cio2|imgu|int3472|tps68470|ipu.bridge|ov[0-9]{4}|sensor"]).lines().map(str::to_string));

    out.push("== libcamera".into());
    out.extend(run(&["sh", "-c", "ldconfig -p | grep -i 'libcamera\\.so'; gst-inspect-1.0 libcamerasrc 2>&1 | grep -E 'Filename|Version'"])
        .lines().map(str::to_string));
    out.push("== cam -l".into());
    if cam.running() {
        // a second libcamera user next to the running camera can hang in the IPU3 driver
        out.push("übersprungen, die Kamera läuft (zum Prüfen Kamera ausschalten)".into());
    } else {
        out.extend(run(&["cam", "-l"]).lines().map(str::to_string));
    }
    out.push("== Kamera-Log".into());
    out.extend(cam.log().into_iter().rev().take(40).rev());
    out
}

/// Last lines of a unit's journal from this boot. The kiosk user reads its own entries (journald keeps
/// per-user files for regular users) and everything once it is in the systemd-journal group.
fn journal(unit: &str) -> Vec<String> {
    match std::process::Command::new("journalctl")
        .args(["-u", &format!("{unit}.service"), "-b", "-n", "300", "--no-pager", "-o", "short-iso"])
        .output()
    {
        Ok(o) => {
            let text = if o.stdout.is_empty() { o.stderr } else { o.stdout };
            String::from_utf8_lossy(&text).lines().map(str::to_string).collect()
        }
        Err(e) => vec![format!("journalctl: {e}")],
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn diag_command_gives_up() {
        let t = std::time::Instant::now();
        assert!(super::run_limited(&["sleep", "30"], 1).contains("keine Antwort nach 1 s"));
        assert!(t.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(super::run_limited(&["echo", "hallo"], 5), "hallo\n");
    }
}
