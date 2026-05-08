//! eframe app body for the Deck kiosk.
//!
//! Pure-data view derivation lives in `derive_view`, which is exhaustively
//! unit-tested. The `update` impl only handles painting and dispatching
//! button taps to the on-disk control surface in `control.rs`.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;

use crate::control::{self, Status};

pub struct KioskApp {
    control_dir: PathBuf,
}

impl KioskApp {
    pub fn new(control_dir: PathBuf) -> Self {
        Self { control_dir }
    }
}

impl eframe::App for KioskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let status = control::read_status(&self.control_dir);
        let view = derive_view(status.as_ref());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(40.0);
                ui.heading(egui::RichText::new(&view.text).size(48.0));
                ui.add_space(40.0);
                let button = egui::Button::new(
                    egui::RichText::new(view.button_label).size(32.0),
                )
                .min_size(egui::vec2(ui.available_width(), 100.0));
                if let Some(target) = view.toggle_to {
                    if ui.add(button).clicked() {
                        if let Err(e) = control::set_paused(&self.control_dir, target) {
                            eprintln!("set_paused failed: {e}");
                        }
                    }
                } else {
                    ui.add_enabled(false, button);
                }
            });
        });

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct View {
    text: String,
    button_label: &'static str,
    /// `Some(target)` means the button toggles `paused` to `target`. `None` means the button is disabled.
    toggle_to: Option<bool>,
}

fn derive_view(status: Option<&Status>) -> View {
    match status {
        None => View {
            text: "Daemon not running".into(),
            button_label: "—",
            toggle_to: None,
        },
        Some(s) if s.paused => View {
            text: "Paused".into(),
            button_label: "Reconnect",
            toggle_to: Some(false),
        },
        Some(s) if !s.peer_present => View {
            text: "Searching for client…".into(),
            button_label: "Pause",
            toggle_to: Some(true),
        },
        Some(s) if !s.bound => View {
            text: format!("Connecting to {}…", peer_label(s)),
            button_label: "Pause",
            toggle_to: Some(true),
        },
        Some(s) => View {
            text: format!("Connected to {}", peer_label(s)),
            button_label: "Disconnect",
            toggle_to: Some(true),
        },
    }
}

fn peer_label(s: &Status) -> &str {
    s.peer_name.as_deref().unwrap_or("client")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(peer_name: Option<&str>, peer_present: bool, bound: bool, paused: bool) -> Status {
        Status {
            peer_name: peer_name.map(str::to_owned),
            peer_present,
            bound,
            paused,
        }
    }

    #[test]
    fn none_status_renders_daemon_not_running_disabled() {
        let v = derive_view(None);
        assert_eq!(v.text, "Daemon not running");
        assert_eq!(v.toggle_to, None);
    }

    #[test]
    fn paused_renders_reconnect_targeting_unpause() {
        let s = status(Some("desktop"), true, true, true);
        let v = derive_view(Some(&s));
        assert_eq!(v.text, "Paused");
        assert_eq!(v.button_label, "Reconnect");
        assert_eq!(v.toggle_to, Some(false));
    }

    #[test]
    fn no_peer_renders_searching_targeting_pause() {
        let s = status(None, false, false, false);
        let v = derive_view(Some(&s));
        assert_eq!(v.text, "Searching for client…");
        assert_eq!(v.button_label, "Pause");
        assert_eq!(v.toggle_to, Some(true));
    }

    #[test]
    fn peer_unbound_renders_connecting_with_name_or_fallback() {
        let with_name = status(Some("desktop"), true, false, false);
        let v = derive_view(Some(&with_name));
        assert_eq!(v.text, "Connecting to desktop…");
        assert_eq!(v.button_label, "Pause");
        assert_eq!(v.toggle_to, Some(true));

        let without_name = status(None, true, false, false);
        let v = derive_view(Some(&without_name));
        assert_eq!(v.text, "Connecting to client…");
    }

    #[test]
    fn peer_bound_renders_connected_targeting_pause() {
        let s = status(Some("desktop"), true, true, false);
        let v = derive_view(Some(&s));
        assert_eq!(v.text, "Connected to desktop");
        assert_eq!(v.button_label, "Disconnect");
        assert_eq!(v.toggle_to, Some(true));
    }
}
