//! HTTP-level tests of the server: lock, status contract, tabs/kiosk/health, validation, ONVIF over HTTP,
//! HA login through /api/kiosk, update check, camera supervisor (with wts-fake-camera). No GTK/GStreamer.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use serde_json::{Value, json};

struct Srv {
    base: String,
    data: PathBuf,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn start(name: &str) -> Srv {
    static ENV: Once = Once::new();
    // SAFETY: set once before any server (the only reader) exists
    ENV.call_once(|| unsafe { std::env::set_var("WTS_CAMERA", env!("CARGO_BIN_EXE_wts-fake-camera")) });
    let data = std::env::temp_dir().join(format!("wts-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let st = wts_server::State::new(data.clone(), 0);
    let port = wts_server::serve(&st, 0).unwrap();
    Srv { base: format!("http://127.0.0.1:{port}"), data }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into()
}

impl Srv {
    fn raw(&self, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        let url = format!("{}{path}", self.base);
        let mut r = match body {
            Some(b) => agent().post(&url).send(b).unwrap(),
            None => agent().get(&url).call().unwrap(),
        };
        (r.status().as_u16(), r.body_mut().with_config().limit(10_000_000).read_to_vec().unwrap())
    }

    fn get(&self, path: &str) -> (u16, Value) {
        let (code, b) = self.raw(path, None);
        (code, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let (code, b) = self.raw(path, Some(body.to_string().as_bytes()));
        (code, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn config(&self) -> Value {
        self.get("/api/config").1
    }

    /// Polls `f` on /api/config until it holds (max 10 s).
    fn wait(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            let c = self.config();
            if f(&c) {
                return c;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{what}: {}", self.config());
    }
}

#[test]
fn lock_get_post_and_persist() {
    let s = start("lock");
    assert_eq!(s.get("/api/lock"), (200, json!({"locked": true})));
    assert_eq!(s.get("/api/status").1["locked"], true);
    let saved: Value = serde_json::from_slice(&std::fs::read(s.data.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["locked"], true);
    assert_eq!(s.get("/api/unlock"), (200, json!({"locked": false})));
    assert_eq!(s.post("/api/lock", json!({"locked": true})), (200, json!({"locked": true})));
    assert_eq!(s.post("/api/lock", json!({"locked": true})), (200, json!({"locked": true}))); // idempotent
    assert_eq!(s.post("/api/lock", json!({"locked": false})), (200, json!({"locked": false})));
    assert_eq!(s.post("/api/lock", json!({"locked": "ja"})).0, 400);
    assert_eq!(s.raw("/api/lock", Some(b"{kaputt")).0, 400);
    assert_eq!(s.post("/api/unlock", json!({})), (200, json!({"locked": false})));
    // survives a restart: a new server on the same data dir
    s.get("/api/lock");
    let st = wts_server::State::new(s.data.clone(), 0);
    assert!(st.config().locked);
}

#[test]
fn status_contract_panel_logs_404() {
    let s = start("status");
    let st = s.get("/api/status").1;
    assert_eq!(st["version"], wts_common::version());
    assert_eq!(st["locked"], false);
    let devices = st["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    for d in devices {
        assert!(["ok", "warn", "error", "offline"].contains(&d["state"].as_str().unwrap()), "{d}");
        assert!(d["name"].is_string() && d["kind"].is_string() && d["detail"].is_string());
    }
    assert_eq!(devices[0]["state"], "offline"); // no kiosk polled yet
    let (code, panel) = s.raw("/", None);
    assert_eq!(code, 200);
    assert!(String::from_utf8(panel).unwrap().contains("WallTabletSuite"));
    assert_eq!(s.get("/api/logs?unit=sshd").0, 400);
    assert!(s.get("/api/logs?unit=wts-update").1["lines"].is_array());
    assert_eq!(s.get("/gibtsnicht").0, 404);
    assert_eq!(s.get("/snapshot.jpg").0, 503); // no camera running
    // camera diagnosis: every section is there, also where the tools are missing (Windows, CI)
    let diag = s.get("/api/camera/diag").1["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect::<Vec<_>>();
    for section in ["== System", "== Video-Geräte", "== ACPI (Sensoren, IPU)", "== Kernel-Module", "== Kernel-Log", "== libcamera", "== cam -l", "== Kamera-Log"] {
        assert!(diag.iter().any(|l| l == section), "{section} missing in {diag:?}");
    }
}

#[test]
fn tabs_kiosk_health() {
    let s = start("kiosk");
    assert_eq!(s.post("/api/config", json!({"tabs": [{"name": "HA", "url": "javascript:alert(1)"}]})).0, 400);
    assert_eq!(s.post("/api/config", json!({"tabs": [{"name": "", "url": "http://x"}]})).0, 400);
    assert_eq!(s.post("/api/config", json!({"tabs": [1]})).0, 400);
    assert_eq!(s.post("/api/config", json!([1])).0, 400);
    let tabs = json!([{"name": "HA", "url": "http://ha:8123/", "ha": false}, {"name": "Beamer", "url": "https://vm:8445/", "ha": false}]);
    assert_eq!(s.post("/api/config", json!({"tabs": [{"name": "HA", "url": "http://ha:8123/"}, tabs[1]]})).0, 200);
    assert_eq!(s.get("/api/health").0, 503);
    let k = s.get("/api/kiosk?tab=HA&up=3").1;
    assert_eq!(k["tabs"], tabs);
    assert_eq!((k["locked"].clone(), k["rotation"].clone()), (json!(false), json!(0)));
    assert_eq!(s.get("/api/health").0, 503); // kiosk runs only 3 s
    s.get("/api/kiosk?tab=HA&up=25");
    assert_eq!(s.get("/api/health").0, 200);
    let st = s.get("/api/status").1;
    assert_eq!((st["devices"][0]["state"].as_str(), st["devices"][0]["detail"].as_str()), (Some("ok"), Some("Tab HA")));
    s.get("/api/kiosk?up=400&safe=1&error=WebKit%20weg");
    assert_eq!(s.get("/api/health").0, 503);
    let c = s.config();
    assert_eq!((c["kiosk"]["safe"].clone(), c["kiosk"]["error"].clone()), (json!(true), json!("WebKit weg")));
    assert_eq!(s.get("/api/status").1["devices"][0]["state"], "error");
}

#[test]
fn camera_update_rotation_validation() {
    let s = start("valid");
    for bad in [
        json!({"camera": {"fps": 500}}), json!({"camera": {"device": "/etc/passwd"}}), json!({"camera": {"bogus": 1}}),
        json!({"camera": {"rotate": "45"}}), json!({"camera": {"audio_device": "hw:0 ! fakesink"}}),
        json!({"camera": {"controls": {"Bad Name": 1}}}), json!({"camera": 5}),
        json!({"update": {"repo": "--upload-pack=x"}}), json!({"update": {"branch": "-x"}}),
        json!({"rotation": 45}), json!({"ha": {"theme": "lila"}}),
    ] {
        assert_eq!(s.post("/api/config", bad.clone()).0, 400, "{bad}");
    }
    let ok = json!({"fps": "20", "name": "Flur", "rotate": "270", "profile": "high", "gop": 1, "camera_id": "\\_SB_.PCI0.LNK1",
                    "controls": {"brightness": 0.2, "ae-enable": false, "awb-mode": 3}});
    assert_eq!(s.post("/api/config", json!({"camera": ok})).0, 200);
    let c = s.config()["config"]["camera"].clone();
    assert_eq!((c["fps"].clone(), c["name"].clone(), c["rotate"].clone()), (json!(20), json!("Flur"), json!("270")));
    assert_eq!(c["controls"], ok["controls"]);
    assert_eq!(s.post("/api/config", json!({"rotation": 90})).0, 200);
    assert_eq!(s.get("/api/kiosk").1["rotation"], 90);
}

fn soap(s: &Srv, svc: &str, action: &str, inner: &str) -> (u16, String) {
    let body = format!("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><{action} xmlns=\"http://www.onvif.org/ver10/{svc}/wsdl\">{inner}</{action}></s:Body></s:Envelope>");
    let (code, b) = s.raw(&format!("/onvif/{svc}_service"), Some(body.as_bytes()));
    (code, String::from_utf8(b).unwrap())
}

fn texts(xml: &str, name: &str) -> Vec<String> {
    let doc = roxmltree::Document::parse(xml).unwrap();
    doc.descendants().filter(|n| n.tag_name().name() == name).filter_map(|n| n.text().map(str::to_string)).collect()
}

#[test]
fn onvif_over_http() {
    let s = start("onvif");
    assert_eq!(soap(&s, "media", "GetProfiles", "").0, 503); // camera off
    // enabled with a name that makes the fake camera exit: ONVIF answers from the config regardless
    assert_eq!(s.post("/api/config", json!({"camera": {"enabled": true, "name": "crash", "rotate": "90", "audio_enabled": true}})).0, 200);
    let port = s.base.rsplit(':').next().unwrap();
    let (code, xml) = soap(&s, "media", "GetStreamUri", "<ProfileToken>main</ProfileToken>");
    assert_eq!(code, 200);
    assert_eq!(texts(&xml, "Uri"), ["rtsp://127.0.0.1:8554/stream"]);
    assert_eq!(texts(&soap(&s, "media", "GetSnapshotUri", "").1, "Uri"), [format!("http://127.0.0.1:{port}/snapshot.jpg")]);
    let (_, xml) = soap(&s, "device", "GetCapabilities", "");
    assert_eq!(texts(&xml, "XAddr")[0], format!("http://127.0.0.1:{port}/onvif/device_service"));
    let (_, xml) = soap(&s, "media", "GetProfiles", "");
    assert_eq!(texts(&xml, "Width")[0], "720");
    assert_eq!(texts(&xml, "Encoding").iter().filter(|e| *e == "AAC").count(), 1);
    let (code, xml) = soap(&s, "device", "SetHostname", "");
    assert_eq!(code, 500);
    assert!(xml.contains("ActionNotSupported"));
    assert_eq!(s.raw("/onvif/device_service", Some(b"not xml")).0, 400);
    assert_eq!(s.post("/api/config", json!({"camera": {"enabled": false}})).0, 200);
}

#[test]
fn ha_login_through_kiosk() {
    let s = start("ha");
    let ha = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", ha.server_addr().to_ip().unwrap().port());
    let (tx, token_requests) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for mut req in ha.incoming_requests() {
            let mut body = String::new();
            std::io::Read::read_to_string(req.as_reader(), &mut body).unwrap();
            let out = match req.url() {
                "/auth/login_flow" => json!({"flow_id": "f1", "type": "form"}),
                "/auth/login_flow/f1" if serde_json::from_str::<Value>(&body).unwrap()["password"] == "geheim" => {
                    json!({"type": "create_entry", "result": "code1"})
                }
                "/auth/login_flow/f1" => json!({"type": "form", "errors": {"base": "invalid_auth"}}),
                _ => {
                    tx.send(body).unwrap();
                    json!({"access_token": "a", "refresh_token": "r", "token_type": "Bearer", "expires_in": 1800})
                }
            };
            let _ = req.respond(tiny_http::Response::from_string(out.to_string()));
        }
    });
    s.post("/api/config", json!({"tabs": [{"name": "HA", "url": format!("{base}/lovelace/0"), "ha": true},
                                          {"name": "Beamer", "url": "http://vm:8445/"}],
                                 "ha": {"username": "kiosk", "password": "falsch", "theme": "dark"}}));
    let c = s.config();
    assert!(c["config"]["ha"].get("password").is_none()); // never sent out
    assert_eq!(c["ha"]["has_password"], true);
    let state = |s: &Srv| s.config()["ha"]["state"][&base].as_str().unwrap_or_default().to_string();
    let poll = |s: &Srv| -> Value {
        for _ in 0..200 {
            s.get("/api/kiosk"); // a kiosk poll starts the login
            if !["", "Anmeldung läuft"].contains(&state(s).as_str()) {
                return s.get("/api/kiosk").1["tabs"].clone();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("login hangs");
    };
    let tabs = poll(&s);
    assert_eq!(state(&s), "Benutzername oder Passwort falsch");
    assert!(tabs[0].get("ha_tokens").is_none());
    assert_eq!(tabs[0]["ha_theme"], "dark");
    assert!(tabs[1].get("ha_theme").is_none());
    s.post("/api/config", json!({"ha": {"username": "kiosk", "password": "geheim"}}));
    let tabs = poll(&s);
    assert_eq!(state(&s), "angemeldet");
    assert_eq!(tabs[0]["ha_tokens"]["refresh_token"], "r");
    assert_eq!(tabs[0]["ha_tokens"]["hassUrl"], base.as_str());
    assert_eq!(tabs[0]["ha_tokens"]["clientId"], format!("{base}/"));
    assert!(tabs[1].get("ha_tokens").is_none());
    assert!(token_requests.recv_timeout(Duration::from_secs(1)).unwrap().contains("code=code1"));
    // empty password field keeps password and login; changing the theme too
    s.post("/api/config", json!({"ha": {"username": "kiosk", "password": ""}}));
    s.post("/api/config", json!({"ha": {"theme": "light"}}));
    let tabs = s.get("/api/kiosk").1["tabs"].clone();
    assert_eq!((tabs[0]["ha_tokens"]["refresh_token"].as_str(), tabs[0]["ha_theme"].as_str()), (Some("r"), Some("light")));
    assert!(token_requests.try_recv().is_err()); // no second login
    s.post("/api/config", json!({"ha": {"username": ""}}));
    assert!(s.get("/api/kiosk").1["tabs"][0].get("ha_tokens").is_none());
}

#[test]
fn update_check_against_latest_json() {
    let s = start("update");
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", srv.server_addr().to_ip().unwrap().port());
    std::thread::spawn(move || {
        for req in srv.incoming_requests() {
            let body = match req.url() {
                "/new/latest.json" => json!({"version": "99.0.0", "file": "walltablet.tar.gz", "sha256": "a".repeat(64)}),
                "/old/latest.json" => json!({"version": "0.0.1", "file": "walltablet.tar.gz", "sha256": "a".repeat(64)}),
                _ => json!({"version": "1.0; reboot", "file": "x", "sha256": "a".repeat(64)}),
            };
            let _ = req.respond(tiny_http::Response::from_string(body.to_string()));
        }
    });
    let check = |path: &str| {
        assert_eq!(s.post("/api/config", json!({"update": {"repo": format!("{base}{path}")}})).0, 200);
        s.post("/api/update/check", json!({})).1
    };
    let v = check("/new/latest.json");
    assert_eq!((v["available"].clone(), v["remote_version"].clone(), v["error"].clone()), (json!(true), json!("99.0.0"), Value::Null));
    assert!(v["checked"].is_u64());
    assert_eq!(check("/old/latest.json")["available"], false);
    assert!(check("/bad/latest.json")["error"].as_str().unwrap().contains("Version"));
    assert_eq!(s.post("/api/config", json!({"update": {"repo": "http://127.0.0.1:1/latest.json"}})).0, 200);
    assert!(s.post("/api/update/check", json!({})).1["error"].as_str().unwrap().contains("127.0.0.1:1"));
    check("/old/latest.json");
    let (code, e) = s.post("/api/update/install", json!({}));
    assert_eq!(code, 400);
    assert!(e["error"].as_str().unwrap().contains("Version"), "{e}");
    assert_eq!(s.config()["update"]["available"], false);
}

#[test]
fn camera_supervisor() {
    let s = start("camera");
    // off: probe runs the camera binary once
    let p = s.get("/api/camera/probe").1;
    assert_eq!(p["cameras"][0]["id"], "fake0");
    assert_eq!(s.post("/api/config", json!({"camera": {"source": "custom", "custom_source": "probe-fail"}})).0, 200);
    assert_eq!(s.get("/api/camera/probe").1["error"], "Kamera kaputt");
    assert_eq!(s.post("/api/config", json!({"camera": {"source": "libcamera", "enabled": true, "controls": {"brightness": 0.5}}})).0, 200);
    let c = s.wait("camera runs", |c| c["camera"]["running"] == true && c["camera"]["clients"] == 2);
    let log = |c: &Value| c["camera"]["log"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(log(&c).iter().any(|l| l.ends_with("Kamera liefert Bilder")));
    // the fake writes no snapshot: process alive but no frames is a warning, not ok
    let cam = s.get("/api/status").1["devices"][1].clone();
    assert_eq!((cam["state"].as_str(), cam["detail"].as_str()), (Some("warn"), Some("keine Bilder")));
    // probe while running comes from the camera's own PROBE line, no second process
    s.wait("probe line", |_| s.get("/api/camera/probe").1["controls"][0]["name"] == "brightness");
    let starts = |c: &Value| log(c).iter().filter(|l| l.contains(" gestartet ")).count();
    s.wait("stderr in log", |c| log(c).iter().any(|l| l.ends_with("stderr üÿ")));
    // changed/added control: live over stdin, no restart
    assert_eq!(s.post("/api/config", json!({"camera": {"controls": {"brightness": 0.7, "contrast": 2}}})).0, 200);
    let c = s.wait("control live", |c| log(c).iter().any(|l| l.contains("SET ") && l.contains("0.7")));
    assert_eq!(starts(&c), 1);
    // removed control: restart, the driver default applies again
    assert_eq!(s.post("/api/config", json!({"camera": {"controls": {"contrast": 2}}})).0, 200);
    s.wait("restart after reset", |c| starts(c) == 2 && c["camera"]["running"] == true);
    // crash: reported, restart pending
    assert_eq!(s.post("/api/config", json!({"camera": {"name": "crash"}})).0, 200);
    let c = s.wait("crash noticed", |c| log(c).iter().any(|l| l.contains("Kamera-Prozess beendet")));
    assert_eq!(c["camera"]["clients"], 0);
    assert_eq!(s.get("/api/status").1["devices"][1]["state"], "error");
    assert_eq!(s.post("/api/config", json!({"camera": {"enabled": false, "name": "Flur"}})).0, 200);
    let c = s.wait("stopped", |c| c["camera"]["running"] == false);
    assert_eq!(s.get("/api/status").1["devices"][1]["detail"], "aus");
    std::thread::sleep(Duration::from_secs(11)); // the pending restart of the crashed run must not fire
    assert_eq!(starts(&s.config()), starts(&c));
    assert_eq!(s.config()["camera"]["running"], false);
}

#[cfg(unix)]
#[test]
fn camera_stop_kills_a_stuck_process() {
    let s = start("stuck");
    assert_eq!(s.post("/api/config", json!({"camera": {"enabled": true, "name": "stuck"}})).0, 200);
    s.wait("camera runs", |c| c["camera"]["running"] == true && c["camera"]["clients"] == 2);
    let t = std::time::Instant::now();
    // ignores SIGTERM: killed after the grace time, and gone before the call returns
    assert_eq!(s.post("/api/config", json!({"camera": {"enabled": false}})).0, 200);
    assert!(t.elapsed() >= Duration::from_secs(9), "{:?}", t.elapsed());
    let c = s.config();
    assert_eq!(c["camera"]["running"], false);
    assert!(c["camera"]["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().ends_with("hart beendet")), "{c}");
}
