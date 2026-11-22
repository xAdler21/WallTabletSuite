#!/usr/bin/env python3
"""Kiosk shell: a tab bar, one WebKit view per tab (kept alive while switching) and the lock screen.

Tabs and lock state come from server.py (polled every second); the kiosk keeps the last state while
the server restarts.
"""
import json
import os
import socket
import threading
import time
import urllib.parse
import urllib.request
from pathlib import Path

import gi

# Ubuntu blocks unprivileged user namespaces for programs without an AppArmor profile (python3 here), so
# WebKit's bubblewrap sandbox cannot start its web processes and the kiosk aborts. Only local pages are shown.
os.environ.setdefault("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1")
gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("WebKit", "6.0")
from gi.repository import Gdk, GLib, Gtk, WebKit  # noqa: E402

SERVER = os.environ.get("WTS_SERVER", "http://127.0.0.1:8447")
DATA = Path(GLib.get_user_data_dir()) / "walltablet"

CSS = """
window, .bar, .empty { background: #101214; color: #e8e8e8; }
.bar { padding: 10px 8px; border-right: 1px solid #22262a; }
.bar button { background: none; box-shadow: none; border: none; border-radius: 14px; color: #b8bcc2;
              font-size: 17px; padding: 20px 14px; margin: 3px 0; min-width: 120px; }
.bar button.active { background: #2f6fd0; color: white; }
.lock { background: rgba(6, 8, 10, 0.94); }
.lock-icon { font-size: 90px; }
.clock { font-size: 72px; font-weight: 300; color: white; }
.lock-text { font-size: 22px; color: #9aa0a6; }
.empty { font-size: 22px; }
"""


# Logs the HA frontend in with the tokens server.py got: written to localStorage before HA's own scripts run
HA_LOGIN = """(function () {
  var tokens = %s;
  if (location.origin !== tokens.hassUrl) return;
  var stored = null;
  try { stored = JSON.parse(localStorage.getItem("hassTokens")); } catch (e) {}
  if (stored && stored.refresh_token === tokens.refresh_token) return;
  try { localStorage.setItem("hassTokens", JSON.stringify(tokens)); } catch (e) {}
})();"""


def local_ip():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect(("10.255.255.255", 9))  # no packet is sent
            return s.getsockname()[0]
    except OSError:
        return socket.gethostname()


class Kiosk(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="de.walltablet.Kiosk")
        self.tabs, self.locked, self.views, self.buttons = None, None, [], []
        self.current, self.current_name = 0, ""

    def do_activate(self):
        css = Gtk.CssProvider()
        css.load_from_string(CSS)
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        DATA.mkdir(parents=True, exist_ok=True)
        # one persistent session: logins (Home Assistant token, cookies) survive restarts
        self.session = WebKit.NetworkSession.new(str(DATA / "data"), str(DATA / "cache"))
        self.session.get_cookie_manager().set_persistent_storage(str(DATA / "cookies.sqlite"),
                                                                 WebKit.CookiePersistentStorage.SQLITE)
        self.session.set_tls_errors_policy(WebKit.TLSErrorsPolicy.IGNORE)  # local devices, self-signed certs

        self.bar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, css_classes=["bar"])
        self.stack = Gtk.Stack(hexpand=True, vexpand=True)
        self.main = Gtk.Box()
        self.main.append(self.bar)
        self.main.append(self.stack)

        self.clock = Gtk.Label(css_classes=["clock"])
        inner = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12,
                        halign=Gtk.Align.CENTER, valign=Gtk.Align.CENTER)
        inner.append(Gtk.Label(label="🔒", css_classes=["lock-icon"]))
        inner.append(self.clock)
        inner.append(Gtk.Label(label="Gesperrt", css_classes=["lock-text"]))
        self.lock = Gtk.Box(css_classes=["lock"], visible=False)  # fills the overlay, so it takes every touch
        self.lock.append(inner)
        inner.set_hexpand(True)

        overlay = Gtk.Overlay(child=self.main)
        overlay.add_overlay(self.lock)
        self.build([])
        self.empty.set_label("Verbinde mit dem Server …")

        win = Gtk.ApplicationWindow(application=self, child=overlay, title="WallTabletSuite")
        win.fullscreen()
        win.present()
        self.tick()
        GLib.timeout_add_seconds(10, self.tick)
        threading.Thread(target=self.poll, daemon=True).start()

    def tick(self):
        self.clock.set_label(GLib.DateTime.new_now_local().format("%H:%M"))
        return True

    def poll(self):
        while True:
            try:
                url = f"{SERVER}/api/kiosk?tab={urllib.parse.quote(self.current_name)}"
                with urllib.request.urlopen(url, timeout=3) as r:
                    data = json.load(r)
                GLib.idle_add(self.apply, data)
            except Exception:
                pass  # server restarting: keep the last state (also the lock)
            time.sleep(1)

    def apply(self, data):
        if data["tabs"] != self.tabs:
            self.build(data["tabs"])
        if data["locked"] != self.locked:
            self.locked = data["locked"]
            self.lock.set_visible(self.locked)
            self.main.set_sensitive(not self.locked)  # also blocks the keyboard
        return False

    def build(self, tabs):
        for box in (self.bar, self.stack):
            child = box.get_first_child()
            while child:
                nxt = child.get_next_sibling()
                box.remove(child)
                child = nxt
        self.tabs, self.views, self.buttons = tabs, [], []
        self.empty = Gtk.Label(label=f"Keine Tabs eingerichtet.\nWeb-Panel: http://{local_ip()}/",
                               css_classes=["empty"], justify=Gtk.Justification.CENTER)
        self.stack.add_named(self.empty, "empty")
        for i, tab in enumerate(tabs):
            scripts = WebKit.UserContentManager()
            if tab.get("ha_tokens"):
                scripts.add_script(WebKit.UserScript.new(
                    HA_LOGIN % json.dumps(tab["ha_tokens"]), WebKit.UserContentInjectedFrames.TOP_FRAME,
                    WebKit.UserScriptInjectionTime.START, None, None))
            view = WebKit.WebView(network_session=self.session, user_content_manager=scripts,
                                  website_policies=WebKit.WebsitePolicies(autoplay=WebKit.AutoplayPolicy.ALLOW))
            view.connect("load-failed", self.on_load_failed)
            view.connect("context-menu", lambda *_: True)
            view.connect("permission-request", self.on_permission)
            view.connect("create", self.on_new_window)
            view.load_uri(tab["url"])
            self.stack.add_named(view, str(i))
            self.views.append(view)
            button = Gtk.Button(label=tab["name"])
            button.connect("clicked", self.on_tab, i)
            self.bar.append(button)
            self.buttons.append(button)
        self.bar.set_visible(len(tabs) > 1)
        if tabs:
            self.show(min(self.current, len(tabs) - 1))
        else:
            self.current_name = ""
            self.stack.set_visible_child_name("empty")

    def show(self, i):
        self.current, self.current_name = i, self.tabs[i]["name"]
        self.stack.set_visible_child_name(str(i))
        for j, button in enumerate(self.buttons):
            (button.add_css_class if j == i else button.remove_css_class)("active")

    def on_tab(self, _button, i):
        if i == self.current:
            self.views[i].load_uri(self.tabs[i]["url"])  # tapping the open tab again goes back to its start page
        self.show(i)

    def on_load_failed(self, view, _event, uri, error):
        if error.matches(WebKit.NetworkError.quark(), WebKit.NetworkError.CANCELLED):
            return False

        def retry():  # service down (e.g. Home Assistant restarting): try again until it is back
            if view.get_uri() in (uri, None):
                view.load_uri(uri)
            return False

        GLib.timeout_add_seconds(10, retry)
        return False  # let WebKit show its error page meanwhile

    def on_permission(self, _view, request):
        request.deny()  # no notification/location/camera prompts on a wall panel
        return True

    def on_new_window(self, view, action):
        view.load_uri(action.get_request().get_uri())  # target=_blank opens in the same tab
        return None


if __name__ == "__main__":
    Kiosk().run(None)
