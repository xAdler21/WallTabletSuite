//! Home Assistant login: runs HA's own login flow once per HA address and keeps the tokens (also across
//! restarts). The HA frontend refreshes the access token itself, so a new login is only needed when the
//! credentials change.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct Logins {
    file: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    key: String,
    tokens: BTreeMap<String, Value>,
    state: BTreeMap<String, String>,
}

enum Fail {
    /// HA said no: not retried until the credentials change, HA bans IPs after repeated failures
    Refused(String),
    /// HA unreachable or restarting
    Unreachable(String),
}

const RETRY: Duration = Duration::from_secs(30);

impl Logins {
    pub fn new(file: PathBuf) -> Logins {
        let saved: Value = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let key = saved["key"].as_str().unwrap_or_default().to_string();
        let tokens: BTreeMap<String, Value> = saved["tokens"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let state = tokens.keys().map(|b| (b.clone(), "angemeldet".to_string())).collect();
        Logins { file, inner: Mutex::new(Inner { key, tokens, state }) }
    }

    pub fn state(&self) -> Value {
        json!(self.inner.lock().unwrap().state)
    }

    /// Tokens for `base`, or None while the login runs or after HA refused it.
    pub fn get(self: &Arc<Self>, base: &str, user: &str, password: &str) -> Option<Value> {
        let key = hex(&Sha256::digest(format!("{user}\0{password}").as_bytes()));
        let mut i = self.inner.lock().unwrap();
        if key != i.key {
            *i = Inner { key: key.clone(), tokens: BTreeMap::new(), state: BTreeMap::new() };
            self.save(&i);
        }
        if let Some(t) = i.tokens.get(base) {
            return Some(t.clone());
        }
        if !i.state.contains_key(base) {
            i.state.insert(base.to_string(), "Anmeldung läuft".into());
            let (me, base, user, password) = (self.clone(), base.to_string(), user.to_string(), password.to_string());
            std::thread::spawn(move || me.login_thread(&base, &user, &password, &key));
        }
        None
    }

    fn login_thread(&self, base: &str, user: &str, password: &str, key: &str) {
        let result = login(base, user, password);
        let error = {
            let mut i = self.inner.lock().unwrap();
            if i.key != key {
                return; // credentials changed meanwhile
            }
            match result {
                Ok(tokens) => {
                    i.tokens.insert(base.into(), tokens);
                    i.state.insert(base.into(), "angemeldet".into());
                    self.save(&i);
                    return;
                }
                Err(Fail::Refused(e)) => {
                    i.state.insert(base.into(), e);
                    return;
                }
                Err(Fail::Unreachable(e)) => {
                    let e = format!("nicht erreichbar ({e}), neuer Versuch in 30 s");
                    i.state.insert(base.into(), e.clone());
                    e
                }
            }
        };
        std::thread::sleep(RETRY);
        let mut i = self.inner.lock().unwrap();
        if i.key == key && i.state.get(base) == Some(&error) {
            i.state.remove(base); // the next kiosk poll starts a new login
        }
    }

    fn save(&self, i: &Inner) {
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.file.with_extension("json.tmp");
        let text = json!({"key": i.key, "tokens": i.tokens}).to_string();
        if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &self.file)) {
            println!("{}: {e}", self.file.display());
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        // local HA with a self-signed certificate, like the kiosk accepts it
        .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
        .build()
        .into()
}

/// HA's login flow (what its login page does) -> tokens in the format the HA frontend keeps in
/// localStorage.hassTokens.
fn login(base: &str, user: &str, password: &str) -> Result<Value, Fail> {
    let agent = agent();
    let client_id = format!("{base}/"); // same client id as the frontend, so it may refresh these tokens itself
    let read = |path: &str, r: Result<ureq::http::Response<ureq::Body>, ureq::Error>| -> Result<Value, Fail> {
        let mut r = r.map_err(|e| Fail::Unreachable(e.to_string()))?;
        let code = r.status().as_u16();
        if code != 200 {
            return Err(Fail::Refused(format!("HTTP {code} auf {path}")));
        }
        r.body_mut().read_json::<Value>().map_err(|e| Fail::Unreachable(e.to_string()))
    };
    let post_json = |path: &str, body: Value| read(path, agent.post(format!("{base}{path}")).send_json(body));
    let flow = post_json("/auth/login_flow", json!({"client_id": client_id, "handler": ["homeassistant", null],
        "redirect_uri": format!("{base}/?auth_callback=1")}))?;
    let flow_id = flow["flow_id"].as_str().ok_or_else(|| Fail::Unreachable("keine flow_id".into()))?;
    let step = post_json(&format!("/auth/login_flow/{flow_id}"), json!({"client_id": client_id, "username": user, "password": password}))?;
    if step["type"] != "create_entry" {
        let why = [&step["errors"]["base"], &step["step_id"], &step["reason"], &step["type"]]
            .into_iter()
            .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
            .unwrap_or("?");
        return Err(Fail::Refused(match why {
            "invalid_auth" => "Benutzername oder Passwort falsch".into(),
            "mfa" => "Zwei-Faktor-Anmeldung aktiv, eigenen Benutzer ohne MFA anlegen".into(),
            _ => format!("Anmeldung abgelehnt ({why})"),
        }));
    }
    let code = step["result"].as_str().unwrap_or_default();
    let t = read("/auth/token", agent.post(format!("{base}/auth/token")).send_form([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &client_id),
    ]))?;
    let (Some(access), Some(refresh), Some(expires_in)) = (t["access_token"].as_str(), t["refresh_token"].as_str(), t["expires_in"].as_u64())
    else {
        return Err(Fail::Unreachable("unvollständige Antwort von /auth/token".into()));
    };
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    Ok(json!({"hassUrl": base, "clientId": client_id, "access_token": access, "refresh_token": refresh,
        "token_type": t["token_type"].as_str().unwrap_or("Bearer"), "expires_in": expires_in,
        "expires": now_ms + expires_in * 1000}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fake HA: accepts kiosk/pw, counts login attempts.
    fn fake_ha() -> (String, Arc<AtomicU32>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", server.server_addr().to_ip().unwrap().port());
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let url = req.url().to_string();
                let reply = if url == "/auth/login_flow" {
                    json!({"flow_id": "f1", "type": "form", "step_id": "init"}).to_string()
                } else if url == "/auth/login_flow/f1" {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let v: Value = serde_json::from_str(&body).unwrap();
                    if v["username"] == "kiosk" && v["password"] == "pw" {
                        json!({"type": "create_entry", "result": "code1"}).to_string()
                    } else {
                        json!({"type": "form", "errors": {"base": "invalid_auth"}}).to_string()
                    }
                } else if url == "/auth/token" && body.contains("code=code1") && body.contains("grant_type=authorization_code") {
                    json!({"access_token": "A", "refresh_token": "R", "token_type": "Bearer", "expires_in": 1800}).to_string()
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404));
                    continue;
                };
                let _ = req.respond(tiny_http::Response::from_string(reply));
            }
        });
        (base, attempts)
    }

    fn wait_for(l: &Arc<Logins>, base: &str, user: &str, pw: &str, until: impl Fn(&Value) -> bool) -> Option<Value> {
        for _ in 0..100 {
            let t = l.get(base, user, pw);
            if until(&l.state()) {
                return t.or_else(|| l.get(base, user, pw));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timeout, state {}", l.state());
    }

    #[test]
    fn login_keep_refuse() {
        let (base, attempts) = fake_ha();
        let dir = std::env::temp_dir().join(format!("wts-ha-{}", std::process::id()));
        let file = dir.join("ha_tokens.json");
        let l = Arc::new(Logins::new(file.clone()));
        let t = wait_for(&l, &base, "kiosk", "pw", |s| s[&base] == "angemeldet").unwrap();
        assert_eq!((t["access_token"].as_str(), t["clientId"].as_str()), (Some("A"), Some(format!("{base}/").as_str())));
        assert_eq!(t["hassUrl"], base.as_str());
        // kept across restarts, no new login
        let l = Arc::new(Logins::new(file.clone()));
        assert_eq!(l.get(&base, "kiosk", "pw").unwrap()["refresh_token"], "R");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        // wrong password: refused once, not retried
        wait_for(&l, &base, "kiosk", "nope", |s| s[&base] == "Benutzername oder Passwort falsch");
        for _ in 0..3 {
            assert!(l.get(&base, "kiosk", "nope").is_none());
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        // unreachable HA: state says so
        let dead = "http://127.0.0.1:1";
        wait_for(&l, dead, "kiosk", "nope", |s| s[dead].as_str().is_some_and(|x| x.starts_with("nicht erreichbar")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
