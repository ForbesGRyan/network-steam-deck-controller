//! Steam Deck kiosk UI: a minimal touch-screen app that displays daemon
//! status and lets the user pause/resume controller sharing.
//!
//! Talks to `server-deck` via files in `/run/network-deck/` — see
//! `control.rs` for the on-disk contract.

// `control` is consumed by the eframe app body in Task 4. The stub `main`
// in this scaffold doesn't reference it yet, so suppress dead-code lints
// for now rather than carry per-item allows.
#[allow(dead_code)]
mod control;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "kiosk-deck requires Linux. Built on: {}",
        std::env::consts::OS
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> eframe::Result {
    use eframe::egui;

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Network Deck",
        options,
        Box::new(|_cc| Ok(Box::<Stub>::default())),
    )
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct Stub;

#[cfg(target_os = "linux")]
impl eframe::App for Stub {
    fn update(&mut self, ctx: &eframe::egui::Context, _: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Network Deck (stub)");
        });
    }
}
