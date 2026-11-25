//! wts-server: web panel, API, kiosk config, lock, Home Assistant login, camera supervisor, ONVIF, updates.
//! No login, plain HTTP: meant for the local network only (see README).

fn main() {
    let data = std::env::var_os("WTS_DATA").map_or_else(|| "data".into(), std::path::PathBuf::from);
    let port = std::env::var("WTS_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(wts_common::PORT);
    if let Err(e) = wts_server::run(data, port) {
        eprintln!("wts-server: {e}");
        std::process::exit(1);
    }
}
