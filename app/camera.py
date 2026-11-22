#!/usr/bin/env python3
"""Camera process. server.py starts it as: camera.py '<camera config json>' <rtsp port>.

One capture pipeline runs all the time and feeds the RTSP streams /stream (main, with audio if enabled)
and /sub (small) through inter elements, plus a JPEG snapshot once a second.
Stdout: log lines, "CLIENTS n", "PROBE {json}". Stdin: one JSON object per line, {"controls": {...}},
applied to the running camera without a restart.
camera.py --probe '<camera config json>' prints the probe JSON (cameras, controls) and exits.
"""
import json
import os
import re
import subprocess
import sys
import threading

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtspServer", "1.0")
from gi.repository import GLib, GObject, Gst, GstRtspServer  # noqa: E402

import onvif  # noqa: E402  stream sizes and snapshot path, shared with the ONVIF answers

FLIP = {"none": "none", "90": "clockwise", "180": "rotate-180", "270": "counterclockwise",
        "hflip": "horizontal-flip", "vflip": "vertical-flip"}
INTS = (GObject.TYPE_INT, GObject.TYPE_UINT, GObject.TYPE_INT64, GObject.TYPE_UINT64,
        GObject.TYPE_LONG, GObject.TYPE_ULONG)
FLOATS = (GObject.TYPE_FLOAT, GObject.TYPE_DOUBLE)


def log(text):
    print(text, flush=True)


def source(c):
    if c["source"] == "custom":
        return c["custom_source"]
    if c["source"] == "v4l2":
        return f"v4l2src name=src device={c['device']} ! decodebin"  # decodebin: raw and MJPEG webcams
    return "libcamerasrc name=src"  # the MIIX 520 cameras sit behind Intel IPU3, only libcamera reads them


def capture(c):
    w, h = onvif.size(c)
    parts = [
        f"{source(c)} ! videoconvert ! videoflip method={FLIP[c['rotate']]} ! videoscale ! videorate ! "
        f"video/x-raw,format=I420,width={w},height={h},framerate={c['fps']}/1 ! tee name=t",
        "t. ! queue leaky=downstream max-size-buffers=2 ! intervideosink channel=main",
        "t. ! queue leaky=downstream max-size-buffers=1 ! videorate drop-only=true ! video/x-raw,framerate=1/1 ! "
        "jpegenc quality=85 ! appsink name=snap max-buffers=1 drop=true emit-signals=true sync=false",
    ]
    if c["sub_enabled"]:
        sw, sh = onvif.size(c, sub=True)
        parts.append(f"t. ! queue leaky=downstream max-size-buffers=2 ! videoscale ! "
                     f"video/x-raw,format=I420,width={sw},height={sh} ! intervideosink channel=sub")
    return " ".join(parts)


def stream(c, sub):
    w, h = onvif.size(c, sub)
    kbit, gop = (c["sub_bitrate"] if sub else c["bitrate"]), c["fps"] * c["gop"]
    enc = {"va": f"vah264enc rate-control=cbr bitrate={kbit} key-int-max={gop}",
           "x264": f"x264enc tune=zerolatency speed-preset=ultrafast bitrate={kbit} key-int-max={gop}"}[c["encoder"]]
    video = (f"intervideosrc channel={'sub' if sub else 'main'} ! "
             f"video/x-raw,format=I420,width={w},height={h},framerate={c['fps']}/1 ! videoconvert ! {enc} ! "
             f"video/x-h264,profile={c['profile']} ! h264parse ! rtph264pay name=pay0 pt=96 config-interval=1")
    audio = ""
    if c["audio_enabled"] and not sub:
        audio = (f" alsasrc device={c['audio_device']} ! audioconvert ! audioresample ! "
                 f"audio/x-raw,rate={onvif.AUDIO_RATE},channels=1 ! avenc_aac bitrate=64000 ! aacparse ! "
                 "rtpmp4gpay name=pay1 pt=97")
    return f"( {video}{audio} )"


def kebab(name):
    """libcamera control name -> libcamerasrc property name: AeEnable -> ae-enable."""
    return re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower()


def probe(c):
    """Cameras libcamera sees and the controls of libcamerasrc this camera supports."""
    out = {"cameras": [], "controls": []}
    if c["source"] != "libcamera":
        return out
    try:
        listing = subprocess.run(["cam", "-l"], capture_output=True, text=True, timeout=15).stdout
    except (OSError, subprocess.SubprocessError):
        listing = ""
    for m in re.finditer(r"^\s*(\d+): (.*?) \((.+)\)\s*$", listing, re.M):
        out["cameras"].append({"index": int(m[1]), "name": m[2], "id": m[3]})
    index = next((cam["index"] for cam in out["cameras"] if cam["id"] == c["camera_id"]), 1)
    supported = set()
    try:  # opens the camera, so only possible before the pipeline runs
        r = subprocess.run(["cam", "-c", str(index), "--list-controls"], capture_output=True, text=True, timeout=15)
        supported = {kebab(m[1]) for m in re.finditer(r"Control:(?:\s*\[\w+\])?\s*(?:\w+::)?(\w+):", r.stdout)}
    except (OSError, subprocess.SubprocessError):
        pass
    src = Gst.ElementFactory.make("libcamerasrc")
    if src is None:
        return out
    for spec in src.list_properties():
        writable = spec.flags & GObject.ParamFlags.WRITABLE and not spec.flags & GObject.ParamFlags.CONSTRUCT_ONLY
        if spec.owner_type != src.__gtype__ or spec.name == "camera-name" or not writable:
            continue
        if supported and spec.name not in supported:  # nothing parsed: show all, the camera ignores the rest
            continue
        entry = {"name": spec.name, "label": spec.get_nick() or spec.name, "help": spec.get_blurb() or ""}
        t = spec.value_type
        if t == GObject.TYPE_BOOLEAN:
            entry.update(type="bool", default=bool(spec.default_value))
        elif t in INTS or t in FLOATS:
            entry.update(type="int" if t in INTS else "float", min=spec.minimum, max=spec.maximum,
                         default=spec.default_value)
        elif t.is_a(GObject.TYPE_ENUM):
            values = type(spec.default_value).__enum_values__  # {int: GEnum member}
            entry.update(type="enum", default=int(spec.default_value),
                         options=[{"value": int(v), "label": e.value_nick} for v, e in values.items()])
        else:
            continue  # rectangles, arrays: not adjustable from the panel
        out["controls"].append(entry)
    return out


def set_controls(src, controls):
    for name, value in controls.items():
        spec = src.find_property(name)
        if spec is None or not spec.flags & GObject.ParamFlags.WRITABLE:
            log(f"Regler {name} gibt es bei dieser Quelle nicht")
            continue
        t = spec.value_type
        try:
            if t == GObject.TYPE_BOOLEAN:
                src.set_property(name, bool(value))
            elif t in INTS or t.is_a(GObject.TYPE_ENUM):
                src.set_property(name, int(value))
            elif t in FLOATS:
                src.set_property(name, float(value))
        except (TypeError, ValueError, OverflowError) as e:
            log(f"Regler {name}={value}: {e}")


def save_snapshot(sink):
    buf = sink.emit("pull-sample").get_buffer()
    ok, info = buf.map(Gst.MapFlags.READ)
    if ok:
        try:
            with open(onvif.SNAPSHOT + ".tmp", "wb") as f:
                f.write(info.data)
            os.replace(onvif.SNAPSHOT + ".tmp", onvif.SNAPSHOT)
        except OSError:
            pass
        finally:
            buf.unmap(info)
    if not save_snapshot.done:
        save_snapshot.done = True
        log("Kamera liefert Bilder")
    return Gst.FlowReturn.OK


save_snapshot.done = False


def main():
    if sys.argv[1] == "--probe":
        Gst.init(None)
        print(json.dumps(probe(json.loads(sys.argv[2]))))
        return
    c, port = json.loads(sys.argv[1]), sys.argv[2]
    Gst.init(None)
    need = ["intervideosink", "jpegenc", {"va": "vah264enc", "x264": "x264enc"}[c["encoder"]],
            {"libcamera": "libcamerasrc", "v4l2": "v4l2src"}.get(c["source"])]
    if c["audio_enabled"]:
        need += ["alsasrc", "avenc_aac", "rtpmp4gpay"]
    missing = [e for e in need if e and not Gst.ElementFactory.find(e)]
    if missing:
        sys.exit(f"GStreamer-Element fehlt: {', '.join(missing)} (install.sh erneut ausführen oder Encoder/Quelle wechseln)")

    log("PROBE " + json.dumps(probe(c)))  # before the pipeline: listing the controls opens the camera
    log(f"Aufnahme: {capture(c)}")
    try:
        pipe = Gst.parse_launch(capture(c))
    except GLib.Error as e:
        sys.exit(f"Pipeline ungültig: {e.message}")
    src = pipe.get_by_name("src")
    if src is not None:
        if c["source"] == "libcamera" and c["camera_id"]:
            src.set_property("camera-name", c["camera_id"])
        set_controls(src, c["controls"])
    pipe.get_by_name("snap").connect("new-sample", save_snapshot)

    loop = GLib.MainLoop()

    def on_error(_bus, msg):
        err, dbg = msg.parse_error()
        log(f"Kamera-Fehler: {err.message} ({dbg})")
        loop.quit()

    bus = pipe.get_bus()
    bus.add_signal_watch()
    bus.connect("message::error", on_error)
    bus.connect("message::eos", lambda *_: (log("Kamera-Quelle beendet"), loop.quit()))
    if pipe.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
        sys.exit("Kamera lässt sich nicht starten")

    server = GstRtspServer.RTSPServer()
    server.set_service(port)
    for path, sub in (("/stream", False), ("/sub", True)):
        if sub and not c["sub_enabled"]:
            continue
        factory = GstRtspServer.RTSPMediaFactory()
        factory.set_launch(stream(c, sub))
        factory.set_shared(True)  # one encoder per stream for all clients
        server.get_mount_points().add_factory(path, factory)
        log(f"RTSP {path}: {stream(c, sub)}")

    clients = [0]

    def connected(_server, client):
        clients[0] += 1
        log(f"CLIENTS {clients[0]}")

        def closed(_client):
            clients[0] -= 1
            log(f"CLIENTS {clients[0]}")

        client.connect("closed", closed)

    server.connect("client-connected", connected)
    if server.attach(None) == 0:
        sys.exit(f"RTSP-Port {port} belegt")

    def read_stdin():  # live control changes from the panel
        for line in sys.stdin:
            try:
                controls = json.loads(line)["controls"]
            except (ValueError, KeyError, TypeError):
                continue
            if src is not None:
                GLib.idle_add(lambda ctl=controls: (set_controls(src, ctl), False)[1])

    threading.Thread(target=read_stdin, daemon=True).start()
    log(f"RTSP bereit auf Port {port}")
    loop.run()
    pipe.set_state(Gst.State.NULL)
    sys.exit(1)


if __name__ == "__main__":
    main()
