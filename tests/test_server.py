"""Self-check for the server: lock API, config validation, ONVIF/WS-Discovery replies.

Run: python -m unittest discover tests   (needs no GTK/GStreamer)
"""
import json
import os
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

os.environ["WTS_DATA"] = tempfile.mkdtemp()
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "app"))
import onvif  # noqa: E402
import server  # noqa: E402

SOAP = ('<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header/><s:Body>'
        '<{a} xmlns="http://www.onvif.org/ver10/{svc}/wsdl"/></s:Body></s:Envelope>')


class ServerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        server.cfg.update(server.load())
        server.save()
        cls.http = server.serve(0)
        cls.base = f"http://127.0.0.1:{cls.http.server_address[1]}"

    @classmethod
    def tearDownClass(cls):
        cls.http.shutdown()

    def req(self, path, body=None, raw=None):
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        try:
            with urllib.request.urlopen(urllib.request.Request(self.base + path, data=data), timeout=5) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def json(self, path, body=None):
        code, data = self.req(path, body)
        return code, json.loads(data)

    def test_lock_get_post_and_persist(self):
        self.assertEqual(self.json("/api/lock"), (200, {"locked": True}))
        self.assertTrue(self.json("/api/status")[1]["locked"])
        self.assertTrue(json.loads((server.DATA / "config.json").read_text())["locked"])
        self.assertEqual(self.json("/api/unlock"), (200, {"locked": False}))
        self.assertEqual(self.json("/api/lock", {"locked": True}), (200, {"locked": True}))
        self.assertEqual(self.json("/api/lock", {"locked": False}), (200, {"locked": False}))
        self.assertEqual(self.json("/api/lock", {"locked": "ja"})[0], 400)
        self.assertEqual(self.req("/api/lock", raw=b"{kaputt")[0], 400)

    def test_status_contract(self):
        s = self.json("/api/status")[1]
        self.assertEqual(s["version"], server.VERSION)
        self.assertIn("locked", s)
        for d in s["devices"]:
            self.assertIn(d["state"], ("ok", "warn", "error", "offline"))

    def test_tabs_and_kiosk(self):
        self.assertEqual(self.json("/api/config", {"tabs": [{"name": "HA", "url": "javascript:alert(1)"}]})[0], 400)
        self.assertEqual(self.json("/api/config", {"tabs": [{"name": "", "url": "http://x"}]})[0], 400)
        tabs = [{"name": "HA", "url": "http://ha:8123/", "ha": False}, {"name": "Beamer", "url": "https://vm:8445/", "ha": False}]
        self.assertEqual(self.json("/api/config", {"tabs": [{"name": "HA", "url": "http://ha:8123/"}, tabs[1]]})[0], 200)
        server.kiosk["seen"] = 0  # other tests poll /api/kiosk too
        self.assertEqual(self.req("/api/health")[0], 503)
        self.assertEqual(self.json("/api/kiosk?tab=HA")[1]["tabs"], tabs)
        self.assertEqual(self.req("/api/health")[0], 200)

    def test_camera_and_update_validation(self):
        self.assertEqual(self.json("/api/config", {"camera": {"fps": 500}})[0], 400)
        self.assertEqual(self.json("/api/config", {"camera": {"device": "/etc/passwd"}})[0], 400)
        self.assertEqual(self.json("/api/config", {"camera": {"bogus": 1}})[0], 400)
        self.assertEqual(self.json("/api/config", {"update": {"repo": "--upload-pack=x"}})[0], 400)
        self.assertEqual(self.json("/api/config", {"update": {"branch": "-x"}})[0], 400)
        self.assertEqual(self.json("/api/config", {"camera": {"fps": "20", "name": "Flur"}})[0], 200)
        self.assertEqual(server.cfg["camera"]["fps"], 20)

    def test_onvif(self):
        self.assertEqual(self.req("/onvif/media_service", raw=SOAP.format(a="GetProfiles", svc="media").encode())[0], 503)
        server.cfg["camera"]["enabled"] = True
        try:
            for action, svc in [("GetSystemDateAndTime", "device"), ("GetDeviceInformation", "device"),
                                ("GetCapabilities", "device"), ("GetServices", "device"), ("GetScopes", "device"),
                                ("GetHostname", "device"), ("GetNetworkInterfaces", "device"),
                                ("GetProfiles", "media"), ("GetProfile", "media"), ("GetVideoSources", "media"),
                                ("GetVideoSourceConfigurations", "media"), ("GetVideoEncoderConfigurations", "media"),
                                ("GetVideoEncoderConfiguration", "media"), ("GetStreamUri", "media")]:
                code, body = self.req(f"/onvif/{svc}_service", raw=SOAP.format(a=action, svc=svc).encode())
                self.assertEqual(code, 200, action)
                root = ET.fromstring(body)  # well-formed, namespaces declared
                self.assertTrue(any(e.tag.endswith(action + "Response") for e in root.iter()), action)
            uri = next(e.text for e in root.iter() if e.tag.endswith("}Uri"))
            self.assertEqual(uri, f"rtsp://127.0.0.1:{server.RTSP_PORT}/stream")
            code, body = self.req("/onvif/device_service", raw=SOAP.format(a="SetHostname", svc="device").encode())
            self.assertEqual(code, 500)
            self.assertIn(b"ActionNotSupported", body)
            self.assertEqual(self.req("/onvif/device_service", raw=b"not xml")[0], 400)
        finally:
            server.cfg["camera"]["enabled"] = False

    def test_ha_login(self):
        class FakeHa(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"])).decode()
                if self.path == "/auth/login_flow":
                    out = {"flow_id": "f1", "type": "form"}
                elif self.path == "/auth/login_flow/f1":
                    ok = json.loads(body)["password"] == "geheim"
                    out = {"type": "create_entry", "result": "code1"} if ok else \
                        {"type": "form", "errors": {"base": "invalid_auth"}}
                else:
                    self.server.token_requests.append(body)
                    out = {"access_token": "a", "refresh_token": "r", "token_type": "Bearer", "expires_in": 1800}
                data = json.dumps(out).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        ha = ThreadingHTTPServer(("127.0.0.1", 0), FakeHa)
        ha.token_requests = []
        threading.Thread(target=ha.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{ha.server_address[1]}"
        try:
            self.json("/api/config", {"tabs": [{"name": "HA", "url": base + "/lovelace/0", "ha": True},
                                               {"name": "Beamer", "url": "http://vm:8445/"}],
                                      "ha": {"username": "kiosk", "password": "falsch"}})
            self.assertNotIn("password", self.json("/api/config")[1]["config"]["ha"])
            for password, expect in (("falsch", None), ("geheim", "r")):
                if password == "geheim":
                    self.json("/api/config", {"ha": {"username": "kiosk", "password": "geheim"}})
                for _ in range(50):
                    tabs = self.json("/api/kiosk")[1]["tabs"]
                    if server.ha_logins.state.get(base) not in (None, "Anmeldung läuft"):
                        break
                    time.sleep(0.05)
                self.assertEqual(tabs[0].get("ha_tokens", {}).get("refresh_token"), expect, password)
                self.assertNotIn("ha_tokens", tabs[1])
            self.assertEqual(server.ha_logins.state[base], "angemeldet")
            self.assertEqual(tabs[0]["ha_tokens"]["hassUrl"], base)
            self.assertEqual(tabs[0]["ha_tokens"]["clientId"], base + "/")
            self.assertIn("code=code1", ha.token_requests[0])
            # password field left empty keeps the stored password and the login
            self.json("/api/config", {"ha": {"username": "kiosk", "password": ""}})
            self.assertEqual(self.json("/api/kiosk")[1]["tabs"][0]["ha_tokens"]["refresh_token"], "r")
            self.assertEqual(len(ha.token_requests), 1)
        finally:
            ha.shutdown()
            self.json("/api/config", {"ha": {"username": ""}})

    def test_origin(self):
        self.assertEqual(server.origin("http://ha:80/lovelace"), "http://ha")
        self.assertEqual(server.origin("https://ha:8123/x?y"), "https://ha:8123")

    def test_version_compare(self):
        self.assertTrue(server.newer("0.1.10", "0.1.9"))
        self.assertTrue(server.newer("1.0.0", "0.9.9"))
        self.assertFalse(server.newer("0.1.0", "0.1.0"))
        self.assertFalse(server.newer("0.1.0", "0.2.0"))
        self.assertFalse(server.newer("?", "0.1.0"))

    def test_discovery_probe(self):
        probe = (b'<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" '
                 b'xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" '
                 b'xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"><s:Header>'
                 b'<a:MessageID>uuid:1234</a:MessageID></s:Header><s:Body><d:Probe><d:Types>'
                 b'dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>')
        ctx = server.onvif_ctx("10.0.0.5", 80)
        root = ET.fromstring(onvif.probe_match(probe, ctx))
        texts = {e.tag.rsplit("}", 1)[-1]: e.text for e in root.iter()}
        self.assertEqual(texts["RelatesTo"], "uuid:1234")
        self.assertEqual(texts["XAddrs"], "http://10.0.0.5:80/onvif/device_service")
        self.assertIsNone(onvif.probe_match(onvif.probe_match(probe, ctx), ctx))  # own ProbeMatches is no Probe


if __name__ == "__main__":
    unittest.main()
