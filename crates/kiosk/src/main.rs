//! wts-kiosk: tab bar, one WebKit view per tab (kept alive while switching), lock screen, rotation inside GTK.
//! Runs under cage on tty1 (wts-kiosk.service). Tabs, lock and rotation come from wts-server, polled every
//! second; the kiosk keeps the last state while the server restarts.
//!
//! Crash guard: the 4th start within 3 minutes starts a safe mode without web views that says so on screen
//! and reports it to the server (panel, /api/health), then tries again after 5 minutes. From then on WebKit
//! and GTK render in software ("careful" mode), the most common cause of such loops being the GPU path.

mod rotated;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;
use webkit::prelude::*;

const CSS: &str = "
window, .empty { background: #101214; color: #e8e8e8; }
.bar { background: #15181b; padding: 8px; }
.bar.side { border-right: 1px solid #23272b; }
.bar.top { border-bottom: 1px solid #23272b; }
.bar button { background: none; background-image: none; box-shadow: none; border: none; border-radius: 12px;
              color: #aeb4bb; font-size: 17px; font-weight: 500; padding: 18px 16px; margin: 3px; min-width: 110px;
              transition: background 120ms ease-out, color 120ms ease-out; }
.bar button:active { background: #23282e; }
.bar button.active { background: #2f6fd0; color: white; }
.bar.top button { padding: 11px 16px; min-width: 0; }
.lock { background: rgba(6, 8, 10, 0.94); }
.lock-icon { font-size: 90px; }
.clock { font-size: 72px; font-weight: 300; color: white; }
.lock-text { font-size: 22px; color: #9aa0a6; }
.empty { font-size: 22px; }
";

/// Logs the HA frontend in with the tokens the server got: written to localStorage before HA's scripts run.
const HA_LOGIN: &str = r#"(function () {
  var tokens = TOKENS;
  if (location.origin !== tokens.hassUrl) return;
  var stored = null;
  try { stored = JSON.parse(localStorage.getItem("hassTokens")); } catch (e) {}
  if (stored && stored.refresh_token === tokens.refresh_token) return;
  try { localStorage.setItem("hassTokens", JSON.stringify(tokens)); } catch (e) {}
})();"#;

/// Light/dark for the HA frontend: its browser setting in localStorage.selectedTheme, the theme name is kept.
const HA_THEME: &str = r#"(function () {
  var dark = DARK;  // true, false or null = let HA decide
  var s = {};
  try { s = JSON.parse(localStorage.getItem("selectedTheme")) || {}; } catch (e) {}
  if (!s.theme) s.theme = "default";
  if (dark === null) delete s.dark; else s.dark = dark;
  try { localStorage.setItem("selectedTheme", JSON.stringify(s)); } catch (e) {}
})();"#;

const SAFE_FOR: u32 = 300;

/// What the poll thread reports besides the uptime: written on the GTK thread, read by the poll thread.
#[derive(Default)]
struct Report {
    tab: String,
    error: String,
}

struct Kiosk {
    safe: bool,
    careful: bool,
    session: Option<webkit::NetworkSession>,
    rotated: rotated::Rotated,
    main: gtk::Box,
    bar: gtk::Box,
    stack: gtk::Stack,
    lock: gtk::Box,
    clock: gtk::Label,
    report: Arc<Mutex<Report>>,
    st: RefCell<State>,
}

#[derive(Default)]
struct State {
    tabs: Option<Vec<Value>>,
    locked: Option<bool>,
    views: Vec<webkit::WebView>,
    buttons: Vec<gtk::Button>,
    current: usize,
}

fn main() -> glib::ExitCode {
    let started = Instant::now();
    let dir = glib::user_data_dir().join("walltablet");
    let _ = std::fs::create_dir_all(&dir);
    let safe = crash_loop(&dir);
    let careful_file = dir.join("careful");
    if safe {
        println!("Kiosk ist mehrfach kurz nacheinander gestartet: sicherer Modus für {} Minuten", SAFE_FOR / 60);
        let _ = std::fs::write(&careful_file, "");
    }
    let careful = careful_file.exists();
    // SAFETY: no other threads exist yet
    unsafe {
        // Ubuntu blocks unprivileged user namespaces for programs without an AppArmor profile, so WebKit's
        // bubblewrap sandbox cannot start its web processes. Only local pages are shown.
        if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
            std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        }
        if careful {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            std::env::set_var("GSK_RENDERER", "cairo");
        }
    }
    let app = gtk::Application::builder()
        .application_id("de.walltablet.Kiosk")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE) // a second start must not hand over to a stale instance
        .build();
    app.connect_activate(move |app| {
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        build_ui(app, &dir, safe, careful, started);
    });
    app.run_with_args::<&str>(&[])
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Records this start; true if it is the 4th within 3 minutes.
fn crash_loop(dir: &Path) -> bool {
    let file = dir.join("starts.json");
    let now = unix_now();
    let mut starts: Vec<u64> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    starts.retain(|&t| t <= now && now - t < 180);
    starts.push(now);
    let _ = std::fs::write(&file, serde_json::to_string(&starts).unwrap_or_default());
    starts.len() >= 4
}

fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("10.255.255.255:9").map(|()| s)) // no packet is sent
        .and_then(|s| s.local_addr())
        .map_or_else(|_| "<tablet>".into(), |a| a.ip().to_string())
}

fn build_ui(app: &gtk::Application, dir: &Path, safe: bool, careful: bool, started: Instant) {
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    // one persistent session: logins (Home Assistant tokens, cookies) survive restarts
    let session = (!safe).then(|| {
        let s = webkit::NetworkSession::new(dir.join("data").to_str(), dir.join("cache").to_str());
        if let Some(cookies) = s.cookie_manager() {
            cookies.set_persistent_storage(&dir.join("cookies.sqlite").to_string_lossy(), webkit::CookiePersistentStorage::Sqlite);
        }
        s.set_tls_errors_policy(webkit::TLSErrorsPolicy::Ignore); // local devices, self-signed certificates
        s
    });

    let bar = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["bar", "side"]).build();
    let stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    let main = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main.append(&bar);
    main.append(&stack);

    let clock = gtk::Label::builder().css_classes(["clock"]).build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    inner.append(&gtk::Label::builder().label("🔒").css_classes(["lock-icon"]).build());
    inner.append(&clock);
    inner.append(&gtk::Label::builder().label("Gesperrt").css_classes(["lock-text"]).build());
    let lock = gtk::Box::builder().css_classes(["lock"]).visible(false).build(); // fills the overlay: takes every touch
    lock.append(&inner);

    let overlay = gtk::Overlay::builder().child(&main).build();
    overlay.add_overlay(&lock);
    let rotated = rotated::Rotated::new(&overlay);
    let window = gtk::ApplicationWindow::builder().application(app).title("WallTabletSuite").child(&rotated).build();

    let report = Arc::new(Mutex::new(Report::default()));
    let k = Rc::new(Kiosk { safe, careful, session, rotated, main, bar, stack, lock, clock, report: report.clone(), st: RefCell::default() });
    k.build(None);
    // fullscreen only once the window is shown: cage 0.2.1 (wlroots 0.19) aborts on set_fullscreen before the
    // surface's first commit, which took the whole kiosk down in a restart loop. cage sizes it to the screen anyway.
    window.set_decorated(false);
    window.connect_map(|w| w.fullscreen()); // runs after GTK's own map, which made the first commit
    window.present();

    k.tick();
    let weak = Rc::downgrade(&k);
    glib::timeout_add_seconds_local(10, move || match weak.upgrade() {
        Some(k) => {
            k.tick();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    if safe {
        let app = app.clone();
        glib::timeout_add_seconds_local_once(SAFE_FOR, move || app.quit()); // systemd starts it again
    }

    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || poll(&report, &tx, safe, started));
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if let Some(v) = rx.try_iter().last() {
            k.apply(&v);
        }
        glib::ControlFlow::Continue
    });
}

/// Fetches the kiosk state once a second and reports tab, error, uptime and safe mode. Server errors keep
/// the last state on screen (also the lock).
fn poll(report: &Mutex<Report>, tx: &mpsc::Sender<Value>, safe: bool, started: Instant) {
    let server = std::env::var("WTS_SERVER").unwrap_or_else(|_| format!("http://127.0.0.1:{}", wts_common::PORT));
    let url = format!("{server}/api/kiosk");
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(3))).build().into();
    loop {
        let (tab, error) = {
            let r = report.lock().unwrap();
            (r.tab.clone(), r.error.clone())
        };
        let reply = agent
            .get(&url)
            .query("tab", &tab)
            .query("error", &error)
            .query("up", started.elapsed().as_secs().to_string())
            .query("safe", if safe { "1" } else { "0" })
            .call();
        if let Ok(v) = reply.and_then(|mut r| r.body_mut().read_json::<Value>())
            && tx.send(v).is_err() {
                return;
            }
        std::thread::sleep(Duration::from_secs(1));
    }
}

impl Kiosk {
    fn tick(&self) {
        if let Ok(now) = glib::DateTime::now_local().and_then(|t| t.format("%H:%M")) {
            self.clock.set_label(&now);
        }
    }

    fn set_error(&self, e: &str) {
        if !e.is_empty() {
            println!("{e}");
        }
        self.report.lock().unwrap().error = e.chars().take(300).collect();
    }

    /// Clears the error only if it is still `e` (a newer error stays).
    fn clear_error(&self, e: &str) {
        let mut r = self.report.lock().unwrap();
        if r.error == e {
            r.error.clear();
        }
    }

    fn apply(self: &Rc<Self>, v: &Value) {
        let degrees = v["rotation"].as_u64().and_then(|r| u16::try_from(r).ok()).unwrap_or(0);
        self.rotated.set_degrees(degrees);
        self.place_bar(degrees % 180 == 90);
        if let Some(tabs) = v["tabs"].as_array()
            && self.st.borrow().tabs.as_ref() != Some(tabs) {
                self.build(Some(tabs));
            }
        if let Some(locked) = v["locked"].as_bool()
            && self.st.borrow().locked != Some(locked) {
                self.st.borrow_mut().locked = Some(locked);
                self.lock.set_visible(locked);
                self.main.set_sensitive(!locked); // also blocks the keyboard
            }
        // set once on a change, not on every poll, and cleared once a later rotation works
        const ROT: &str = "Drehung: Transformation passt nicht, Bild ungedreht";
        let shown = self.report.lock().unwrap().error == ROT;
        if self.rotated.failed() && !shown {
            self.set_error(ROT);
        } else if !self.rotated.failed() && shown {
            self.clear_error(ROT);
        }
    }

    /// Portrait: tabs in a row on top, the screen is narrow. Landscape: a column on the left.
    fn place_bar(&self, portrait: bool) {
        let (outer, row) = if portrait {
            (gtk::Orientation::Vertical, gtk::Orientation::Horizontal)
        } else {
            (gtk::Orientation::Horizontal, gtk::Orientation::Vertical)
        };
        self.main.set_orientation(outer);
        self.bar.set_orientation(row);
        self.bar.set_homogeneous(portrait); // equal widths across the full row
        self.bar.set_css_classes(if portrait { &["bar", "top"] } else { &["bar", "side"] });
    }

    /// Recreates tab bar and views; `None` = no answer from the server yet.
    fn build(self: &Rc<Self>, tabs: Option<&[Value]>) {
        while let Some(c) = self.bar.first_child() {
            self.bar.remove(&c);
        }
        while let Some(c) = self.stack.first_child() {
            self.stack.remove(&c);
        }
        let text = if self.safe {
            format!("Sicherer Modus: Der Kiosk ist mehrfach abgestürzt.\nUrsache im Web-Panel unter Logs: http://{}/\nNeuer Versuch in {} Minuten.", local_ip(), SAFE_FOR / 60)
        } else if tabs.is_none() {
            "Verbinde mit dem Server …".into()
        } else {
            format!("Keine Tabs eingerichtet.\nWeb-Panel: http://{}/", local_ip())
        };
        let empty = gtk::Label::builder().label(text).css_classes(["empty"]).justify(gtk::Justification::Center).build();
        self.stack.add_named(&empty, Some("empty"));
        let mut views = Vec::new();
        let mut buttons = Vec::new();
        if let Some(session) = &self.session {
            for (i, tab) in tabs.unwrap_or_default().iter().enumerate() {
                let view = self.view(session, tab);
                self.stack.add_named(&view, Some(&i.to_string()));
                views.push(view);
                let button = gtk::Button::with_label(tab["name"].as_str().unwrap_or("?"));
                let weak = Rc::downgrade(self);
                button.connect_clicked(move |_| {
                    if let Some(k) = weak.upgrade() {
                        k.on_tab(i);
                    }
                });
                self.bar.append(&button);
                buttons.push(button);
            }
        }
        self.bar.set_visible(buttons.len() > 1);
        let current = {
            let mut st = self.st.borrow_mut();
            st.tabs = tabs.map(<[Value]>::to_vec);
            st.views = views;
            st.buttons = buttons;
            st.current.min(st.views.len().saturating_sub(1))
        };
        if self.st.borrow().views.is_empty() {
            self.report.lock().unwrap().tab.clear();
            self.stack.set_visible_child_name("empty");
        } else {
            self.show(current);
        }
    }

    fn view(self: &Rc<Self>, session: &webkit::NetworkSession, tab: &Value) -> webkit::WebView {
        let scripts = webkit::UserContentManager::new();
        let mut code = Vec::new();
        if tab["ha_tokens"].is_object() {
            code.push(HA_LOGIN.replace("TOKENS", &tab["ha_tokens"].to_string()));
        }
        if let Some(theme) = tab["ha_theme"].as_str() {
            code.push(HA_THEME.replace("DARK", match theme {
                "dark" => "true",
                "light" => "false",
                _ => "null",
            }));
        }
        for js in code {
            scripts.add_script(&webkit::UserScript::new(
                &js,
                webkit::UserContentInjectedFrames::TopFrame,
                webkit::UserScriptInjectionTime::Start,
                &[],
                &[],
            ));
        }
        let policies = webkit::WebsitePolicies::builder().autoplay(webkit::AutoplayPolicy::Allow).build();
        let view = webkit::WebView::builder()
            .network_session(session)
            .user_content_manager(&scripts)
            .website_policies(&policies)
            .hexpand(true)
            .vexpand(true)
            .build();
        if self.careful
            && let Some(s) = WebViewExt::settings(&view) {
                s.set_hardware_acceleration_policy(webkit::HardwareAccelerationPolicy::Never);
            }
        view.connect_context_menu(|_, _, _| true);
        view.connect_permission_request(|_, request| {
            request.deny(); // no notification/location/camera prompts on a wall panel
            true
        });
        // target=_blank and window.open load in the same tab. window.open skips the new-window policy and
        // only emits "create"; raw signal because the typed binding cannot return NULL (= no new view)
        view.connect_local("create", false, |a| {
            let view = a[0].get::<webkit::WebView>().ok()?;
            let mut action = a[1].get::<webkit::NavigationAction>().ok()?;
            if let Some(uri) = action.request().and_then(|r| r.uri()) {
                view.load_uri(&uri);
            }
            Some(None::<gtk::Widget>.to_value())
        });
        view.connect_load_failed(|view, _event, uri, error| {
            if error.matches(webkit::NetworkError::Cancelled) || error.kind::<webkit::PolicyError>().is_some() {
                return false;
            }
            // service down (e.g. Home Assistant restarting): try again until it is back, while the view is shown
            let (weak, uri) = (view.downgrade(), uri.to_string());
            glib::timeout_add_seconds_local_once(10, move || {
                if let Some(view) = weak.upgrade()
                    && view.parent().is_some() && view.uri().is_none_or(|u| u == uri) {
                        view.load_uri(&uri);
                    }
            });
            false // WebKit shows its error page meanwhile
        });
        let weak = Rc::downgrade(self);
        let name = tab["name"].as_str().unwrap_or("?").to_string();
        let crashed = Rc::new(RefCell::new(String::new()));
        let (k2, c2) = (weak.clone(), crashed.clone());
        view.connect_load_changed(move |_, event| {
            // the reload after a crash worked: the message is old news
            if event == webkit::LoadEvent::Finished
                && let Some(k) = k2.upgrade() {
                    k.clear_error(&c2.take());
                }
        });
        view.connect_web_process_terminated(move |view, reason| {
            if reason == webkit::WebProcessTerminationReason::TerminatedByApi {
                return;
            }
            if let Some(k) = weak.upgrade() {
                let msg = format!("Tab {name}: Webprozess beendet ({reason:?}), lade neu");
                k.set_error(&msg);
                crashed.replace(msg);
            }
            let weak = view.downgrade();
            glib::timeout_add_seconds_local_once(2, move || {
                if let Some(view) = weak.upgrade().filter(|v| v.parent().is_some()) {
                    view.reload();
                }
            });
        });
        view.load_uri(tab["url"].as_str().unwrap_or("about:blank"));
        view
    }

    fn show(&self, i: usize) {
        let st = self.st.borrow();
        let Some(tab) = st.tabs.as_ref().and_then(|t| t.get(i)) else { return };
        self.report.lock().unwrap().tab = tab["name"].as_str().unwrap_or_default().to_string();
        self.stack.set_visible_child_name(&i.to_string());
        for (j, b) in st.buttons.iter().enumerate() {
            if j == i {
                b.add_css_class("active");
            } else {
                b.remove_css_class("active");
            }
        }
        drop(st);
        self.st.borrow_mut().current = i;
    }

    fn on_tab(&self, i: usize) {
        let reload = {
            let st = self.st.borrow();
            (i == st.current).then(|| (st.views.get(i).cloned(), st.tabs.as_ref().and_then(|t| t[i]["url"].as_str().map(str::to_string))))
        };
        // tapping the open tab again goes back to its start page
        if let Some((Some(view), Some(url))) = reload {
            view.load_uri(&url);
        }
        self.show(i);
    }
}
