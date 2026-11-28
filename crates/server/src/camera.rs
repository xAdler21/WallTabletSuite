//! Runs wts-camera as a child process (GStreamer or driver crashes stay out of the server) and restarts it.
//! Protocol: argv = camera config JSON and RTSP port. Stdout: log lines, "CLIENTS n", "PROBE {json}".
//! Stdin: one {"controls": {...}} per line, applied live. `wts-camera --probe <json>` prints the probe JSON.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wts_common::{Camera, RTSP_PORT};

const RETRY: Duration = Duration::from_secs(10);

pub struct Supervisor {
    exe: PathBuf,
    inner: Mutex<Inner>,
    clients: AtomicU32,
    log: Mutex<VecDeque<String>>,
}

#[derive(Default)]
struct Inner {
    /// bumped on every apply: threads of an older run see it and stop
    epoch: u64,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    config: Camera,
    probed: Option<Value>,
}

impl Supervisor {
    pub fn new() -> Supervisor {
        // WTS_CAMERA: other binary for tests; normally wts-camera next to wts-server
        let exe = std::env::var_os("WTS_CAMERA").map(PathBuf::from).unwrap_or_else(|| {
            let me = std::env::current_exe().unwrap_or_default();
            me.with_file_name(if cfg!(windows) { "wts-camera.exe" } else { "wts-camera" })
        });
        Supervisor { exe, inner: Mutex::default(), clients: AtomicU32::new(0), log: Mutex::default() }
    }

    fn say(&self, line: &str) {
        let mut log = self.log.lock().unwrap();
        if log.len() >= 80 {
            log.pop_front();
        }
        log.push_back(format!("{} {line}", crate::clock()));
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn last_log(&self) -> Option<String> {
        self.log.lock().unwrap().back().cloned()
    }

    pub fn clients(&self) -> u32 {
        self.clients.load(Ordering::Relaxed)
    }

    pub fn running(&self) -> bool {
        self.inner.lock().unwrap().child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// Stops the running camera and starts it with `c` if enabled.
    pub fn apply(self: &Arc<Self>, c: Camera) {
        let mut i = self.inner.lock().unwrap();
        i.epoch += 1;
        i.config = c;
        i.probed = None;
        if let Some(note) = stop(&mut i) {
            self.say(note);
        }
        self.clients.store(0, Ordering::Relaxed);
        if i.config.enabled {
            self.start(&mut i);
        }
    }

    /// Only the image controls changed: hands them to the running camera instead of restarting it. False if
    /// that is not possible; a removed control also needs a restart, the source keeps the last value otherwise.
    pub fn set_controls(&self, c: &Camera) -> bool {
        let mut i = self.inner.lock().unwrap();
        if !i.config.controls.keys().all(|k| c.controls.contains_key(k)) {
            return false;
        }
        let line = format!("{}\n", json!({"controls": c.controls}));
        let ok = i.stdin.as_mut().is_some_and(|s| s.write_all(line.as_bytes()).and_then(|()| s.flush()).is_ok());
        if ok {
            i.config = c.clone();
        }
        ok
    }

    /// Cameras and controls: from the running camera (it lists them before opening the camera), else from a
    /// short probe run. Never probes while the camera runs: listing controls opens the camera.
    pub fn probe(&self) -> Value {
        let config = {
            let mut i = self.inner.lock().unwrap();
            if let Some(p) = &i.probed {
                return p.clone();
            }
            if i.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) {
                return json!({"cameras": [], "controls": [], "error": "Kamera startet noch, gleich nochmal versuchen"});
            }
            i.config.clone()
        };
        match run_probe(&self.exe, &config) {
            Ok(v) => v,
            Err(e) => json!({"cameras": [], "controls": [], "error": e}),
        }
    }

    fn start(self: &Arc<Self>, i: &mut Inner) {
        let epoch = i.epoch;
        let mut cmd = Command::new(&self.exe);
        cmd.arg(serde_json::to_string(&i.config).expect("camera serializes"))
            .arg(RTSP_PORT.to_string())
            .env("GST_DEBUG", "2")
            .env("GST_DEBUG_NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = runtime_dir() {
            cmd.env("XDG_RUNTIME_DIR", dir); // PipeWire/Pulse behind ALSA "default" lives there
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.say(&format!("{}: {e}, neuer Versuch in 10 s", self.exe.display()));
                self.restart_later(epoch);
                return;
            }
        };
        i.stdin = child.stdin.take();
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        i.child = Some(child);
        let me = self.clone();
        std::thread::spawn(move || {
            for line in lines(err) {
                me.say(&line);
            }
        });
        let me = self.clone();
        std::thread::spawn(move || me.read(out, epoch));
    }

    fn read(self: Arc<Self>, out: impl Read, epoch: u64) {
        for line in lines(out) {
            if let Some(n) = line.strip_prefix("CLIENTS ") {
                if self.inner.lock().unwrap().epoch == epoch {
                    self.clients.store(n.trim().parse().unwrap_or(0), Ordering::Relaxed);
                }
            } else if let Some(p) = line.strip_prefix("PROBE ") {
                let mut i = self.inner.lock().unwrap();
                if i.epoch == epoch {
                    i.probed = serde_json::from_str(p).ok();
                }
            } else {
                self.say(&line);
            }
        }
        let status = {
            let mut i = self.inner.lock().unwrap();
            if i.epoch != epoch {
                return; // stopped on purpose
            }
            i.stdin = None;
            i.child.take().map(|mut c| c.wait())
        };
        self.clients.store(0, Ordering::Relaxed);
        let how = match status {
            Some(Ok(s)) => s.to_string(),
            Some(Err(e)) => e.to_string(),
            None => "?".into(),
        };
        self.say(&format!("Kamera-Prozess beendet ({how}), neuer Versuch in 10 s"));
        self.restart_later(epoch);
    }

    fn restart_later(self: &Arc<Self>, epoch: u64) {
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(RETRY);
            let mut i = me.inner.lock().unwrap();
            if i.epoch == epoch && i.child.is_none() && i.config.enabled {
                me.start(&mut i);
            }
        });
    }
}

/// Lines of a child's output, invalid UTF-8 replaced instead of ending the reader.
fn lines(r: impl Read) -> impl Iterator<Item = String> {
    BufReader::new(r).split(b'\n').map_while(Result::ok).map(|l| String::from_utf8_lossy(&l).trim_end().to_string())
}

/// Waits up to `secs` for the child to exit; true if it did.
fn exited(child: &mut Child, secs: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        if !matches!(child.try_wait(), Ok(None)) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM first so libcamera stops the camera cleanly (IPU3 can need a few seconds), SIGKILL after 10 s.
/// A new camera only starts once the old one is gone: two processes on the device break the next start.
/// Returns a note for the log when the camera had to be killed.
fn stop(i: &mut Inner) -> Option<&'static str> {
    i.stdin = None;
    let mut child = i.child.take()?;
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    if exited(&mut child, 10) {
        return None;
    }
    let _ = child.kill();
    if exited(&mut child, 5) {
        return Some("Kamera-Prozess reagierte 10 s nicht auf Beenden, hart beendet");
    }
    // reaped outside the lock: a process stuck in a driver call (D state) would otherwise block it for good
    std::thread::spawn(move || child.wait());
    Some("Kamera-Prozess hängt im Treiber und lässt sich nicht beenden, Tablet neu starten")
}

fn runtime_dir() -> Option<PathBuf> {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return None;
    }
    #[cfg(unix)]
    {
        let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        return dir.is_dir().then_some(dir);
    }
    #[allow(unreachable_code)]
    None
}

fn run_probe(exe: &PathBuf, c: &Camera) -> Result<Value, String> {
    let mut child = Command::new(exe)
        .arg("--probe")
        .arg(serde_json::to_string(c).expect("camera serializes"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let out = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let until = Instant::now() + Duration::from_secs(40);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Abfrage dauert zu lange (40 s)".into());
            }
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        return Err(err.lines().rev().find(|l| !l.trim().is_empty()).map_or_else(|| status.to_string(), |l| l.trim().to_string()));
    }
    serde_json::from_slice(&out).map_err(|e| format!("Antwort der Kamera-Abfrage: {e}"))
}
