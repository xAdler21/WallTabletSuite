//! wts-camera, started by wts-server as `wts-camera '<camera json>' <rtsp port>`.
//! One capture pipeline runs all the time and feeds the RTSP streams /stream (main, with audio if enabled)
//! and /sub (small) through inter elements, plus a JPEG snapshot once a second.
//! Stdout: log lines, "CLIENTS n", "PROBE {json}". Stdin: one {"controls": {...}} per line, applied live;
//! the end of stdin (server gone) stops the camera.
//! `wts-camera --probe '<camera json>'` prints the cameras and controls as JSON and exits.

use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gst_rtsp_server::prelude::*;
use serde_json::{Value, json};
use wts_common::{AUDIO_RATE, Camera, SNAPSHOT, size};

fn log(line: impl AsRef<str>) {
    println!("{}", line.as_ref());
}

fn fail(why: impl AsRef<str>) -> ! {
    eprintln!("{}", why.as_ref());
    std::process::exit(1);
}

fn flip(rotate: &str) -> &'static str {
    match rotate {
        "90" => "clockwise",
        "180" => "rotate-180",
        "270" => "counterclockwise",
        "hflip" => "horizontal-flip",
        "vflip" => "vertical-flip",
        _ => "none",
    }
}

fn source(c: &Camera) -> String {
    match c.source.as_str() {
        "custom" => c.custom_source.clone(),
        "v4l2" => format!("v4l2src name=src device={} ! decodebin", c.device), // decodebin: raw and MJPEG webcams
        _ => "libcamerasrc name=src".into(), // the MIIX 520 cameras sit behind Intel IPU3, only libcamera reads them
    }
}

/// The always-running part: camera -> rotate/scale -> main and sub channels plus the snapshot.
pub fn capture(c: &Camera) -> String {
    let (w, h) = size(c, false);
    let mut parts = vec![
        format!(
            "{} ! videoconvert ! videoflip method={} ! videoscale ! videorate ! video/x-raw,format=I420,width={w},height={h},framerate={}/1 ! tee name=t",
            source(c), flip(&c.rotate), c.fps
        ),
        // sync=false: intervideosrc re-times the frames on the RTSP side anyway
        "t. ! queue leaky=downstream max-size-buffers=2 ! intervideosink channel=main sync=false".into(),
        "t. ! queue leaky=downstream max-size-buffers=1 ! videorate drop-only=true ! video/x-raw,framerate=1/1 ! jpegenc quality=85 ! appsink name=snap max-buffers=1 drop=true sync=false".into(),
    ];
    if c.sub_enabled {
        let (sw, sh) = size(c, true);
        parts.push(format!(
            "t. ! queue leaky=downstream max-size-buffers=2 ! videoscale ! video/x-raw,format=I420,width={sw},height={sh} ! intervideosink channel=sub sync=false"
        ));
    }
    parts.join(" ")
}

/// One RTSP stream, built per client session (shared between clients).
pub fn stream(c: &Camera, sub: bool, audio: bool) -> String {
    let (w, h) = size(c, sub);
    let (kbit, gop) = (if sub { c.sub_bitrate } else { c.bitrate }, c.fps * c.gop);
    let enc = match c.encoder.as_str() {
        "x264" => format!("x264enc tune=zerolatency speed-preset=ultrafast bitrate={kbit} key-int-max={gop}"),
        _ => format!("vah264enc rate-control=cbr bitrate={kbit} key-int-max={gop}"),
    };
    let mut s = format!(
        "( intervideosrc channel={} ! video/x-raw,format=I420,width={w},height={h},framerate={}/1 ! videoconvert ! {enc} ! video/x-h264,profile={} ! h264parse ! rtph264pay name=pay0 pt=96 config-interval=1",
        if sub { "sub" } else { "main" }, c.fps, c.profile
    );
    if audio && !sub {
        s += &format!(
            // provide-clock=false: the audio clock only runs while samples come, a stalled mic would freeze video
            " alsasrc device={} provide-clock=false ! audioconvert ! audioresample ! audio/x-raw,rate={AUDIO_RATE},channels=1 ! avenc_aac bitrate=64000 ! aacparse ! rtpmp4gpay name=pay1 pt=97",
            c.audio_device
        );
    }
    s + " )"
}

/// The microphone delivers samples right now. Opening is not enough: behind PipeWire ("default") the open
/// works even without a capture source, and a stream that never sends a buffer blocks the whole RTSP media.
fn mic_ok(c: &Camera) -> bool {
    let ok = (|| {
        let src = gst::ElementFactory::make("alsasrc").property("device", &c.audio_device).build().ok()?;
        let sink = gst_app::AppSink::builder().max_buffers(1).drop(true).sync(false).build();
        let pipe = gst::Pipeline::new();
        pipe.add_many([&src, sink.upcast_ref()]).ok()?;
        src.link(&sink).ok()?;
        let got = pipe.set_state(gst::State::Playing).is_ok() && sink.try_pull_sample(gst::ClockTime::from_seconds(2)).is_some();
        let _ = pipe.set_state(gst::State::Null);
        Some(got)
    })()
    .unwrap_or(false);
    if !ok {
        log(format!("Ton: {} liefert nichts, Haupt-Stream ohne Ton", c.audio_device));
    }
    ok
}

/// Main-stream factory that decides about audio each time the media is built: video must not die because of
/// the microphone.
mod factory {
    use super::*;
    use gst_rtsp_server::subclass::prelude::*;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct MainFactory {
        pub camera: OnceLock<Camera>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainFactory {
        const NAME: &'static str = "WtsMainFactory";
        type Type = super::MainFactory;
        type ParentType = gst_rtsp_server::RTSPMediaFactory;
    }

    impl ObjectImpl for MainFactory {}

    impl RTSPMediaFactoryImpl for MainFactory {
        fn create_element(&self, _url: &gst_rtsp_server::gst_rtsp::RTSPUrl) -> Option<gst::Element> {
            let c = self.camera.get()?;
            let launch = stream(c, false, c.audio_enabled && mic_ok(c));
            gst::parse::launch_full(&launch, None, gst::ParseFlags::PLACE_IN_BIN)
                .map_err(|e| log(format!("RTSP /stream: {e}")))
                .ok()
        }
    }
}

glib::wrapper! {
    pub struct MainFactory(ObjectSubclass<factory::MainFactory>) @extends gst_rtsp_server::RTSPMediaFactory;
}

fn has(element: &str) -> bool {
    gst::ElementFactory::find(element).is_some()
}

/// Output of a short command (cam), empty if it fails or hangs.
fn run_cmd(cmd: &str, args: &[&str]) -> String {
    let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    else {
        return String::new();
    };
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned()
}

/// `cam -l`: "1: Internal front camera (\_SB_.PCI0.LNK1)"; without location/model just "1: (\_SB_...)".
pub fn parse_cameras(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let (index, rest) = line.trim().split_once(':')?;
            let index: u64 = index.trim().parse().ok()?;
            let rest = rest.trim().strip_suffix(')')?;
            let open = rest.rfind('(')?;
            let (name, id) = (rest[..open].trim(), &rest[open + 1..]);
            (!id.is_empty()).then(|| json!({"index": index, "name": if name.is_empty() { id } else { name }, "id": id}))
        })
        .collect()
}

/// libcamera control name -> libcamerasrc property name: AeEnable -> ae-enable.
pub fn kebab(name: &str) -> String {
    let mut s = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            s.push('-');
        }
        s.push(ch.to_ascii_lowercase());
    }
    s
}

/// `cam --list-controls`: "Control: [inout] ExposureTime: [1..66666]" -> {"exposure-time": Some((1, 66666))}.
/// The direction tag is padded ("[in   ]"), the namespace ("draft::") optional, the range may be missing.
pub fn parse_controls(text: &str) -> BTreeMap<String, Option<(f64, f64)>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.split_once("Control:").map(|p| p.1.trim_start()) else { continue };
        let rest = match rest.strip_prefix('[') {
            Some(r) => r.split_once(']').map_or("", |p| p.1).trim_start(),
            None => rest,
        };
        let Some((name, range)) = rest.split_once(": ").or_else(|| rest.strip_suffix(':').map(|n| (n, ""))) else { continue };
        let name = name.rsplit("::").next().unwrap_or(name);
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let range = range.trim().strip_prefix('[').and_then(|r| r.strip_suffix(']')).and_then(|r| r.split_once("..")).and_then(|(lo, hi)| {
            Some((lo.trim().parse::<f64>().ok()?, hi.trim().parse::<f64>().ok()?))
        });
        out.insert(kebab(name), range.filter(|(lo, hi)| lo <= hi));
    }
    out
}

/// Cameras libcamera sees and the libcamerasrc controls this camera supports. Lists the controls with `cam`,
/// which opens the camera: only call it while the capture pipeline is not running.
fn probe(c: &Camera) -> Value {
    if c.source != "libcamera" {
        return json!({"cameras": [], "controls": []});
    }
    let cameras = parse_cameras(&run_cmd("cam", &["-l"]));
    let index = cameras.iter().find(|cam| cam["id"] == c.camera_id.as_str()).and_then(|cam| cam["index"].as_u64()).unwrap_or(1);
    let supported = parse_controls(&run_cmd("cam", &["-c", &index.to_string(), "--list-controls"]));
    let Ok(src) = gst::ElementFactory::make("libcamerasrc").build() else {
        return json!({"cameras": cameras, "controls": [], "error": "libcamerasrc fehlt (gstreamer1.0-libcamera)"});
    };
    let mut controls = Vec::new();
    for spec in src.list_properties() {
        let flags = spec.flags();
        let name = spec.name().to_string();
        let writable = flags.contains(glib::ParamFlags::WRITABLE) && !flags.contains(glib::ParamFlags::CONSTRUCT_ONLY);
        if spec.owner_type() != src.type_() || name == "camera-name" || !writable {
            continue;
        }
        if !supported.is_empty() && !supported.contains_key(&name) {
            continue; // nothing parsed: show all, the camera ignores the rest
        }
        let mut e = json!({"name": name, "label": spec.nick(), "help": spec.blurb().unwrap_or_default()});
        let numeric = |e: &mut Value, kind: &str, min: f64, max: f64, default: f64| {
            // pspec ranges are just the type's limits; cam knows the camera's real range
            let (min, max) = supported.get(&name).copied().flatten().unwrap_or((min, max));
            let round = |v: f64| if kind == "int" { v.round() } else { v };
            e["type"] = json!(kind);
            e["min"] = json!(round(min));
            e["max"] = json!(round(max));
            e["default"] = json!(round(default.clamp(min, max)));
        };
        if let Some(p) = spec.downcast_ref::<glib::ParamSpecBoolean>() {
            e["type"] = json!("bool");
            e["default"] = json!(p.default_value());
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecInt>() {
            numeric(&mut e, "int", p.minimum().into(), p.maximum().into(), p.default_value().into());
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecUInt>() {
            numeric(&mut e, "int", p.minimum().into(), p.maximum().into(), p.default_value().into());
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecInt64>() {
            numeric(&mut e, "int", p.minimum() as f64, p.maximum() as f64, p.default_value() as f64);
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecUInt64>() {
            numeric(&mut e, "int", p.minimum() as f64, p.maximum() as f64, p.default_value() as f64);
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecFloat>() {
            numeric(&mut e, "float", p.minimum().into(), p.maximum().into(), p.default_value().into());
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecDouble>() {
            numeric(&mut e, "float", p.minimum(), p.maximum(), p.default_value());
        } else if let Some(p) = spec.downcast_ref::<glib::ParamSpecEnum>() {
            let options: Vec<Value> = p.enum_class().values().iter().map(|v| json!({"value": v.value(), "label": v.nick()})).collect();
            e["type"] = json!("enum");
            e["default"] = json!(p.default_value_as_i32());
            e["options"] = json!(options);
        } else {
            continue; // rectangles, arrays: not adjustable from the panel
        }
        controls.push(e);
    }
    json!({"cameras": cameras, "controls": controls})
}

/// Sets image controls on the running source; values out of range or of the wrong kind are logged, never fatal.
fn set_controls(src: &gst::Element, controls: &serde_json::Map<String, Value>) {
    for (name, v) in controls {
        let Some(spec) = src.find_property(name) else {
            log(format!("Regler {name} gibt es bei dieser Quelle nicht"));
            continue;
        };
        let flags = spec.flags();
        if !flags.contains(glib::ParamFlags::WRITABLE) || flags.contains(glib::ParamFlags::CONSTRUCT_ONLY) {
            log(format!("Regler {name} lässt sich nicht setzen"));
            continue;
        }
        let n = v.as_f64().unwrap_or_else(|| f64::from(u8::from(v.as_bool().unwrap_or(false))));
        let t = spec.value_type();
        let value = if t == glib::Type::BOOL {
            (n != 0.0).to_value()
        } else if t == glib::Type::I32 {
            (n.round() as i32).to_value()
        } else if t == glib::Type::U32 {
            (n.round() as u32).to_value()
        } else if t == glib::Type::I64 {
            (n.round() as i64).to_value()
        } else if t == glib::Type::U64 {
            (n.round() as u64).to_value()
        } else if t == glib::Type::F32 {
            (n as f32).to_value()
        } else if t == glib::Type::F64 {
            n.to_value()
        } else if let Some(v) = t.is_a(glib::Type::ENUM).then(|| glib::EnumClass::with_type(t)).flatten().and_then(|c| c.to_value(n.round() as i32)) {
            v
        } else {
            log(format!("Regler {name}={v}: Wert passt nicht zum Typ {t}"));
            continue;
        };
        if !spec.value_is_valid(&value) {
            log(format!("Regler {name}={v}: außerhalb des gültigen Bereichs"));
            continue;
        }
        src.set_property_from_value(name, &value);
    }
}

fn save_snapshot(sink: &gst_app::AppSink) -> Result<gst::FlowSuccess, gst::FlowError> {
    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
    if let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) {
        let tmp = format!("{SNAPSHOT}.tmp");
        if std::fs::write(&tmp, map.as_slice()).is_ok() {
            let _ = std::fs::rename(&tmp, SNAPSHOT);
        }
    }
    Ok(gst::FlowSuccess::Ok)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Err(e) = gst::init() {
        fail(format!("GStreamer: {e}"));
    }
    let config = |i: usize| -> Camera {
        let text = args.get(i).unwrap_or_else(|| fail("Aufruf: wts-camera '<json>' <port> | wts-camera --probe '<json>'"));
        serde_json::from_str(text).unwrap_or_else(|e| fail(format!("Kamera-Konfiguration: {e}")))
    };
    if args.get(1).map(String::as_str) == Some("--probe") {
        println!("{}", probe(&config(2)));
        return;
    }
    let port = args.get(2).cloned().unwrap_or_else(|| wts_common::RTSP_PORT.to_string());
    let code = run(config(1), &port);
    std::process::exit(code);
}

fn run(mut c: Camera, port: &str) -> i32 {
    if c.encoder == "va" && !has("vah264enc") && has("x264enc") {
        log("vah264enc fehlt (VA-Treiber ohne H.264-Encoder), nehme x264enc");
        c.encoder = "x264".into();
    }
    let mut need = vec!["intervideosink", "intervideosrc", "jpegenc", "appsink", "h264parse", "rtph264pay"];
    need.push(if c.encoder == "x264" { "x264enc" } else { "vah264enc" });
    match c.source.as_str() {
        "libcamera" => need.push("libcamerasrc"),
        "v4l2" => need.push("v4l2src"),
        _ => {}
    }
    let missing: Vec<&str> = need.into_iter().filter(|e| !has(e)).collect();
    if !missing.is_empty() {
        fail(format!("GStreamer-Element fehlt: {} (install.sh erneut ausführen oder Encoder/Quelle wechseln)", missing.join(", ")));
    }
    if c.audio_enabled {
        let missing: Vec<&str> = ["alsasrc", "avenc_aac", "aacparse", "rtpmp4gpay"].into_iter().filter(|e| !has(e)).collect();
        if !missing.is_empty() {
            log(format!("Ton aus, GStreamer-Element fehlt: {}", missing.join(", ")));
            c.audio_enabled = false;
        }
    }

    println!("PROBE {}", probe(&c)); // before the pipeline: listing the controls opens the camera
    let launch = capture(&c);
    log(format!("Aufnahme: {launch}"));
    let pipe = gst::parse::launch(&launch)
        .unwrap_or_else(|e| fail(format!("Pipeline ungültig: {e}")))
        .downcast::<gst::Pipeline>()
        .unwrap_or_else(|_| fail("Pipeline ungültig: keine Pipeline"));
    let src = pipe.by_name("src");
    if let Some(src) = &src {
        if c.source == "libcamera" && !c.camera_id.is_empty() && src.find_property("camera-name").is_some() {
            src.set_property("camera-name", &c.camera_id);
        }
        set_controls(src, &c.controls.clone().into_iter().collect());
    }
    let snap = pipe.by_name("snap").and_then(|s| s.downcast::<gst_app::AppSink>().ok()).expect("snap is an appsink");
    let first = AtomicBool::new(true);
    snap.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let r = save_snapshot(sink);
                if first.swap(false, Ordering::Relaxed) {
                    log("Kamera liefert Bilder");
                }
                r
            })
            .build(),
    );

    let main_loop = glib::MainLoop::new(None, false);
    let failed = Arc::new(AtomicBool::new(false));
    let bus = pipe.bus().expect("pipeline has a bus");
    let (l, f) = (main_loop.clone(), failed.clone());
    let _watch = bus
        .add_watch_local(move |_, msg| {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    log(format!("Kamera-Fehler: {} ({})", e.error(), e.debug().map(|d| d.to_string()).unwrap_or_default()));
                    f.store(true, Ordering::Relaxed);
                    l.quit();
                }
                gst::MessageView::Eos(_) => {
                    log("Kamera-Quelle beendet");
                    f.store(true, Ordering::Relaxed);
                    l.quit();
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        })
        .expect("one bus watch");
    if pipe.set_state(gst::State::Playing).is_err() {
        let why = bus
            .pop_filtered(&[gst::MessageType::Error])
            .and_then(|m| match m.view() {
                gst::MessageView::Error(e) => Some(e.error().to_string()),
                _ => None,
            })
            .unwrap_or_default();
        fail(format!("Kamera lässt sich nicht starten {why}"));
    }

    let server = gst_rtsp_server::RTSPServer::new();
    server.set_service(port);
    let mounts = server.mount_points().expect("server has mount points");
    let main = MainFactory::new(c.clone());
    main.set_shared(true); // one encoder per stream for all clients
    mounts.add_factory("/stream", main);
    log(format!("RTSP /stream: {}", stream(&c, false, c.audio_enabled)));
    if c.sub_enabled {
        let sub = gst_rtsp_server::RTSPMediaFactory::new();
        sub.set_launch(&stream(&c, true, false));
        sub.set_shared(true);
        mounts.add_factory("/sub", sub);
        log(format!("RTSP /sub: {}", stream(&c, true, false)));
    }
    let clients = Arc::new(AtomicI32::new(0));
    server.connect_client_connected(move |_, client| {
        println!("CLIENTS {}", clients.fetch_add(1, Ordering::Relaxed) + 1);
        let clients = clients.clone();
        client.connect_closed(move |_| println!("CLIENTS {}", clients.fetch_sub(1, Ordering::Relaxed) - 1));
    });
    let _server_source = server.attach(None).unwrap_or_else(|_| fail(format!("RTSP-Port {port} belegt")));

    // before the stdin thread: its invoke() may own the default context briefly, and this call would then panic
    let l = main_loop.clone();
    glib::unix_signal_add_local(libc::SIGTERM, move || {
        l.quit();
        glib::ControlFlow::Break
    });
    // live control changes from the panel; stdin closes when the server goes away
    let (l, live) = (main_loop.clone(), src.clone());
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&line) else { continue };
            let (Some(Value::Object(ctl)), Some(src)) = (m.remove("controls"), live.clone()) else { continue };
            glib::MainContext::default().invoke(move || set_controls(&src, &ctl));
        }
        l.quit();
    });

    log(format!("RTSP bereit auf Port {port}"));
    main_loop.run();
    let _ = pipe.set_state(gst::State::Null);
    i32::from(failed.load(Ordering::Relaxed))
}

impl MainFactory {
    fn new(c: Camera) -> MainFactory {
        use glib::subclass::prelude::ObjectSubclassIsExt;
        let f: MainFactory = glib::Object::new();
        let _ = f.imp().camera.set(c);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cam_output() {
        let cams = parse_cameras("Available cameras:\n1: Internal front camera (\\_SB_.PCI0.LNK1)\n2: (\\_SB_.PCI0.LNK0)\n3: 'HD Webcam' (/base/usb@1-1.2:1.0-0c45:6366)\n");
        assert_eq!(cams.len(), 3);
        assert_eq!((cams[0]["index"].as_u64(), cams[0]["name"].as_str(), cams[0]["id"].as_str()), (Some(1), Some("Internal front camera"), Some("\\_SB_.PCI0.LNK1")));
        assert_eq!(cams[1]["name"], "\\_SB_.PCI0.LNK0");
        assert_eq!(cams[2]["id"], "/base/usb@1-1.2:1.0-0c45:6366");
        let ctl = parse_controls(
            "Camera 1\nControl: [inout] ExposureTime: [1..66666]\nControl: [in   ] AeEnable: [false..true]\nControl: [  out] draft::PipelineDepth: [2..3]\nControl: AeExposureMode: [0..3]\nControl: [inout] Brightness: [-1.000000..1.000000]\nControl: [inout] ScalerCrop: [(0, 0)/0x0..(65535, 65535)/65535x65535]\n",
        );
        assert_eq!(ctl["exposure-time"], Some((1.0, 66666.0)));
        assert_eq!(ctl["ae-enable"], None);
        assert_eq!(ctl["pipeline-depth"], Some((2.0, 3.0)));
        assert_eq!(ctl["ae-exposure-mode"], Some((0.0, 3.0)));
        assert_eq!(ctl["brightness"], Some((-1.0, 1.0)));
        assert_eq!(ctl["scaler-crop"], None);
        assert_eq!(kebab("AeEnable"), "ae-enable");
    }

    #[test]
    fn pipelines() {
        let c = Camera { rotate: "90".into(), audio_enabled: true, encoder: "x264".into(), ..Camera::default() };
        let cap = capture(&c);
        assert!(cap.starts_with("libcamerasrc name=src ! videoconvert ! videoflip method=clockwise"));
        assert!(cap.contains("width=720,height=1280,framerate=15/1"));
        assert!(cap.contains("width=360,height=640 ! intervideosink channel=sub"));
        let main = stream(&c, false, true);
        assert!(main.contains("x264enc") && main.contains("key-int-max=30") && main.contains("alsasrc device=default"));
        assert!(!stream(&c, false, false).contains("alsasrc"));
        assert!(!stream(&c, true, true).contains("alsasrc"));
        assert!(stream(&c, true, false).contains("channel=sub") && stream(&c, true, false).contains("bitrate=500"));
        let c = Camera { sub_enabled: false, source: "v4l2".into(), ..Camera::default() };
        assert!(!capture(&c).contains("channel=sub"));
        assert!(capture(&c).starts_with("v4l2src name=src device=/dev/video0 ! decodebin"));
        assert!(stream(&c, false, false).contains("vah264enc rate-control=cbr bitrate=2000"));
    }
}
