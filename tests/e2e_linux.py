#!/usr/bin/env python3
"""End-to-end test on a real Ubuntu (WSL, a VM or the tablet itself), without touching the installed system
(the camera part needs RTSP port 8554 and /dev/shm/walltablet-snapshot.jpg, it is skipped if a camera runs).

Runs the release binaries (wts-server, wts-kiosk inside a headless cage, wts-camera with a GStreamer test
picture), then checks everything from the outside: screenshots (grim), taps through the rotation (virtual pointer),
HTTP, RTSP (ffprobe/ffmpeg) and ONVIF (onvif-zeep if installed). Needs the packages from deploy/install.sh
plus: grim ffmpeg (optionally `pip install onvif-zeep`), and a build: cargo build --release --workspace.

    python3 tests/e2e_linux.py            # all checks
    python3 tests/e2e_linux.py kiosk      # only kiosk checks (rotation, taps, lock, safe mode)
    python3 tests/e2e_linux.py camera     # only camera/ONVIF checks
WTS_BIN=<dir> picks the binaries (default: $CARGO_TARGET_DIR/release or target/release).
"""
import glob
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("WTS_BIN") or Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target") / "release")
PORT = 18447          # wts-server (keeps 8447 free if a real install runs on this machine)
PAGE_PORT = 18080     # test page shown in the kiosk
SERVER = f"http://127.0.0.1:{PORT}"
TMP = Path(tempfile.mkdtemp(prefix="wts-e2e-"))
results = []
procs = []

# top half red, bottom half blue: makes the orientation visible in a screenshot. A tap on the red half
# paints everything green, on the blue half yellow: shows where a tap lands in the page.
PAGE = b"""<!doctype html><html><body style="margin:0">
<div id="a" style="height:50vh;background:#ff0000" onclick="paint('#00ff00')"></div>
<div id="b" style="height:50vh;background:#0000ff" onclick="paint('#ffff00')"></div>
<script>function paint(c) { a.style.background = b.style.background = c; }</script>
</body></html>"""
# green page that opens the red/blue page with window.open on a tap (like Home Assistant's url actions)
OPENER = b"""<!doctype html><html><body style="margin:0;height:100vh;background:#00ff00"
onclick="window.open('/?opened', '_blank')"></body></html>"""


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""), flush=True)
    return ok


def http(path, body=None, timeout=10):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(SERVER + path, data, {"Content-Type": "application/json"} if data else {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if r.headers.get_content_type() == "application/json" else raw)
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except OSError as e:
        return 0, str(e)


def wait(cond, timeout, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(step)
    return False


def start(name, cmd, env=None):
    log = open(TMP / f"{name}.log", "w")
    p = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, env={**os.environ, **(env or {})},
                         cwd=ROOT, start_new_session=True)
    procs.append(p)
    return p


def tail(name, n=30):
    try:
        return "\n".join((TMP / f"{name}.log").read_text(errors="replace").splitlines()[-n:])
    except OSError:
        return ""


def stop_all():
    for p in procs:
        if p.poll() is None:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except OSError:
                pass
    for p in procs:
        try:
            p.wait(5)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)


# ---------- screenshots ----------

def screenshot(wayland):
    """(width, height, rgb bytes) of the cage output via grim (PPM, no image library needed)."""
    r = subprocess.run(["grim", "-t", "ppm", "-"], capture_output=True, timeout=15,
                       env={**os.environ, "WAYLAND_DISPLAY": wayland})
    if r.returncode:
        raise RuntimeError(f"grim: {r.stderr.decode(errors='replace').strip()}")
    out = r.stdout
    parts = out.split(maxsplit=4)  # P6, width, height, maxval, data
    w, h = int(parts[1]), int(parts[2])
    return w, h, parts[4][: w * h * 3]


def color(shot, fx, fy):
    """Dominant colour name at a fractional position."""
    w, h, px = shot
    i = (int(fy * (h - 1)) * w + int(fx * (w - 1))) * 3
    r, g, b = px[i], px[i + 1], px[i + 2]
    if r > 180 and g < 80 and b < 80:
        return "red"
    if b > 180 and r < 80 and g < 80:
        return "blue"
    if g > 180 and r < 80 and b < 80:
        return "green"
    if r > 180 and g > 180 and b < 80:
        return "yellow"
    if r < 60 and g < 60 and b < 60:
        return "dark"
    return f"rgb({r},{g},{b})"


def layout(shot):
    """Where the red half is: top, bottom, left or right."""
    # corners, away from the red/blue border that runs through the middle in every orientation
    c = {k: color(shot, *v) for k, v in {"tl": (.15, .15), "tr": (.85, .15), "bl": (.15, .85), "br": (.85, .85)}.items()}
    halves = {"top": {"tl", "tr"}, "bottom": {"bl", "br"}, "left": {"tl", "bl"}, "right": {"tr", "br"}}
    for name, red in halves.items():
        if all(c[k] == ("red" if k in red else "blue") for k in c):
            return name
    return f"unclear {c}"


def tap(wayland, fx, fy):
    """Clicks at a fractional screen position through one zwlr_virtual_pointer that lives for the whole move +
    click (tools like wlrctl make a new pointer per call: the seat loses its pointer in between and GTK misses
    the click). Speaks the Wayland wire protocol directly, no extra package. Returns an error or ''."""
    def msg(obj, op, *args):  # args: int = uint, str = string
        body = b""
        for a in args:
            if isinstance(a, str):
                s = a.encode() + b"\0"
                body += struct.pack("<I", len(s)) + s + b"\0" * (-len(s) % 4)
            else:
                body += struct.pack("<I", a)
        return struct.pack("<II", obj, (8 + len(body)) << 16 | op) + body

    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(5)
            s.connect(wayland)
            s.sendall(msg(1, 1, 2) + msg(1, 0, 3))  # wl_display.get_registry(2), wl_display.sync(3)
            buf, name, version = b"", None, 0
            while True:  # registry globals until the sync callback is done
                while len(buf) < 8 or len(buf) < struct.unpack_from("<I", buf, 4)[0] >> 16:
                    chunk = s.recv(65536)
                    if not chunk:
                        return "compositor closed the connection"
                    buf += chunk
                obj, size_op = struct.unpack_from("<II", buf)
                size, op, data, buf = size_op >> 16, size_op & 0xFFFF, buf[8:size_op >> 16], buf[size_op >> 16:]
                if obj == 2 and op == 0:  # wl_registry.global(name, interface, version)
                    n, ln = struct.unpack_from("<II", data)
                    if data[8:8 + ln - 1] == b"zwlr_virtual_pointer_manager_v1":
                        name, version = n, struct.unpack_from("<I", data, 8 + ln + (-ln % 4))[0]
                elif obj == 3:
                    break
                elif obj == 1 and op == 0:
                    return f"wayland error: {data[8:]!r}"
            if name is None:
                return "compositor has no zwlr_virtual_pointer_manager_v1"
            # bind manager as 4, create_virtual_pointer(seat=null) as 5
            s.sendall(msg(2, 0, name, "zwlr_virtual_pointer_manager_v1", min(version, 1), 4) + msg(4, 0, 0, 5))
            time.sleep(0.5)  # the client binds wl_pointer now that the seat has one
            t = int(time.monotonic() * 1000) & 0xFFFFFFFF
            ext = 10000
            s.sendall(msg(5, 1, t, int(fx * ext), int(fy * ext), ext, ext) + msg(5, 4))  # motion_absolute, frame
            time.sleep(0.3)
            s.sendall(msg(5, 2, t + 300, 0x110, 1) + msg(5, 4))  # button BTN_LEFT pressed, frame
            time.sleep(0.1)
            s.sendall(msg(5, 2, t + 400, 0x110, 0) + msg(5, 4))  # released, frame
            time.sleep(0.3)
            s.sendall(msg(5, 8))  # destroy
    except OSError as e:
        return f"virtual pointer: {e}"
    return ""


# ---------- test page ----------

class Page(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        page = OPENER if self.path.startswith("/open") else PAGE
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(page)))
        self.end_headers()
        self.wfile.write(page)


# ---------- checks ----------

def kiosk_checks():
    if not check("cage installed", shutil.which("cage")) or not check("grim installed", shutil.which("grim")):
        return
    runtime = TMP / "runtime"
    runtime.mkdir(mode=0o700)
    env = {"XDG_RUNTIME_DIR": str(runtime), "WLR_BACKENDS": "headless", "WLR_RENDERER": "pixman",
           "WLR_LIBINPUT_NO_DEVICES": "1", "WLR_HEADLESS_OUTPUTS": "1", "LIBGL_ALWAYS_SOFTWARE": "1",
           "XDG_DATA_HOME": str(TMP / "share"), "WTS_SERVER": SERVER, "WAYLAND_DISPLAY": "",
           "DISPLAY": ""}
    kiosk = ["cage", "-s", "--", str(BIN / "wts-kiosk")]
    cage = start("kiosk", kiosk, env)
    found = wait(lambda: glob.glob(str(runtime / "wayland-*[0-9]")), 15)
    if not check("cage starts", found and cage.poll() is None, tail("kiosk")):
        return
    # absolute path: a bare name would resolve in the caller's XDG_RUNTIME_DIR (e.g. WSLg's compositor)
    wayland = sorted(glob.glob(str(runtime / "wayland-*[0-9]")))[0]
    online = wait(lambda: http("/api/config")[1]["kiosk"]["online"], 40)
    check("kiosk polls the server", online, tail("kiosk"))
    check("health 503 while the kiosk runs < 20 s", http("/api/health")[0] == 503)
    time.sleep(22)  # the loop on the tablet happened within seconds
    check("kiosk still running after 20 s", cage.poll() is None, tail("kiosk"))
    check("health 200", http("/api/health")[0] == 200)

    if not wait(lambda: layout(screenshot(wayland)) == "top", 40):
        check("page visible, rotation 0", False, f"{layout(screenshot(wayland))}; {tail('kiosk')}")
        return
    check("page visible, rotation 0", True)

    seen = {"last": "top"}
    for deg in (90, 180, 270, 0):
        http("/api/config", {"rotation": deg})
        ok = wait(lambda: layout(screenshot(wayland)) != seen.get("last"), 15)
        seen[deg] = layout(screenshot(wayland))
        seen["last"] = seen[deg]
        check(f"rotation {deg}: kiosk alive", cage.poll() is None and ok, f"red half: {seen[deg]}")
    check("rotations give four orientations",
          sorted([seen[0], seen[90], seen[180], seen[270]]) == ["bottom", "left", "right", "top"], str(seen))
    err = http("/api/config")[1]["kiosk"]["error"]
    check("no kiosk error reported", not err, err)

    # taps must land where the turned page shows: at 90 degrees the red (top) half is on the right
    for n, (deg, fx, fy, want) in enumerate([(0, 0.5, 0.2, "green"), (0, 0.5, 0.8, "yellow"),
                                             (90, 0.85, 0.85, "green"), (90, 0.15, 0.85, "yellow"),
                                             (270, 0.15, 0.15, "green"), (270, 0.85, 0.15, "yellow")]):
        http("/api/config", {"rotation": deg, "tabs": [{"name": "Test", "url": f"http://127.0.0.1:{PAGE_PORT}/?t={n}"}]})
        fresh = wait(lambda: layout(screenshot(wayland)) == {0: "top", 90: "right", 270: "left"}[deg], 15)
        err = tap(wayland, fx, fy) if fresh else "page not shown"
        got = wait(lambda: color(screenshot(wayland), 0.5, 0.5) == want, 5)
        check(f"tap at {deg} degrees lands in the {'red' if want == 'green' else 'blue'} half", got,
              err or color(screenshot(wayland), 0.5, 0.5))
    # window.open from a page loads in the same tab (no new window in a kiosk, but not nothing either)
    http("/api/config", {"rotation": 0, "tabs": [{"name": "Test", "url": f"http://127.0.0.1:{PAGE_PORT}/open"}]})
    green = wait(lambda: color(screenshot(wayland), 0.5, 0.5) == "green", 15)
    err = tap(wayland, 0.5, 0.5) if green else "opener page not shown"
    check("window.open loads in the same tab", wait(lambda: layout(screenshot(wayland)) == "top", 10),
          err or layout(screenshot(wayland)))

    http("/api/config", {"rotation": 0, "tabs": [{"name": "Test", "url": f"http://127.0.0.1:{PAGE_PORT}/"}]})
    wait(lambda: layout(screenshot(wayland)) == "top", 15)

    http("/api/lock")
    check("lock screen covers the page", wait(lambda: color(screenshot(wayland), 0.5, 0.15) == "dark", 10))
    http("/api/unlock")
    check("unlock shows the page again", wait(lambda: layout(screenshot(wayland)) == "top", 10))

    # two tabs: the tab bar must appear and the kiosk must survive rebuilding all views
    http("/api/config", {"tabs": [{"name": "Eins", "url": f"http://127.0.0.1:{PAGE_PORT}/"},
                                  {"name": "Zwei", "url": f"http://127.0.0.1:{PAGE_PORT}/?2", "ha": True}],
                         "ha": {"theme": "dark"}})
    time.sleep(5)
    check("kiosk survives tab change + HA theme script", cage.poll() is None, tail("kiosk"))
    shot = screenshot(wayland)
    check("tab bar shown with two tabs", color(shot, 0.02, 0.5) == "dark" and color(shot, 0.6, 0.15) == "red",
          f"{color(shot, 0.02, 0.5)} / {color(shot, 0.6, 0.15)}")
    check("kiosk log has no panic", "panicked" not in (TMP / "kiosk.log").read_text(errors="replace"), tail("kiosk"))

    # crash guard: the 4th start within 3 minutes is a safe mode that the server and the health check see
    os.killpg(cage.pid, signal.SIGTERM)
    cage.wait(10)
    share = TMP / "share" / "walltablet"
    now = int(time.time())
    (share / "starts.json").write_text(json.dumps([now - 30, now - 20, now - 10]))
    cage = start("kiosk-safe", kiosk, env)
    safe = wait(lambda: http("/api/config")[1]["kiosk"]["safe"], 40)
    check("crash loop -> safe mode reported", safe, tail("kiosk-safe"))
    # past the 20 s start-up time, so only the safe mode can make health fail
    wait(lambda: http("/api/config")[1]["kiosk"]["up"] >= 21, 40)
    check("health 503 in safe mode", http("/api/health")[0] == 503)
    check("status: tablet error in safe mode", http("/api/status")[1]["devices"][0]["state"] == "error")
    check("next start renders in software", (share / "careful").exists())
    check("safe mode keeps running", cage.poll() is None, tail("kiosk-safe"))


def camera_checks():
    if not check("ffprobe installed", shutil.which("ffprobe")):
        return
    with socket.socket() as s:
        if s.connect_ex(("127.0.0.1", 8554)) == 0:
            check("RTSP port 8554 in use (installed camera?) - camera checks skipped", True)
            return
    cam = {"enabled": True, "source": "custom", "custom_source": "videotestsrc name=src is-live=true pattern=smpte",
           "encoder": "x264", "rotate": "90", "width": 640, "height": 360, "fps": 15, "bitrate": 800,
           "sub_enabled": True, "sub_width": 320, "sub_height": 180, "sub_bitrate": 200, "audio_enabled": False}
    code, _ = http("/api/config", {"camera": cam})
    check("camera config accepted", code == 200)
    up = wait(lambda: any("Kamera liefert Bilder" in l for l in http("/api/config")[1]["camera"]["log"]), 30)
    log = "\n".join(http("/api/config")[1]["camera"]["log"][-15:])
    if not check("camera delivers pictures", up, log):
        return

    def probe(path):
        r = subprocess.run(["ffprobe", "-v", "error", "-rtsp_transport", "tcp", "-show_entries",
                            "stream=codec_name,width,height,codec_type", "-of", "json", f"rtsp://127.0.0.1:8554/{path}"],
                           capture_output=True, text=True, timeout=30)
        return json.loads(r.stdout or "{}").get("streams", []), r.stderr

    streams, err = probe("stream")
    v = [s for s in streams if s.get("codec_type") == "video"]
    check("main stream H.264 360x640 (rotated)", v and v[0]["codec_name"] == "h264" and (v[0]["width"], v[0]["height"]) == (360, 640),
          f"{streams} {err}")
    streams, err = probe("sub")
    v = [s for s in streams if s.get("codec_type") == "video"]
    check("sub stream H.264 180x320 (rotated)", v and (v[0]["width"], v[0]["height"]) == (180, 320), f"{streams} {err}")

    code, body = http("/snapshot.jpg")
    check("snapshot is a JPEG", code == 200 and body[:2] == b"\xff\xd8", f"HTTP {code}")

    running_before = http("/api/config")[1]["camera"]["running"]
    starts_before = [l for l in http("/api/config")[1]["camera"]["log"] if "Aufnahme:" in l]
    rec = TMP / "rec.mp4"
    ff = subprocess.Popen(["ffmpeg", "-v", "error", "-rtsp_transport", "tcp", "-i", "rtsp://127.0.0.1:8554/stream",
                           "-t", "8", "-c", "copy", "-y", str(rec)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    time.sleep(2)
    code, _ = http("/api/config", {"camera": {"controls": {"pattern": 1}}})  # live control on the running source
    check("live control accepted", code == 200)
    ff.wait(30)
    check("8 s recording while changing a control", rec.exists() and rec.stat().st_size > 20000,
          f"{rec.stat().st_size if rec.exists() else 0} bytes; {ff.stderr.read().decode()[-300:]}")
    log = http("/api/config")[1]["camera"]["log"]
    check("control applied without restart", running_before and not any("Kamera-Prozess beendet" in l for l in log)
          and [l for l in log if "Aufnahme:" in l] == starts_before, "\n".join(log[-10:]))
    starts = sum("Aufnahme:" in l for l in log)
    pictures = sum("Kamera liefert Bilder" in l for l in log)
    http("/api/config", {"camera": {"controls": {}}})  # reset = remove: the source needs a restart for its default
    restarted = wait(lambda: sum("Aufnahme:" in l for l in http("/api/config")[1]["camera"]["log"]) > starts, 20)
    check("reset control restarts the camera", restarted)
    check("camera delivers pictures again",  # a new first frame, not the old process's snapshot
          wait(lambda: sum("Kamera liefert Bilder" in l for l in http("/api/config")[1]["camera"]["log"]) > pictures, 30))

    # ONVIF with a real client, if available
    try:
        from onvif import ONVIFCamera  # onvif-zeep
    except ImportError:
        check("ONVIF client (onvif-zeep) available - skipped", True, "pip install onvif-zeep to run it")
        return
    try:
        dev = ONVIFCamera("127.0.0.1", PORT, "user", "pass")
        info = dev.devicemgmt.GetDeviceInformation()
        media = dev.create_media_service()
        profiles = media.GetProfiles()
        uris = {}
        for p in profiles:
            req = media.create_type("GetStreamUri")
            req.ProfileToken = p.token
            req.StreamSetup = {"Stream": "RTP-Unicast", "Transport": {"Protocol": "RTSP"}}
            uris[p.token] = media.GetStreamUri(req).Uri
        snap = media.GetSnapshotUri({"ProfileToken": profiles[0].token}).Uri
        check("ONVIF via onvif-zeep", info.Manufacturer == "WallTabletSuite" and uris.get("main", "").endswith("/stream")
              and uris.get("sub", "").endswith("/sub") and snap.endswith("/snapshot.jpg"), f"{uris} {snap}")
    except Exception as e:
        check("ONVIF via onvif-zeep", False, f"{type(e).__name__}: {e}")


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    page = ThreadingHTTPServer(("127.0.0.1", PAGE_PORT), Page)
    threading.Thread(target=page.serve_forever, daemon=True).start()
    data = TMP / "data"
    data.mkdir()
    (data / "config.json").write_text(json.dumps({"tabs": [{"name": "Test", "url": f"http://127.0.0.1:{PAGE_PORT}/"}]}))
    start("server", [str(BIN / "wts-server")], {"WTS_DATA": str(data), "WTS_PORT": str(PORT)})
    try:
        if check("server answers", wait(lambda: http("/api/status")[0] == 200, 20), tail("server")):
            for part, run in (("kiosk", kiosk_checks), ("camera", camera_checks)):
                if which in ("all", part):
                    try:
                        run()
                    except Exception as e:  # e.g. grim fails after cage died: a FAIL, the other part still runs
                        check(f"{part} checks ran to the end", False, f"{type(e).__name__}: {e}")
    finally:
        stop_all()
        page.shutdown()
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} passed, logs in {TMP}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
