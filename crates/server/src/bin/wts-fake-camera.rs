//! Test double for wts-camera (used by tests/api.rs, not shipped): speaks the same stdin/stdout protocol.
//! Camera name "crash" exits right after starting, "stuck" ignores SIGTERM; custom_source "probe-fail" makes
//! --probe fail.

use std::io::BufRead;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let probe = r#"{"cameras":[{"index":1,"name":"Fake","id":"fake0"}],"controls":[{"name":"brightness","label":"Helligkeit","type":"float","min":-1.0,"max":1.0,"default":0.0}]}"#;
    if args.get(1).map(String::as_str) == Some("--probe") {
        if args[2].contains("probe-fail") {
            eprintln!("Kamera kaputt");
            std::process::exit(2);
        }
        println!("{probe}");
        return;
    }
    let config: serde_json::Value = serde_json::from_str(&args[1]).expect("config json");
    println!("PROBE {probe}");
    println!("gestartet {}", std::process::id());
    eprintln!("stderr \u{fc}\u{ff}"); // non-ASCII on stderr reaches the log too
    println!("Kamera liefert Bilder");
    println!("CLIENTS 2");
    if config["name"] == "crash" {
        std::process::exit(3);
    }
    #[cfg(unix)]
    if config["name"] == "stuck" {
        // like libcamera hanging in stop: only SIGKILL ends it
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        std::thread::sleep(std::time::Duration::from_secs(600));
    }
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        println!("SET {line}");
    }
}
