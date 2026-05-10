//! Pair flow used by the tray ("Pair new Deck...") and by first-run setup.
//!
//! Drives `discovery::pair::run_pair_with` from a worker thread while a
//! single egui window walks the user through the states: optional intro
//! (first-run only) → searching → confirm fingerprint → finalizing →
//! result. Channels bridge the worker (calls into `PairUI`) and the egui
//! thread (renders state, collects decisions).
//!
//! Single egui app per pair invocation — `eframe::run_native` (winit)
//! panics if it's called twice in the same process, so all dialog states
//! are rolled into one app rather than several sequential `run_native`
//! calls.

use std::net::UdpSocket;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;

use discovery::pair::{Decision, PairConfig, PairOutcome, PairUI};
use discovery::TrustedPeer;

const WINDOW_SIZE: [f32; 2] = [520.0, 320.0];

/// Run the pair flow against `sock`. Consumes the socket: on return the
/// worker has dropped it, so the caller can re-bind 49152 immediately.
///
/// `show_intro = true` for first-run (the dialog opens with a "Start
/// pairing" button); `false` for tray-triggered pair (jumps straight to
/// the searching state since the user already clicked a Pair menu item).
pub fn run(
    sock: UdpSocket,
    identity: Arc<discovery::Identity>,
    self_name: String,
    state_dir: &Path,
    show_intro: bool,
) -> PairOutcome {
    let cfg = PairConfig {
        identity,
        recv_sock: sock,
        targets: discovery::netifs::broadcast_targets(super::DEFAULT_PORT),
        self_name,
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(120),
    };

    let (event_tx, event_rx) = mpsc::channel::<UiEvent>();
    let (decision_tx, decision_rx) = mpsc::channel::<Decision>();
    let start = Arc::new(AtomicBool::new(!show_intro));
    let cancel = Arc::new(AtomicBool::new(false));
    let outcome: Arc<Mutex<Option<PairOutcome>>> = Arc::new(Mutex::new(None));

    let worker = {
        let start = start.clone();
        let cancel = cancel.clone();
        let outcome = outcome.clone();
        thread::Builder::new()
            .name("pair-worker".into())
            .spawn(move || {
                // Block until the user clicks Start (first-run intro), or
                // immediately fall through if the dialog skipped it.
                while !start.load(Ordering::Acquire) {
                    if cancel.load(Ordering::Acquire) {
                        eprintln!("pair: cancelled before start");
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                eprintln!(
                    "pair: starting broadcast (fingerprint {})",
                    cfg.identity.fingerprint_str(),
                );
                let mut ui = ChannelUi {
                    tx: event_tx,
                    decisions: decision_rx,
                    cancel,
                };
                let result = discovery::pair::run_pair_with(&cfg, &mut ui);
                eprintln!("pair: outcome {result:?}");
                *outcome.lock().expect("outcome mutex") = Some(result);
                // cfg drops here, releasing the UDP socket so the caller
                // can re-bind 49152 if it wants to resume normal operation.
            })
            .expect("spawn pair-worker")
    };

    let app = PairApp {
        state: if show_intro { State::Intro } else { State::Searching },
        events: event_rx,
        decisions: decision_tx,
        start,
        cancel: cancel.clone(),
    };

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(WINDOW_SIZE)
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "Network Deck — pairing",
        opts,
        Box::new(move |_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    )
    .expect("eframe::run_native(pair)");

    // Window closed. Either the worker finished naturally and stored an
    // outcome, or the user cancelled — flag the worker so it exits at the
    // next tick (max ~200 ms), then join. Joining is what guarantees the
    // socket is released.
    cancel.store(true, Ordering::Release);
    let _ = worker.join();

    let result = outcome
        .lock()
        .expect("outcome mutex")
        .take()
        .unwrap_or(PairOutcome::Cancelled);
    result
}

enum UiEvent {
    Prompt { name: String, fingerprint: String },
    Paired { name: String },
    Failed { reason: String },
}

#[derive(Clone)]
enum State {
    Intro,
    Searching,
    Confirm { name: String, fingerprint: String },
    Finalizing { name: String },
    Done { message: String, is_error: bool },
}

struct ChannelUi {
    tx: mpsc::Sender<UiEvent>,
    decisions: mpsc::Receiver<Decision>,
    cancel: Arc<AtomicBool>,
}

impl PairUI for ChannelUi {
    fn on_started(&mut self, my_fingerprint: &str, _self_name: &str) {
        eprintln!("pair: on_started (my fingerprint {my_fingerprint})");
    }

    fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision {
        eprintln!("pair: candidate \"{name}\" fingerprint {fingerprint}");
        let _ = self.tx.send(UiEvent::Prompt {
            name: name.to_owned(),
            fingerprint: fingerprint.to_owned(),
        });
        // Block until the egui thread sends a decision. If the channel
        // closes (window destroyed), treat as Reject so the pair flow
        // unwinds cleanly.
        self.decisions.recv().unwrap_or(Decision::Reject)
    }

    fn on_paired(&mut self, peer: &TrustedPeer) {
        let _ = self.tx.send(UiEvent::Paired { name: peer.name.clone() });
    }

    fn on_failed(&mut self, reason: &str) {
        let _ = self.tx.send(UiEvent::Failed { reason: reason.to_owned() });
    }

    fn cancelled(&mut self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
}

struct PairApp {
    state: State,
    events: mpsc::Receiver<UiEvent>,
    decisions: mpsc::Sender<Decision>,
    start: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl PairApp {
    fn drain_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(UiEvent::Prompt { name, fingerprint }) => {
                    self.state = State::Confirm { name, fingerprint };
                }
                Ok(UiEvent::Paired { name }) => {
                    self.state = State::Done {
                        message: format!(
                            "Paired with {name}.\n\n\
                             The tray will restart to pick up the new pairing.",
                        ),
                        is_error: false,
                    };
                }
                Ok(UiEvent::Failed { reason }) => {
                    self.state = State::Done {
                        message: format!("Pair failed.\n\n{reason}"),
                        is_error: true,
                    };
                }
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
}

impl eframe::App for PairApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.drain_events();

        // X-button → cancel everywhere except the final Done screen
        // (where it's just a "close after reading the result" gesture).
        if ctx.input(|i| i.viewport().close_requested())
            && !matches!(self.state, State::Done { .. })
        {
            self.cancel.store(true, Ordering::Release);
            // If we're sitting on a confirm prompt the worker is blocked
            // on decisions.recv(); send Reject so it can return.
            if matches!(self.state, State::Confirm { .. }) {
                let _ = self.decisions.send(Decision::Reject);
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(20.0);
            // Clone state to avoid borrow conflicts when we mutate self
            // inside the match arms.
            let state = self.state.clone();
            match state {
                State::Intro => self.render_intro(ctx, ui),
                State::Searching => self.render_searching(ctx, ui),
                State::Confirm { name, fingerprint } => {
                    self.render_confirm(ctx, ui, &name, &fingerprint);
                }
                State::Finalizing { name } => Self::render_finalizing(ui, &name),
                State::Done { message, is_error } => {
                    Self::render_done(ctx, ui, &message, is_error);
                }
            }
        });

        // Keep ticking so events get drained even when the user isn't
        // interacting; ~10 Hz is enough.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl PairApp {
    fn render_intro(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("No paired Deck found.")
                .size(18.0)
                .strong(),
        );
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "On the Deck, launch Network Deck and tap \"Start pairing\".\n\
                 Then click \"Start pairing\" below to broadcast from this PC.",
            )
            .size(15.0),
        );
        bottom_buttons(ui, |ui| {
            // right_to_left: rightmost is added first. Primary action
            // (Start pairing) goes on the right per Win32 convention; the
            // anti-foot-gun left-side-Accept is reserved for the
            // fingerprint confirm screen.
            if button(ui, "Start pairing").clicked() {
                self.start.store(true, Ordering::Release);
                self.state = State::Searching;
            }
            ui.add_space(8.0);
            if button(ui, "Cancel").clicked() {
                self.cancel.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn render_searching(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.heading(egui::RichText::new("Looking for Deck...").size(20.0));
            ui.add_space(12.0);
            ui.spinner();
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "Waiting up to 120 s. Make sure the Deck is on the same network\n\
                     and \"Start pairing\" is active there too.",
                )
                .size(14.0),
            );
        });
        bottom_buttons(ui, |ui| {
            if button(ui, "Cancel").clicked() {
                self.cancel.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn render_confirm(
        &mut self,
        _ctx: &egui::Context,
        ui: &mut egui::Ui,
        name: &str,
        fingerprint: &str,
    ) {
        ui.label(
            egui::RichText::new(format!("Found Deck \"{name}\"."))
                .size(18.0)
                .strong(),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Fingerprint:").size(14.0));
        ui.label(
            egui::RichText::new(fingerprint)
                .size(15.0)
                .monospace(),
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Verify the same fingerprint shows on the Deck before accepting.",
            )
            .size(14.0),
        );
        bottom_buttons(ui, |ui| {
            // Accept on the LEFT (added second under right_to_left), so a
            // muscle-memory tap on the right edge — where Windows usually
            // puts a primary OK — doesn't auto-trust an unknown Deck.
            if button(ui, "Reject").clicked() {
                let _ = self.decisions.send(Decision::Reject);
                self.state = State::Searching;
            }
            ui.add_space(8.0);
            if button(ui, "Accept").clicked() {
                let _ = self.decisions.send(Decision::Accept);
                self.state = State::Finalizing { name: name.to_owned() };
            }
        });
    }

    fn render_finalizing(ui: &mut egui::Ui, name: &str) {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.heading(egui::RichText::new("Completing pair...").size(20.0));
            ui.add_space(12.0);
            ui.spinner();
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(format!(
                    "Exchanging confirmation with \"{name}\".",
                ))
                .size(14.0),
            );
        });
        // No buttons here — the accept exchange is short. If it stalls the
        // outer 120 s timeout will land us in the Done/error state.
    }

    fn render_done(ctx: &egui::Context, ui: &mut egui::Ui, message: &str, is_error: bool) {
        if is_error {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                egui::RichText::new(message).size(15.0),
            );
        } else {
            ui.label(egui::RichText::new(message).size(15.0));
        }
        bottom_buttons(ui, |ui| {
            if button(ui, "OK").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

fn button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(15.0))
            .min_size(egui::vec2(120.0, 36.0)),
    )
}

fn bottom_buttons<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_space(20.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), contents)
        .inner
}
