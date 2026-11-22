#!/usr/bin/env python3
"""RTSP server for the front camera. server.py starts it as: camera.py '<camera config json>' <port>.

Prints plain log lines; "CLIENTS n" lines report the number of connected RTSP clients.
"""
import json
import sys

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtspServer", "1.0")
from gi.repository import GLib, Gst, GstRtspServer  # noqa: E402

SOURCES = {"libcamera": "libcamerasrc", "v4l2": "v4l2src"}
ENCODERS = {"va": "vah264enc", "x264": "x264enc"}


def source(c):
    if c["source"] == "custom":
        return c["custom_source"]
    if c["source"] == "v4l2":
        return f"v4l2src device={c['device']} ! decodebin"  # decodebin: raw and MJPEG webcams
    return "libcamerasrc"  # the MIIX 520 front camera sits behind Intel IPU3, only libcamera reads it


def chain(c):
    """Source up to H.264 (without payloader)."""
    w, h, fps, br = c["width"], c["height"], c["fps"], c["bitrate"]
    enc = {"va": f"vah264enc rate-control=cbr bitrate={br} key-int-max={fps * 2}",
           "x264": f"x264enc tune=zerolatency speed-preset=ultrafast bitrate={br} key-int-max={fps * 2}"}[c["encoder"]]
    return (f"{source(c)} ! videoconvert ! videoscale ! videorate ! "
            f"video/x-raw,width={w},height={h},framerate={fps}/1 ! videoconvert ! {enc} ! h264parse")


def self_test(c):
    """Opens the camera for a moment so errors show up in the panel before the NVR connects."""
    pipe = Gst.parse_launch(f"{chain(c)} ! fakesink")
    pipe.set_state(Gst.State.PLAYING)
    msg = pipe.get_bus().timed_pop_filtered(10 * Gst.SECOND, Gst.MessageType.ASYNC_DONE | Gst.MessageType.ERROR)
    pipe.set_state(Gst.State.NULL)
    if msg is None:
        return "Kamera-Test: keine Bilder nach 10 s"
    if msg.type == Gst.MessageType.ERROR:
        err, dbg = msg.parse_error()
        return f"Kamera-Test fehlgeschlagen: {err.message} ({dbg})"
    return None


def main():
    c, port = json.loads(sys.argv[1]), sys.argv[2]
    Gst.init(None)
    missing = [e for e in (ENCODERS[c["encoder"]], SOURCES.get(c["source"])) if e and not Gst.ElementFactory.find(e)]
    if missing:
        sys.exit(f"GStreamer-Element fehlt: {', '.join(missing)} (Paket installiert? Anderen Encoder/Quelle wählen)")

    launch = f"( {chain(c)} ! rtph264pay name=pay0 pt=96 config-interval=1 )"
    print(f"Pipeline: {launch}", flush=True)
    try:
        err = self_test(c)
    except GLib.Error as e:
        err = f"Pipeline ungültig: {e.message}"
    if err:
        sys.exit(err)
    print("Kamera-Test ok", flush=True)

    server = GstRtspServer.RTSPServer()
    server.set_service(port)
    factory = GstRtspServer.RTSPMediaFactory()
    factory.set_launch(launch)
    factory.set_shared(True)  # one camera pipeline for all clients

    def configure(_factory, media):
        print("Stream startet", flush=True)
        media.connect("unprepared", lambda _m: print("Stream beendet", flush=True))

    factory.connect("media-configure", configure)
    server.get_mount_points().add_factory("/stream", factory)

    clients = [0]

    def connected(_server, client):
        clients[0] += 1
        print(f"CLIENTS {clients[0]}", flush=True)

        def closed(_client):
            clients[0] -= 1
            print(f"CLIENTS {clients[0]}", flush=True)

        client.connect("closed", closed)

    server.connect("client-connected", connected)
    if server.attach(None) == 0:
        sys.exit(f"RTSP-Port {port} belegt")
    print(f"RTSP bereit: rtsp://<ip>:{port}/stream", flush=True)
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
