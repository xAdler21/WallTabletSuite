//! Self-update from release packages: release/latest.json in the repo names the version, the tarball and its
//! SHA-256. The server checks it, deploy/update.sh (own systemd unit, outlives the server restart) installs it,
//! checks health and rolls back.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use wts_common::{Update, newer, version};

const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

pub struct Updater {
    log: PathBuf,
    env: PathBuf,
    info: Mutex<Info>,
}

#[derive(Default)]
struct Info {
    checked: Option<u64>,
    remote_version: Option<String>,
    available: bool,
    error: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

impl Updater {
    pub fn new(log: PathBuf, env: PathBuf) -> Updater {
        Updater { log, env, info: Mutex::new(Info::default()) }
    }

    /// Version update.sh rolled back from; auto-update skips it until a newer one appears.
    fn failed(&self) -> Option<String> {
        let f = std::fs::read_to_string(self.env.with_file_name("update.failed")).ok()?;
        Some(f.trim().to_string()).filter(|v| !v.is_empty())
    }

    pub fn view(&self) -> Value {
        let i = self.info.lock().unwrap();
        let log = std::fs::read(&self.log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        json!({"checked": i.checked, "remote_version": i.remote_version, "available": i.available, "error": i.error,
               "failed": self.failed(), "log": lines[lines.len().saturating_sub(40)..]})
    }

    pub fn check(&self, u: &Update) -> Value {
        let r = fetch(u);
        {
            let mut i = self.info.lock().unwrap();
            i.checked = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
            match &r {
                Ok(rel) => {
                    i.remote_version = Some(rel.version.clone());
                    i.available = newer(&rel.version, version());
                    i.error = None;
                }
                Err(e) => i.error = Some(e.clone()),
            }
        }
        self.view()
    }

    /// Hands the newest release to update.sh. Checks again first: install acts on what the repo has now.
    pub fn install(&self, u: &Update) -> Result<(), String> {
        let rel = fetch(u)?;
        if !newer(&rel.version, version()) {
            return Err(format!("Keine neuere Version (Repo hat {}, installiert ist {})", rel.version, version()));
        }
        // systemd merges a start into the running job: the request would be lost while we answer ok
        let running = std::process::Command::new("systemctl")
            .args(["is-active", "wts-update.service"])
            .output()
            .is_ok_and(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "active" | "activating"));
        if running {
            return Err("Ein Update läuft gerade, bitte warten, bis es fertig ist".into());
        }
        self.write_env(&rel)?;
        // update.sh runs in its own unit: it restarts this server and must outlive it
        let out = std::process::Command::new("sudo")
            .args(["-n", "/usr/bin/systemctl", "start", "--no-block", "wts-update.service"])
            .output()
            .map_err(|e| format!("sudo: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if err.is_empty() { "systemctl start wts-update.service fehlgeschlagen".into() } else { err });
        }
        Ok(())
    }

    /// update.env: plain KEY=value lines that update.sh reads with `read`, never `source`. Values are checked
    /// in fetch(), so no quoting is needed.
    fn write_env(&self, r: &Release) -> Result<(), String> {
        let tmp = self.env.with_extension("env.tmp");
        std::fs::write(&tmp, format!("VERSION={}\nURL={}\nSHA256={}\n", r.version, r.url, r.sha256))
            .and_then(|()| std::fs::rename(&tmp, &self.env))
            .map_err(|e| format!("update.env: {e}"))
    }

    pub fn run_loop(&self, st: &crate::State) {
        std::thread::sleep(Duration::from_secs(120));
        loop {
            let u = st.config().update;
            self.check(&u);
            let (available, remote) = {
                let i = self.info.lock().unwrap();
                (i.available && i.error.is_none(), i.remote_version.clone())
            };
            if u.auto && available && remote != self.failed()
                && let Err(e) = self.install(&u) {
                    self.info.lock().unwrap().error = Some(e);
                }
            std::thread::sleep(CHECK_EVERY);
        }
    }
}

/// Where latest.json lives: GitHub repo links map to raw.githubusercontent.com, a link ending in .json is
/// used as is (own mirror, tests).
pub fn latest_url(u: &Update) -> Result<String, String> {
    if u.repo.ends_with(".json") {
        return Ok(u.repo.clone());
    }
    let parts: Vec<&str> = u
        .repo
        .strip_prefix("https://github.com/")
        .map(|r| r.trim_end_matches('/').trim_end_matches(".git").split('/').collect())
        .unwrap_or_default();
    match parts[..] {
        [owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            Ok(format!("https://raw.githubusercontent.com/{owner}/{repo}/{}/release/latest.json", u.branch))
        }
        _ => Err("Repo-Link: https://github.com/<besitzer>/<repo> oder Link auf eine latest.json".into()),
    }
}

/// Validates latest.json; the tarball sits next to it.
pub fn parse_latest(url: &str, latest: &Value) -> Result<Release, String> {
    let field = |k: &str| latest[k].as_str().map(str::trim).unwrap_or_default().to_string();
    let (ver, file, sha) = (field("version"), field("file"), field("sha256").to_ascii_lowercase());
    if ver.is_empty() || ver.len() > 32 || !ver.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)) || !ver.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!("latest.json: ungültige Version {ver:?}"));
    }
    if file.is_empty() || file.len() > 100 || !file.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) || file.starts_with('.') {
        return Err(format!("latest.json: ungültiger Dateiname {file:?}"));
    }
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("latest.json: sha256 fehlt oder ist ungültig".into());
    }
    let dir = url.rsplit_once('/').map_or(url, |d| d.0);
    let url = format!("{dir}/{file}");
    if url.chars().any(|c| c.is_whitespace() || c.is_control() || "\"'`$\\".contains(c)) {
        return Err("latest.json: ungültige Adresse".into());
    }
    Ok(Release { version: ver, url, sha256: sha })
}

fn fetch(u: &Update) -> Result<Release, String> {
    let url = latest_url(u)?;
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into();
    let latest: Value = agent
        .get(&url)
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("{url}: {e}"))?;
    parse_latest(&url, &latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(repo: &str) -> Update {
        Update { repo: repo.into(), branch: "main".into(), auto: false }
    }

    #[test]
    fn urls() {
        let raw = "https://raw.githubusercontent.com/xAdler21/WallTabletSuite/main/release/latest.json";
        assert_eq!(latest_url(&up("https://github.com/xAdler21/WallTabletSuite.git")).unwrap(), raw);
        assert_eq!(latest_url(&up("https://github.com/xAdler21/WallTabletSuite/")).unwrap(), raw);
        assert_eq!(latest_url(&up("http://10.0.0.2:8000/latest.json")).unwrap(), "http://10.0.0.2:8000/latest.json");
        assert!(latest_url(&up("https://gitlab.com/a/b")).is_err());
        assert!(latest_url(&up("https://github.com/a")).is_err());
        assert!(latest_url(&up("https://github.com/a/b/c")).is_err());
    }

    #[test]
    fn latest_json() {
        let sha = "ab".repeat(32);
        let url = "https://h/x/release/latest.json";
        let r = parse_latest(url, &json!({"version": "0.3.0", "file": "walltablet.tar.gz", "sha256": sha.to_uppercase()})).unwrap();
        assert_eq!(r, Release { version: "0.3.0".into(), url: "https://h/x/release/walltablet.tar.gz".into(), sha256: sha.clone() });
        for bad in [
            json!({"version": "0.3.0; rm -rf /", "file": "a.tar.gz", "sha256": sha}),
            json!({"version": "0.3.0", "file": "../a.tar.gz", "sha256": sha}),
            json!({"version": "0.3.0", "file": "a b", "sha256": sha}),
            json!({"version": "0.3.0", "file": "a.tar.gz", "sha256": "xyz"}),
            json!({"version": "", "file": "a.tar.gz", "sha256": sha}),
            json!({"file": "a.tar.gz"}),
        ] {
            assert!(parse_latest(url, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_and_failed() {
        let dir = std::env::temp_dir().join(format!("wts-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let u = Updater::new(dir.join("update.log"), dir.join("update.env"));
        let rel = Release { version: "1.2.3".into(), url: "https://h/a.tar.gz".into(), sha256: "0".repeat(64) };
        u.write_env(&rel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("update.env")).unwrap(), format!("VERSION=1.2.3\nURL=https://h/a.tar.gz\nSHA256={}\n", "0".repeat(64)));
        assert_eq!(u.view()["failed"], Value::Null);
        std::fs::write(dir.join("update.failed"), "1.2.3\n").unwrap();
        std::fs::write(dir.join("update.log"), (0..50).map(|n| format!("z{n}\n")).collect::<String>()).unwrap();
        let v = u.view();
        assert_eq!(v["failed"], "1.2.3");
        assert_eq!(v["log"].as_array().unwrap().len(), 40);
        assert_eq!(v["log"][39], "z49");
        let _ = std::fs::remove_dir_all(dir);
    }
}
