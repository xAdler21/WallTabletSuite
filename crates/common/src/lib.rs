//! Shared by server, kiosk and camera: the configuration (same JSON as config.json), its validation,
//! stream sizes and a few constants.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PORT: u16 = 8447;
pub const RTSP_PORT: u16 = 8554;
/// Written by the camera once a second, served by the server as /snapshot.jpg.
pub const SNAPSHOT: &str = "/dev/shm/walltablet-snapshot.jpg";
pub const AUDIO_RATE: u32 = 16000;
pub const H264_PROFILES: [(&str, &str); 3] = [("main", "Main"), ("high", "High"), ("constrained-baseline", "Baseline")];
pub const ROTATIONS: [u16; 4] = [0, 90, 180, 270];

pub fn version() -> &'static str {
    include_str!("../../../VERSION").trim()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    pub locked: bool,
    pub rotation: u16,
    pub tabs: Vec<Tab>,
    pub ha: Ha,
    pub camera: Camera,
    pub update: Update,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Tab {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub ha: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ha {
    pub username: String,
    pub password: String,
    pub theme: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Camera {
    pub enabled: bool,
    pub name: String,
    pub source: String,
    pub camera_id: String,
    pub device: String,
    pub custom_source: String,
    pub rotate: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub encoder: String,
    pub profile: String,
    pub gop: u32,
    pub sub_enabled: bool,
    pub sub_width: u32,
    pub sub_height: u32,
    pub sub_bitrate: u32,
    pub audio_enabled: bool,
    pub audio_device: String,
    pub controls: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Update {
    pub repo: String,
    pub branch: String,
    pub auto: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            locked: false,
            rotation: 0,
            tabs: vec![Tab { name: "Home Assistant".into(), url: "http://homeassistant.local:8123/".into(), ha: true }],
            ha: Ha { username: String::new(), password: String::new(), theme: "auto".into() },
            camera: Camera::default(),
            update: Update {
                repo: "https://github.com/xAdler21/WallTabletSuite.git".into(),
                branch: "main".into(),
                auto: false,
            },
        }
    }
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            enabled: false,
            name: "Wall Tablet".into(),
            source: "libcamera".into(),
            camera_id: String::new(),
            device: "/dev/video0".into(),
            custom_source: String::new(),
            rotate: "none".into(),
            width: 1280,
            height: 720,
            fps: 15,
            bitrate: 2000,
            encoder: "va".into(),
            profile: "main".into(),
            gop: 2,
            sub_enabled: true,
            sub_width: 640,
            sub_height: 360,
            sub_bitrate: 500,
            audio_enabled: false,
            audio_device: "default".into(),
            controls: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Reads a saved config leniently: every section is validated on its own, a broken section falls back
    /// to its default instead of losing the whole file (configs written by older versions included).
    pub fn from_json(text: &str) -> Config {
        let mut c = Config::default();
        let Ok(Value::Object(saved)) = serde_json::from_str::<Value>(text) else { return c };
        if let Some(v) = saved.get("locked") {
            c.locked = truthy(v);
        }
        if let Some(r) = saved.get("rotation").and_then(rotation) {
            c.rotation = r;
        }
        if let Some(Ok(tabs)) = saved.get("tabs").map(clean_tabs) {
            c.tabs = tabs;
        }
        if let Some(Value::Object(ha)) = saved.get("ha") {
            let user = text_of(ha.get("username")).trim().chars().take(200).collect::<String>();
            if !user.is_empty() {
                c.ha.username = user;
                c.ha.password = text_of(ha.get("password")).chars().take(200).collect();
            }
            if let Some(Value::String(t)) = ha.get("theme").filter(|t| matches!(t.as_str(), Some("auto" | "light" | "dark"))) {
                c.ha.theme = t.clone();
            }
        }
        if let Some(Value::Object(cam)) = saved.get("camera") {
            // keys a newer version wrote are dropped, keys an older one lacked keep their defaults
            let known: Map<String, Value> = cam.iter().filter(|(k, _)| camera_keys().contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
            if let Ok(cam) = clean_camera(&known, &Camera::default()) {
                c.camera = cam;
            }
        }
        if let Some(Value::Object(up)) = saved.get("update")
            && let Ok(up) = clean_update(up, &c.update) {
                c.update = up;
            }
        c
    }

    /// Applies a partial update from POST /api/config. Unknown top-level keys are ignored.
    pub fn apply(&self, data: &Map<String, Value>) -> Result<Config, String> {
        let mut c = self.clone();
        if let Some(tabs) = data.get("tabs") {
            c.tabs = clean_tabs(tabs)?;
        }
        if let Some(r) = data.get("rotation") {
            c.rotation = rotation(r).ok_or("rotation: 0, 90, 180 oder 270")?;
        }
        if let Some(cam) = data.get("camera") {
            c.camera = clean_camera(cam.as_object().ok_or("camera muss ein Objekt sein")?, &self.camera)?;
        }
        if let Some(up) = data.get("update") {
            c.update = clean_update(up.as_object().ok_or("update muss ein Objekt sein")?, &self.update)?;
        }
        if let Some(ha) = data.get("ha") {
            c.ha = clean_ha(ha.as_object().ok_or("ha muss ein Objekt sein")?, &self.ha)?;
        }
        Ok(c)
    }
}

fn camera_keys() -> Vec<&'static str> {
    vec![
        "enabled", "name", "source", "camera_id", "device", "custom_source", "rotate", "width", "height", "fps", "bitrate",
        "encoder", "profile", "gop", "sub_enabled", "sub_width", "sub_height", "sub_bitrate", "audio_enabled",
        "audio_device", "controls",
    ]
}

fn rotation(v: &Value) -> Option<u16> {
    let r = v.as_u64()?;
    ROTATIONS.iter().copied().find(|&x| u64::from(x) == r)
}

/// JSON truthiness like Python's bool(): false, 0, "", null, [] and {} are false.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A JSON value as text, like Python's str() for strings and numbers.
fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn is_http_url(url: &str) -> bool {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty()) && !url.chars().any(char::is_whitespace)
}

pub fn clean_tabs(tabs: &Value) -> Result<Vec<Tab>, String> {
    let tabs = tabs.as_array().filter(|t| t.len() <= 20).ok_or("tabs: Liste mit höchstens 20 Einträgen")?;
    tabs.iter()
        .map(|t| {
            let t = t.as_object().ok_or("tabs: Einträge brauchen name und url")?;
            let name: String = text_of(t.get("name")).trim().chars().take(40).collect();
            let url = text_of(t.get("url")).trim().to_string();
            if name.is_empty() {
                return Err("Ein Tab hat keinen Namen".to_string());
            }
            if !is_http_url(&url) {
                return Err(format!("{name}: Adresse muss mit http:// oder https:// anfangen"));
            }
            Ok(Tab { name, url, ha: t.get("ha").is_some_and(truthy) })
        })
        .collect()
}

pub fn clean_ha(new: &Map<String, Value>, old: &Ha) -> Result<Ha, String> {
    let mut c = old.clone();
    if new.contains_key("username") {
        let user: String = text_of(new.get("username")).trim().chars().take(200).collect();
        let password: String = text_of(new.get("password")).chars().take(200).collect();
        // empty user clears the login, empty password field keeps the stored password
        (c.username, c.password) = if user.is_empty() {
            (String::new(), String::new())
        } else {
            let password = if password.is_empty() { old.password.clone() } else { password };
            (user, password)
        };
    }
    if let Some(theme) = new.get("theme") {
        match theme.as_str() {
            Some(t @ ("auto" | "light" | "dark")) => c.theme = t.to_string(),
            _ => return Err("ha.theme: auto, light oder dark".into()),
        }
    }
    Ok(c)
}

fn int_field(obj: &Map<String, Value>, key: &str, lo: u32, hi: u32) -> Result<u32, String> {
    let n = match obj.get(key) {
        Some(Value::Number(n)) => n.as_f64().map(|f| f.trunc() as i64),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
    .ok_or(format!("{key} muss eine Zahl sein"))?;
    if n < i64::from(lo) || n > i64::from(hi) {
        return Err(format!("{key} muss zwischen {lo} und {hi} liegen"));
    }
    Ok(n as u32)
}

fn choice(obj: &Map<String, Value>, key: &str, choices: &[&str]) -> Result<String, String> {
    match obj.get(key).and_then(Value::as_str) {
        Some(v) if choices.contains(&v) => Ok(v.to_string()),
        _ => Err(format!("{key}: {}", choices.join(", "))),
    }
}

pub fn clean_camera(new: &Map<String, Value>, old: &Camera) -> Result<Camera, String> {
    let Value::Object(mut c) = serde_json::to_value(old).expect("camera serializes") else { unreachable!() };
    for (k, v) in new {
        if !c.contains_key(k) {
            return Err(format!("camera.{k} gibt es nicht"));
        }
        c.insert(k.clone(), v.clone());
    }
    let name: String = text_of(c.get("name")).trim().chars().take(40).collect();
    let source = choice(&c, "source", &["libcamera", "v4l2", "custom"])?;
    let device = text_of(c.get("device"));
    if !device.strip_prefix("/dev/video").is_some_and(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit())) {
        return Err("Gerät muss /dev/videoN sein".into());
    }
    let custom_source: String = text_of(c.get("custom_source")).trim().chars().take(500).collect();
    if source == "custom" && custom_source.is_empty() {
        return Err("Eigene Quelle ist leer".into());
    }
    let audio_device = text_of(c.get("audio_device"));
    let audio_ok = (1..=64).contains(&audio_device.chars().count())
        && audio_device.chars().all(|ch| ch.is_alphanumeric() || "_:,.=-".contains(ch));
    if !audio_ok {
        return Err("Audio-Gerät z. B. default oder plughw:0,0".into());
    }
    let controls = match c.get("controls") {
        Some(Value::Object(m)) if m.len() <= 100 => m,
        _ => return Err("controls: {name: Zahl oder true/false}".into()),
    };
    let control_ok = |(k, v): (&String, &Value)| {
        (1..=64).contains(&k.len())
            && k.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            && (v.is_number() || v.is_boolean())
    };
    if !controls.iter().all(control_ok) {
        return Err("controls: {name: Zahl oder true/false}".into());
    }
    let profiles: Vec<&str> = H264_PROFILES.iter().map(|p| p.0).collect();
    Ok(Camera {
        enabled: c.get("enabled").is_some_and(truthy),
        name: if name.is_empty() { "Wall Tablet".into() } else { name },
        source,
        camera_id: text_of(c.get("camera_id")).trim().chars().take(200).collect(),
        device,
        custom_source,
        rotate: choice(&c, "rotate", &["none", "90", "180", "270", "hflip", "vflip"])?,
        width: int_field(&c, "width", 160, 3840)?,
        height: int_field(&c, "height", 120, 2160)?,
        fps: int_field(&c, "fps", 1, 60)?,
        bitrate: int_field(&c, "bitrate", 100, 20000)?,
        encoder: choice(&c, "encoder", &["va", "x264"])?,
        profile: choice(&c, "profile", &profiles)?,
        gop: int_field(&c, "gop", 1, 10)?,
        sub_enabled: c.get("sub_enabled").is_some_and(truthy),
        sub_width: int_field(&c, "sub_width", 160, 1920)?,
        sub_height: int_field(&c, "sub_height", 90, 1080)?,
        sub_bitrate: int_field(&c, "sub_bitrate", 64, 8000)?,
        audio_enabled: c.get("audio_enabled").is_some_and(truthy),
        audio_device,
        controls: controls.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

pub fn clean_update(new: &Map<String, Value>, old: &Update) -> Result<Update, String> {
    let get = |k: &str, old: String| new.get(k).map(|v| text_of(Some(v))).unwrap_or(old);
    let repo = get("repo", old.repo.clone()).trim().to_string();
    let branch = get("branch", old.branch.clone()).trim().to_string();
    let auto = new.get("auto").map_or(old.auto, truthy);
    if !is_http_url(&repo) {
        return Err("Repo-Link muss mit https:// anfangen".into());
    }
    let mut chars = branch.chars();
    let branch_ok = chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric() || "._".contains(ch))
        && chars.all(|ch| ch.is_ascii_alphanumeric() || "._/-".contains(ch));
    if !branch_ok {
        return Err("Ungültiger Branch".into());
    }
    Ok(Update { repo, branch, auto })
}

/// Output size of a stream: rotating by 90/270 degrees swaps width and height.
pub fn size(c: &Camera, sub: bool) -> (u32, u32) {
    let (w, h) = if sub { (c.sub_width, c.sub_height) } else { (c.width, c.height) };
    if c.rotate == "90" || c.rotate == "270" { (h, w) } else { (w, h) }
}

pub struct Stream {
    pub profile: &'static str,
    pub encoder: &'static str,
    pub path: &'static str,
    pub width: u32,
    pub height: u32,
    pub kbit: u32,
}

/// The RTSP streams the camera serves and ONVIF announces: main always, sub if enabled.
pub fn streams(c: &Camera) -> Vec<Stream> {
    let (w, h) = size(c, false);
    let mut out = vec![Stream { profile: "main", encoder: "vec", path: "stream", width: w, height: h, kbit: c.bitrate }];
    if c.sub_enabled {
        let (w, h) = size(c, true);
        out.push(Stream { profile: "sub", encoder: "vec_sub", path: "sub", width: w, height: h, kbit: c.sub_bitrate });
    }
    out
}

/// True if version string `remote` is higher than `local` (1.10.0 > 1.9.2).
// shortcut: compares the numbers only, so 1.0.0-alpha.5 counts as 1.0.0.5; fine while no pre-releases are used
pub fn newer(remote: &str, local: &str) -> bool {
    let nums = |v: &str| -> Vec<u64> {
        v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()
    };
    let r = nums(remote);
    !r.is_empty() && r > nums(local)
}

/// scheme://host[:port] of a URL, without default ports, like the browser's location.origin.
pub fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match scheme {
        "http" => host.strip_suffix(":80").unwrap_or(host),
        "https" => host.strip_suffix(":443").unwrap_or(host),
        _ => host,
    };
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn version_compare() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("?", "0.1.0"));
    }

    #[test]
    fn origins() {
        assert_eq!(origin("http://ha:80/lovelace"), "http://ha");
        assert_eq!(origin("https://ha:8123/x?y"), "https://ha:8123");
        assert_eq!(origin("http://10.0.0.5:8123"), "http://10.0.0.5:8123");
    }

    #[test]
    fn loads_python_era_config_and_broken_sections() {
        // config.json as 0.1.x/0.2.0 (Python) wrote it: width as number, missing new keys, extra keys
        let old = json!({
            "locked": true,
            "tabs": [{"name": "HA", "url": "http://ha:8123/"}],
            "ha": {"username": "kiosk", "password": "pw"},
            "camera": {"enabled": true, "width": "1920", "fps": 25, "gone": 1},
            "update": {"repo": "https://github.com/a/b.git", "branch": "dev", "auto": true},
            "future": 1
        });
        let c = Config::from_json(&old.to_string());
        assert!(c.locked);
        assert_eq!(c.tabs, vec![Tab { name: "HA".into(), url: "http://ha:8123/".into(), ha: false }]);
        assert_eq!((c.ha.username.as_str(), c.ha.password.as_str(), c.ha.theme.as_str()), ("kiosk", "pw", "auto"));
        assert!(c.camera.enabled);
        assert_eq!((c.camera.width, c.camera.fps, c.camera.sub_enabled), (1920, 25, true));
        assert_eq!(c.update.branch, "dev");
        // a broken section falls back alone
        let c = Config::from_json(&json!({"tabs": "kaputt", "camera": {"fps": 999}, "locked": true}).to_string());
        assert_eq!(c.tabs, Config::default().tabs);
        assert_eq!(c.camera, Camera::default());
        assert!(c.locked);
        assert_eq!(Config::from_json("kein json"), Config::default());
    }

    #[test]
    fn camera_validation() {
        let d = Camera::default();
        for bad in [
            json!({"rotate": "45"}), json!({"profile": "extreme"}), json!({"gop": 0}), json!({"sub_width": 5000}),
            json!({"audio_device": "hw:0 ! fakesink"}), json!({"controls": {"brightness": "hell"}}),
            json!({"controls": {"Bad Name": 1}}), json!({"controls": [1]}), json!({"fps": 500}),
            json!({"device": "/etc/passwd"}), json!({"bogus": 1}), json!({"source": "custom"}),
        ] {
            assert!(clean_camera(&obj(bad.clone()), &d).is_err(), "{bad}");
        }
        let c = clean_camera(&obj(json!({"fps": "20", "name": "  Flur ", "rotate": "270", "camera_id": "\\_SB_.PCI0.LNK1",
            "controls": {"brightness": 0.2, "ae-enable": false, "awb-mode": 3}})), &d).unwrap();
        assert_eq!((c.fps, c.name.as_str(), c.rotate.as_str()), (20, "Flur", "270"));
        assert_eq!(c.controls.len(), 3);
        assert_eq!(size(&c, false), (720, 1280));
        assert_eq!(size(&c, true), (360, 640));
    }

    #[test]
    fn tabs_ha_update_rotation() {
        let base = Config::default();
        assert!(base.apply(&obj(json!({"tabs": [{"name": "HA", "url": "javascript:alert(1)"}]}))).is_err());
        assert!(base.apply(&obj(json!({"tabs": [{"name": "", "url": "http://x"}]}))).is_err());
        assert!(base.apply(&obj(json!({"tabs": [{"name": "A", "url": "http://x y"}]}))).is_err());
        assert!(base.apply(&obj(json!({"rotation": 45}))).is_err());
        assert!(base.apply(&obj(json!({"update": {"repo": "--upload-pack=x"}}))).is_err());
        assert!(base.apply(&obj(json!({"update": {"branch": "-x"}}))).is_err());
        assert!(base.apply(&obj(json!({"ha": {"theme": "lila"}}))).is_err());
        let c = base.apply(&obj(json!({"rotation": 90, "ha": {"username": "kiosk", "password": "pw"}}))).unwrap();
        assert_eq!(c.rotation, 90);
        let c = c.apply(&obj(json!({"ha": {"theme": "dark"}}))).unwrap();
        assert_eq!((c.ha.username.as_str(), c.ha.theme.as_str()), ("kiosk", "dark"));
        let c = c.apply(&obj(json!({"ha": {"username": "kiosk", "password": ""}}))).unwrap();
        assert_eq!(c.ha.password, "pw"); // empty field keeps the password
        let c = c.apply(&obj(json!({"ha": {"username": ""}}))).unwrap();
        assert_eq!((c.ha.username.as_str(), c.ha.password.as_str()), ("", ""));
    }
}
