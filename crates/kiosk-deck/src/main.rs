//! Steam Deck kiosk UI: a minimal touch-screen app that displays daemon
//! status and lets the user pause/resume controller sharing.
//!
//! Talks to `server-deck` via files in `/run/network-deck/` — see
//! `control.rs` for the on-disk contract.

// On non-Linux targets the bin's `main` is a stub that never reaches the
// app body, so all the items in `app` and `control` look dead. They're
// still compiled (and unit-tested) so we silence the warning rather than
// hide the modules — keeps the test surface identical across platforms.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod app;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod control;

#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "kiosk-deck requires Linux. Built on: {}",
        std::env::consts::OS
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn parse_control_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--control-dir" {
            return args.next().map(PathBuf::from).unwrap_or_else(|| {
                eprintln!("--control-dir requires a value");
                std::process::exit(2);
            });
        }
    }
    PathBuf::from("/run/network-deck")
}

#[cfg(target_os = "linux")]
fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("Network Deck"),
        ..Default::default()
    };
    eframe::run_native(
        "Network Deck",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::KioskApp::new(parse_control_dir())))),
    )
}
