"""Self-check for the server: lock API, config validation, ONVIF/WS-Discovery replies.

Run: python -m unittest discover tests   (needs no GTK/GStreamer)
"""
import json
import os
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
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
        tabs = [{"name": "HA", "url": "http://ha:8123/"}, {"name": "Beamer", "url": "https://vm:8445/"}]
        self.assertEqual(self.json("/api/config", {"tabs": tabs})[0], 200)
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
