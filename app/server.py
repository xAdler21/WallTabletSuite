#!/usr/bin/env python3
"""WallTabletSuite server: web panel, lock API, kiosk config, camera supervisor, ONVIF, self-update.

No login, plain HTTP: the panel is meant for the local network only (see README).
"""
import hashlib
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlsplit

import onvif

APP = Path(__file__).resolve().parent
ROOT = APP.parent
DATA = Path(os.environ.get("WTS_DATA", ROOT / "data"))
PORT = int(os.environ.get("WTS_PORT", "8447"))
RTSP_PORT = 8554
VERSION = (ROOT / "VERSION").read_text().strip()
UPDATE_LOG = DATA / "update.log"
CHECK_EVERY = 6 * 3600

DEFAULTS = {
    "locked": False,
    "tabs": [{"name": "Home Assistant", "url": "http://homeassistant.local:8123/", "ha": True}],
    "ha": {"username": "", "password": ""},
    "camera": {
        "enabled": False, "name": "Wall Tablet", "source": "libcamera", "device": "/dev/video0",
        "custom_source": "", "width": 1280, "height": 720, "fps": 15, "bitrate": 2000, "encoder": "va",
    },
    "update": {"repo": "https://github.com/xAdler21/WallTabletSuite.git", "branch": "main", "auto": False},
}

lock = threading.Lock()  # guards cfg and kiosk
cfg = {}
kiosk = {"seen": 0.0, "tab": ""}


# ---------- config ----------

def load():
    try:
        saved = json.loads((DATA / "config.json").read_text())
    except (OSError, ValueError):
        saved = {}
    c = json.loads(json.dumps(DEFAULTS))
    for k, v in saved.items():
        if isinstance(c.get(k), dict) and isinstance(v, dict):
            c[k].update({kk: vv for kk, vv in v.items() if kk in c[k]})
        elif k in c:
            c[k] = v
    return c


def save():
    DATA.mkdir(parents=True, exist_ok=True)
    tmp = DATA / "config.json.tmp"
    tmp.write_text(json.dumps(cfg, indent=2, ensure_ascii=False))
    os.replace(tmp, DATA / "config.json")


def clean_tabs(tabs):
    if not isinstance(tabs, list) or len(tabs) > 20:
        raise ValueError("tabs: Liste mit höchstens 20 Einträgen")
    out = []
    for t in tabs:
        if not isinstance(t, dict):
            raise ValueError("tabs: Einträge brauchen name und url")
        name, url = str(t.get("name", "")).strip()[:40], str(t.get("url", "")).strip()
        if not name:
            raise ValueError("Ein Tab hat keinen Namen")
        if not re.fullmatch(r"https?://\S+", url):
            raise ValueError(f"{name}: Adresse muss mit http:// oder https:// anfangen")
        out.append({"name": name, "url": url, "ha": bool(t.get("ha"))})
    return out


def clean_ha(new, old):
    user, password = str(new.get("username", "")).strip()[:200], str(new.get("password", ""))[:200]
    if not user:
        return {"username": "", "password": ""}
    return {"username": user, "password": password or old["password"]}  # empty field = keep the stored password


def clean_camera(new, old):
    c = dict(old)
    for k, v in new.items():
        if k not in c:
            raise ValueError(f"camera.{k} gibt es nicht")
        c[k] = v
    c["enabled"] = bool(c["enabled"])
    c["name"] = str(c["name"]).strip()[:40] or "Wall Tablet"
    if c["source"] not in ("libcamera", "v4l2", "custom"):
        raise ValueError("Quelle: libcamera, v4l2 oder custom")
    if not re.fullmatch(r"/dev/video\d+", str(c["device"])):
        raise ValueError("Gerät muss /dev/videoN sein")
    c["custom_source"] = str(c["custom_source"]).strip()[:500]
    if c["source"] == "custom" and not c["custom_source"]:
        raise ValueError("Eigene Quelle ist leer")
    if c["encoder"] not in ("va", "x264"):
        raise ValueError("Encoder: va oder x264")
    for k, lo, hi in (("width", 160, 3840), ("height", 120, 2160), ("fps", 1, 60), ("bitrate", 100, 20000)):
        try:
            c[k] = int(c[k])
        except (TypeError, ValueError):
            raise ValueError(f"{k} muss eine Zahl sein") from None
        if not lo <= c[k] <= hi:
            raise ValueError(f"{k} muss zwischen {lo} und {hi} liegen")
    return c


def clean_update(new, old):
    c = {**old, **{k: v for k, v in new.items() if k in old}}
    c["repo"], c["branch"], c["auto"] = str(c["repo"]).strip(), str(c["branch"]).strip(), bool(c["auto"])
    if not re.fullmatch(r"https?://\S+", c["repo"]):
        raise ValueError("Repo-Link muss mit https:// anfangen")
    if not re.fullmatch(r"[A-Za-z0-9._][A-Za-z0-9._/-]*", c["branch"]):
        raise ValueError("Ungültiger Branch")
    return c


def set_locked(value):
    with lock:
        if cfg["locked"] != value:
            cfg["locked"] = value
            save()
    return {"locked": value}


# ---------- camera supervisor ----------

class Camera:
    """Runs camera.py as a child process (GStreamer crashes stay out of the panel) and restarts it."""

    def __init__(self):
        self.proc, self.gen, self.clients = None, 0, 0
        self.log = deque(maxlen=60)
        self.mutex = threading.Lock()

    def apply(self, c):
        with self.mutex:
            self.gen += 1
            if self.proc and self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(5)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
            self.proc, self.clients = None, 0
            if c["enabled"]:
                self._start(c, self.gen)

    def _start(self, c, gen):
        env = {**os.environ, "GST_DEBUG": "2", "GST_DEBUG_NO_COLOR": "1"}
        self.proc = subprocess.Popen(
            [sys.executable, str(APP / "camera.py"), json.dumps(c), str(RTSP_PORT)],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
        threading.Thread(target=self._read, args=(self.proc, c, gen), daemon=True).start()

    def _read(self, proc, c, gen):
        for line in proc.stdout:
            line = line.rstrip()
            if line.startswith("CLIENTS "):
                self.clients = int(line.split()[1])
            else:
                self.log.append(time.strftime("%H:%M:%S ") + line)
        code = proc.wait()
        if gen != self.gen:
            return  # stopped on purpose
        self.log.append(time.strftime("%H:%M:%S ") + f"Kamera-Prozess beendet (Code {code}), neuer Versuch in 10 s")
        time.sleep(10)
        with self.mutex:
            if gen == self.gen:
                self._start(c, gen)

    def running(self):
        return self.proc is not None and self.proc.poll() is None


camera = Camera()


# ---------- self-update ----------

def newer(remote, local):
    """True if version string remote is higher than local (1.10.0 > 1.9.2)."""
    # shortcut: compares the numbers only, so 1.0.0-alpha.5 counts as 1.0.0.5; fine while no pre-releases are used
    def nums(v):
        return [int(n) for n in re.findall(r"\d+", v)]
    return bool(nums(remote)) and nums(remote) > nums(local)


class Updater:
    def __init__(self):
        self.info = {"checked": None, "remote_version": None, "available": False, "changes": [], "error": None}

    def git(self, *args):
        return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True,
                              timeout=120, check=True).stdout.strip()

    def check(self):
        with lock:
            u = dict(cfg["update"])
        try:
            self.git("fetch", "--quiet", u["repo"], u["branch"])
            try:
                remote_version = self.git("show", "FETCH_HEAD:VERSION")
            except subprocess.CalledProcessError:
                remote_version = "?"
            available = newer(remote_version, VERSION)
            self.info.update(
                remote_version=remote_version, available=available,
                changes=self.git("log", "--format=%s", "-n", "20", "HEAD..FETCH_HEAD").splitlines() if available else [],
                error=None)
        except subprocess.CalledProcessError as e:
            self.info["error"] = (e.stderr or str(e)).strip()[-400:]
        except (OSError, subprocess.SubprocessError) as e:
            self.info["error"] = str(e)
        self.info["checked"] = time.time()
        return self.info

    def install(self):
        # update.sh runs in its own unit: it restarts this server and must outlive it
        r = subprocess.run(["sudo", "-n", "/usr/bin/systemctl", "start", "--no-block", "wts-update.service"],
                           capture_output=True, text=True, timeout=15)
        if r.returncode:
            raise RuntimeError(r.stderr.strip() or "systemctl start wts-update.service fehlgeschlagen")

    def log(self):
        try:
            return UPDATE_LOG.read_text(errors="replace").splitlines()[-40:]
        except OSError:
            return []

    def loop(self):
        time.sleep(120)
        while True:
            self.check()
            with lock:
                auto = cfg["update"]["auto"]
            if auto and self.info["available"] and not self.info["error"]:
                try:
                    self.install()
                except (OSError, RuntimeError, subprocess.SubprocessError) as e:
                    self.info["error"] = str(e)
            time.sleep(CHECK_EVERY)


updater = Updater()


# ---------- Home Assistant login ----------

HA_FILE = DATA / "ha_tokens.json"
INSECURE = ssl._create_unverified_context()  # local HA with a self-signed certificate, like the kiosk


def origin(url):
    u = urlsplit(url)
    host = u.netloc.removesuffix(":80") if u.scheme == "http" else u.netloc.removesuffix(":443")
    return f"{u.scheme}://{host}"


def ha_login(base, user, password):
    """Home Assistant's own login flow (what its login page does) -> tokens in the format the HA
    frontend keeps in localStorage.hassTokens. Raises ValueError when HA refuses."""
    client_id = base + "/"  # same client id as the frontend, so it may refresh these tokens itself

    def post(path, data, form=False):
        body = urlencode(data).encode() if form else json.dumps(data).encode()
        ctype = "application/x-www-form-urlencoded" if form else "application/json"
        req = urllib.request.Request(base + path, body, {"Content-Type": ctype})
        try:
            with urllib.request.urlopen(req, timeout=10, context=INSECURE) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            raise ValueError(f"HTTP {e.code} auf {path}") from None

    flow = post("/auth/login_flow", {"client_id": client_id, "handler": ["homeassistant", None],
                                     "redirect_uri": base + "/?auth_callback=1"})
    step = post(f"/auth/login_flow/{flow['flow_id']}",
                {"client_id": client_id, "username": user, "password": password})
    if step.get("type") != "create_entry":
        why = (step.get("errors") or {}).get("base") or step.get("step_id") or step.get("reason") or step.get("type")
        raise ValueError({"invalid_auth": "Benutzername oder Passwort falsch",
                          "mfa": "Zwei-Faktor-Anmeldung aktiv, eigenen Benutzer ohne MFA anlegen"}
                         .get(why, f"Anmeldung abgelehnt ({why})"))
    t = post("/auth/token", {"grant_type": "authorization_code", "code": step["result"], "client_id": client_id},
             form=True)
    return {"hassUrl": base, "clientId": client_id, "access_token": t["access_token"],
            "refresh_token": t["refresh_token"], "token_type": t["token_type"], "expires_in": t["expires_in"],
            "expires": int(time.time() * 1000) + t["expires_in"] * 1000}


class HaLogins:
    """Logs in once per HA address and keeps the tokens (also across restarts): the HA frontend refreshes
    the access token on its own, so a new login is only needed when the credentials change."""

    def __init__(self):
        self.mutex = threading.Lock()
        try:
            saved = json.loads(HA_FILE.read_text())
            self.key, self.tokens = saved["key"], saved["tokens"]
        except (OSError, ValueError, KeyError):
            self.key, self.tokens = None, {}
        self.state = {base: "angemeldet" for base in self.tokens}

    def get(self, base, user, password):
        """Tokens for base, or None while the login runs or after HA refused it."""
        key = hashlib.sha256(f"{user}\0{password}".encode()).hexdigest()
        with self.mutex:
            if key != self.key:
                self.key, self.tokens, self.state = key, {}, {}
                self._save()
            if base in self.tokens:
                return self.tokens[base]
            if base not in self.state:
                self.state[base] = "Anmeldung läuft"
                threading.Thread(target=self._login, args=(base, user, password, key), daemon=True).start()
        return None

    def _login(self, base, user, password, key):
        retry = False
        try:
            tokens, error = ha_login(base, user, password), None
        except ValueError as e:  # refused: not retried until the credentials change, HA bans IPs after failures
            tokens, error = None, str(e)
        except (OSError, KeyError, TypeError) as e:  # HA unreachable or restarting
            tokens, error, retry = None, f"nicht erreichbar ({e}), neuer Versuch in 30 s", True
        with self.mutex:
            if key != self.key:
                return
            if tokens:
                self.tokens[base], self.state[base] = tokens, "angemeldet"
                self._save()
            else:
                self.state[base] = error
        if retry:
            time.sleep(30)
            with self.mutex:
                if key == self.key and self.state.get(base) == error:
                    del self.state[base]  # the next kiosk poll starts a new login

    def _save(self):
        DATA.mkdir(parents=True, exist_ok=True)
        HA_FILE.write_text(json.dumps({"key": self.key, "tokens": self.tokens}))


ha_logins = HaLogins()


# ---------- status ----------

def kiosk_online():
    return time.time() - kiosk["seen"] < 10


def status():
    with lock:
        locked, cam = cfg["locked"], dict(cfg["camera"])
        tab = kiosk["tab"]
    devices = [{"name": socket.gethostname(), "kind": "Tablet",
                "state": "ok" if kiosk_online() else "offline",
                "detail": "gesperrt" if locked else (f"Tab {tab}" if tab else "")}]
    if not cam["enabled"]:
        devices.append({"name": "Kamera", "kind": "Kamera", "state": "ok", "detail": "aus"})
    elif camera.running():
        devices.append({"name": "Kamera", "kind": "Kamera", "state": "ok",
                        "detail": f"RTSP, {camera.clients} Client(s)"})
    else:
        devices.append({"name": "Kamera", "kind": "Kamera", "state": "error",
                        "detail": camera.log[-1] if camera.log else "läuft nicht"})
    return {"version": VERSION, "locked": locked, "devices": devices}


def onvif_ctx(ip, port):
    with lock:
        cam = dict(cfg["camera"])
    return {"ip": ip, "port": port, "rtsp_port": RTSP_PORT, "camera": cam, "version": VERSION,
            "hostname": socket.gethostname()}


# ---------- HTTP ----------

class Handler(BaseHTTPRequestHandler):
    server_version = "WallTabletSuite/" + VERSION
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, code, body, ctype="application/json; charset=utf-8"):
        if not isinstance(body, (bytes, str)):
            body = json.dumps(body, ensure_ascii=False)
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n > 1_000_000:
            raise ValueError("Anfrage zu groß")
        return self.rfile.read(n) if n else b""

    def body_json(self):
        raw = self.body()
        try:
            data = json.loads(raw) if raw.strip() else {}
        except ValueError:
            raise ValueError("Kein gültiges JSON") from None
        if not isinstance(data, dict):
            raise ValueError("JSON-Objekt erwartet")
        return data

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def route(self, method):
        url = urlsplit(self.path)
        path = url.path.rstrip("/") or "/"
        try:
            if path.startswith("/onvif/") and method == "POST":
                ip, port = self.connection.getsockname()[:2]
                code, xml = onvif.handle(self.body(), onvif_ctx(ip, port))
                return self.send(code, xml, "application/soap+xml; charset=utf-8")
            handler = ROUTES.get((method, path))
            if not handler:
                return self.send(404, {"error": "nicht gefunden"})
            handler(self, parse_qs(url.query))
        except ValueError as e:
            self.send(400, {"error": str(e)})
        except Exception as e:  # report instead of dropping the connection
            self.send(500, {"error": f"{type(e).__name__}: {e}"})

    # --- routes ---

    def panel(self, q):
        self.send(200, (APP / "panel.html").read_bytes(), "text/html; charset=utf-8")

    def api_status(self, q):
        self.send(200, status())

    def api_health(self, q):
        # used by update.sh: the new version counts as good once the server answers and the kiosk polls
        ok = kiosk_online()
        self.send(200 if ok else 503, {"ok": ok})

    def api_kiosk(self, q):
        with lock:
            kiosk["seen"], kiosk["tab"] = time.time(), q.get("tab", [""])[0][:40]
            data = {"locked": cfg["locked"], "tabs": [dict(t) for t in cfg["tabs"]]}
            ha = dict(cfg["ha"])
        # HA tokens only for the kiosk on this machine: the API has no login
        if self.client_address[0] in ("127.0.0.1", "::1") and ha["username"] and ha["password"]:
            for t in data["tabs"]:
                tokens = t.get("ha") and ha_logins.get(origin(t["url"]), ha["username"], ha["password"])
                if tokens:
                    t["ha_tokens"] = tokens
        self.send(200, data)

    def api_lock(self, q):
        locked = True
        if self.command == "POST":
            locked = self.body_json().get("locked", True)
            if not isinstance(locked, bool):
                raise ValueError("locked muss true oder false sein")
        self.send(200, set_locked(locked))

    def api_unlock(self, q):
        if self.command == "POST":
            self.body()
        self.send(200, set_locked(False))

    def api_config(self, q):
        host = urlsplit("//" + (self.headers.get("Host") or "localhost")).hostname or "localhost"
        host = f"[{host}]" if ":" in host else host
        with lock:
            c = json.loads(json.dumps(cfg))
            tab = kiosk["tab"]
        has_password = bool(c["ha"].pop("password"))  # never sent out, the API has no login
        self.send(200, {
            "config": c, "version": VERSION,
            "ha": {"has_password": has_password, "state": dict(ha_logins.state)},
            "kiosk": {"online": kiosk_online(), "tab": tab},
            "camera": {"running": camera.running(), "clients": camera.clients, "log": list(camera.log),
                       "rtsp": f"rtsp://{host}:{RTSP_PORT}/stream",
                       "onvif": f"http://{host}:{PORT}/onvif/device_service"},
            "update": {**updater.info, "log": updater.log()},
        })

    def api_config_save(self, q):
        data = self.body_json()
        restart_camera = None
        with lock:
            new = dict(cfg)
            if "tabs" in data:
                new["tabs"] = clean_tabs(data["tabs"])
            if "camera" in data:
                if not isinstance(data["camera"], dict):
                    raise ValueError("camera muss ein Objekt sein")
                new["camera"] = clean_camera(data["camera"], cfg["camera"])
                if new["camera"] != cfg["camera"]:
                    restart_camera = new["camera"]
            if "update" in data:
                if not isinstance(data["update"], dict):
                    raise ValueError("update muss ein Objekt sein")
                new["update"] = clean_update(data["update"], cfg["update"])
            if "ha" in data:
                if not isinstance(data["ha"], dict):
                    raise ValueError("ha muss ein Objekt sein")
                new["ha"] = clean_ha(data["ha"], cfg["ha"])
            cfg.update(new)
            save()
        if restart_camera:
            camera.apply(restart_camera)
        self.send(200, {"ok": True})

    def api_update_check(self, q):
        self.body()
        self.send(200, updater.check())

    def api_update_install(self, q):
        self.body()
        updater.install()
        self.send(200, {"ok": True})


ROUTES = {
    ("GET", "/"): Handler.panel,
    ("GET", "/api/status"): Handler.api_status,
    ("GET", "/api/health"): Handler.api_health,
    ("GET", "/api/kiosk"): Handler.api_kiosk,
    ("GET", "/api/lock"): Handler.api_lock,
    ("POST", "/api/lock"): Handler.api_lock,
    ("GET", "/api/unlock"): Handler.api_unlock,
    ("POST", "/api/unlock"): Handler.api_unlock,
    ("GET", "/api/config"): Handler.api_config,
    ("POST", "/api/config"): Handler.api_config_save,
    ("POST", "/api/update/check"): Handler.api_update_check,
    ("POST", "/api/update/install"): Handler.api_update_install,
}


def serve(port):
    server = ThreadingHTTPServer(("", port), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    os.umask(0o077)  # config.json holds the HA password, ha_tokens.json its tokens
    cfg.update(load())
    save()
    serve(PORT)
    onvif_port = PORT
    try:
        serve(80)  # some NVRs only try port 80 when a camera is added by IP
        onvif_port = 80
    except OSError as e:
        print(f"Port 80 nicht verfügbar ({e}), ONVIF nur auf {PORT}", flush=True)
    camera.apply(cfg["camera"])
    threading.Thread(target=updater.loop, daemon=True).start()
    threading.Thread(target=onvif.discovery, args=(lambda ip: onvif_ctx(ip, onvif_port),), daemon=True).start()
    print(f"WallTabletSuite {VERSION} auf http://0.0.0.0:{PORT}", flush=True)
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
