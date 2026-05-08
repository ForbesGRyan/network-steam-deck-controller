//! eframe app body for the kiosk UI.
//!
//! Pure-data view derivation lives in `derive_view`, which is exhaustively
//! unit-tested. The `update` impl only handles painting and dispatching
//! button taps to the on-disk control surface in `control.rs`.

use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::control::{self, Status};
use crate::daemon_child::{DaemonChild, DaemonState, Escalation};
use crate::pair_worker::{PairWorker, Phase as PairPhase};

/// Setup state machine for the first-run screen.
enum SetupPhase {
    /// Showing instructions + "Install now" button.
    Idle,
    /// `pkexec network-deck install` is running.
    Installing(Child),
    /// Installer exited successfully; user should relaunch.
    Done,
    /// Installer exited non-zero or failed to spawn.
    Failed(String),
}

struct SetupState {
    self_exe: PathBuf,
    phase: SetupPhase,
}

/// Pre-pair flow: we have an installed binary but no trust file yet.
struct PairState {
    identity: Arc<discovery::Identity>,
    self_name: String,
    state_dir: PathBuf,
    /// `None` until the user taps "Pair" — then a worker is spawned.
    worker: Option<PairWorker>,
    /// Set after a successful pair so the UI can show a final "restart" panel.
    completed: Option<discovery::TrustedPeer>,
    /// Last spawn error, if `start_worker` failed (e.g. port 49152 in use).
    last_error: Option<String>,
}

pub struct KioskApp {
    control_dir: PathBuf,
    /// Present iff the binary detected we're not yet installed. Drives the
    /// first-run flow instead of the normal status panel.
    setup: Option<SetupState>,
    /// Present iff installed but no trust file. Drives the pair flow.
    pair: Option<PairState>,
    /// Path to our own binary, used to respawn the daemon (sudo / pkexec
    /// fallback). `None` if not provided — Escalate button hidden.
    self_exe: Option<PathBuf>,
    /// Owned daemon supervisor; replaced on Escalate. `Drop` SIGTERMs.
    daemon_child: Option<DaemonChild>,
    /// Shared view of the daemon child's lifecycle. `None` only when we
    /// never even tried to spawn (e.g. not installed yet).
    daemon: Option<Arc<DaemonState>>,
    /// Set when the user closes the window — paints a "stopping" panel for
    /// one frame so we can synchronously SIGTERM-and-wait the daemon
    /// before letting the close go through.
    shutting_down: bool,
}

impl KioskApp {
    pub fn new(control_dir: PathBuf) -> Self {
        Self {
            control_dir,
            setup: None,
            pair: None,
            self_exe: None,
            daemon_child: None,
            daemon: None,
            shutting_down: false,
        }
    }

    #[must_use]
    pub fn with_setup_required(mut self, self_exe: PathBuf) -> Self {
        self.setup = Some(SetupState {
            self_exe: self_exe.clone(),
            phase: SetupPhase::Idle,
        });
        self.self_exe = Some(self_exe);
        self
    }

    #[must_use]
    pub fn with_pair_required(
        mut self,
        identity: Arc<discovery::Identity>,
        self_name: String,
        state_dir: PathBuf,
    ) -> Self {
        self.pair = Some(PairState {
            identity,
            self_name,
            state_dir,
            worker: None,
            completed: None,
            last_error: None,
        });
        self
    }

    #[must_use]
    pub fn with_daemon(
        mut self,
        self_exe: PathBuf,
        child: Option<DaemonChild>,
        state: Arc<DaemonState>,
    ) -> Self {
        self.self_exe = Some(self_exe);
        self.daemon_child = child;
        self.daemon = Some(state);
        self
    }

    /// Spawn a fresh daemon child with the given escalation method, replacing
    /// the existing one. The old child's `Drop` sends SIGTERM.
    fn respawn_daemon(&mut self, method: Escalation) {
        let Some(self_exe) = self.self_exe.clone() else { return };
        // Drop old child first so its Drop kills it before we spawn another.
        self.daemon_child = None;
        match DaemonChild::spawn_with(&self_exe, method) {
            Ok((child, state)) => {
                self.daemon_child = Some(child);
                self.daemon = Some(state);
            }
            Err(e) => {
                let state = Arc::new(DaemonState::default());
                if let Ok(mut tail) = state.stderr_tail.lock() {
                    tail.push(format!("could not spawn daemon ({method:?}): {e}"));
                }
                if let Ok(mut slot) = state.exit_status.lock() {
                    *slot = Some("spawn failed".into());
                }
                self.daemon = Some(state);
            }
        }
    }
}

impl eframe::App for KioskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Window-close handling. eframe fires `close_requested` on X-button
        // clicks; we cancel the close once, paint a "stopping" panel,
        // synchronously drop the daemon child (waits for usbip unbind),
        // then send Close to actually exit. This guarantees the controller
        // is released back to the Deck — relying on Drop alone is brittle
        // because some platforms shortcut process::exit on close.
        if ctx.input(|i| i.viewport().close_requested()) && self.daemon_child.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.shutting_down = true;
        }
        if self.shutting_down {
            self.draw_shutting_down(ctx);
            // Drop the daemon synchronously after one paint so the user
            // sees the "stopping" message, then close on the next frame.
            self.daemon_child = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.setup.is_some() {
            self.draw_setup(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }

        if self.pair.is_some() {
            self.draw_pair(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }

        // If the daemon has died (or never started), surface why instead of
        // a blank "Daemon not running." This is the difference between a
        // useful first-launch experience and silent failure.
        let daemon_dead = self
            .daemon
            .as_ref()
            .is_some_and(|d| !d.is_alive());
        if daemon_dead {
            self.draw_daemon_failed(ctx);
            ctx.request_repaint_after(Duration::from_millis(500));
            return;
        }

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

impl KioskApp {
    fn draw_shutting_down(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(80.0);
                ui.heading(egui::RichText::new("Stopping…").size(48.0));
                ui.add_space(20.0);
                ui.label(
                    egui::RichText::new("Releasing the controller back to the Deck.")
                        .size(20.0),
                );
                ui.add_space(40.0);
                ui.spinner();
            });
        });
    }

    fn draw_pair(&mut self, ctx: &egui::Context) {
        let Some(state) = self.pair.as_mut() else { return };

        // Snapshot worker phase up front; lock is released before any UI
        // code runs so we don't keep the mutex held across egui callbacks.
        let phase: Option<PairPhase> = state
            .worker
            .as_ref()
            .and_then(|w| w.phase.lock().ok().as_deref().cloned());

        // Promote a Done phase into `state.completed` so the worker can be
        // dropped (and joined) before painting the success screen.
        if let Some(PairPhase::Done(peer)) = &phase {
            state.completed = Some(peer.clone());
            state.worker = None;
        }

        // Click intents collected during paint, applied after the egui
        // borrow ends. This avoids holding `&state.worker` across a
        // potential `state.worker = None` mutation in the same closure.
        #[derive(Default)]
        struct Intent {
            accept: bool,
            reject: bool,
            reset_worker: bool,
            start_worker: bool,
            close: bool,
        }
        let mut intent = Intent::default();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(40.0);

                if let Some(peer) = &state.completed {
                    ui.heading(egui::RichText::new("Paired!").size(56.0));
                    ui.add_space(20.0);
                    ui.label(egui::RichText::new(format!("with {}", peer.name)).size(28.0));
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "fingerprint {}",
                            discovery::packet::fingerprint_str(
                                &discovery::packet::fingerprint(&peer.pubkey),
                            ),
                        ))
                        .monospace()
                        .size(16.0),
                    );
                    ui.add_space(40.0);
                    ui.label(
                        egui::RichText::new(
                            "Close this window and relaunch from Game Mode\nto start the bridge.",
                        )
                        .size(20.0),
                    );
                    ui.add_space(40.0);
                    let btn = egui::Button::new(egui::RichText::new("Close").size(28.0))
                        .min_size(egui::vec2(ui.available_width(), 80.0));
                    if ui.add(btn).clicked() { intent.close = true; }
                    return;
                }

                let Some(phase) = phase.clone() else {
                    ui.heading(egui::RichText::new("Pair with PC").size(48.0));
                    ui.add_space(20.0);
                    ui.label(
                        egui::RichText::new(
                            "On your PC, open the Network Deck tray and click Pair.\n\
                             Then tap Start below — both sides have 120 s to confirm.",
                        )
                        .size(20.0),
                    );
                    if let Some(err) = &state.last_error {
                        ui.add_space(20.0);
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            egui::RichText::new(err).size(16.0),
                        );
                    }
                    ui.add_space(40.0);
                    let btn = egui::Button::new(egui::RichText::new("Start pairing").size(32.0))
                        .min_size(egui::vec2(ui.available_width(), 100.0));
                    if ui.add(btn).clicked() { intent.start_worker = true; }
                    return;
                };

                match phase {
                    PairPhase::Starting => {
                        ui.heading(egui::RichText::new("Starting…").size(48.0));
                        ui.add_space(40.0);
                        ui.spinner();
                    }
                    PairPhase::Searching { my_fingerprint } => {
                        ui.heading(egui::RichText::new("Searching for PC…").size(48.0));
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(format!("This Deck: {my_fingerprint}"))
                                .monospace()
                                .size(16.0),
                        );
                        ui.add_space(40.0);
                        ui.spinner();
                    }
                    PairPhase::Prompt { name, fingerprint, my_fingerprint } => {
                        ui.heading(egui::RichText::new("Confirm peer").size(48.0));
                        ui.add_space(20.0);
                        ui.label(egui::RichText::new(format!("Found {name}")).size(28.0));
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!("Their fingerprint:\n{fingerprint}"))
                                .monospace()
                                .size(18.0),
                        );
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Verify on the PC tray that this Deck shows:\n{my_fingerprint}",
                            ))
                            .monospace()
                            .size(14.0),
                        );
                        ui.add_space(30.0);
                        ui.horizontal(|ui| {
                            let half = ui.available_width() / 2.0 - 8.0;
                            let accept = egui::Button::new(egui::RichText::new("Accept").size(28.0))
                                .min_size(egui::vec2(half, 80.0));
                            let reject = egui::Button::new(egui::RichText::new("Reject").size(28.0))
                                .min_size(egui::vec2(half, 80.0));
                            if ui.add(accept).clicked() { intent.accept = true; }
                            if ui.add(reject).clicked() { intent.reject = true; }
                        });
                    }
                    PairPhase::Confirming { peer_name, .. } => {
                        ui.heading(egui::RichText::new("Confirming…").size(48.0));
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Waiting for {peer_name} to confirm on their side."
                            ))
                            .size(20.0),
                        );
                        ui.add_space(40.0);
                        ui.spinner();
                    }
                    PairPhase::Done(_) => {} // handled above
                    PairPhase::Failed(reason) => {
                        ui.heading(egui::RichText::new("Pair failed").size(48.0));
                        ui.add_space(20.0);
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            egui::RichText::new(reason).size(20.0),
                        );
                        ui.add_space(30.0);
                        let btn = egui::Button::new(egui::RichText::new("Try again").size(28.0))
                            .min_size(egui::vec2(ui.available_width(), 80.0));
                        if ui.add(btn).clicked() { intent.reset_worker = true; }
                    }
                }
            });
        });

        // Apply intents — egui closure has released its borrow on `state`.
        if intent.accept {
            if let Some(w) = state.worker.as_ref() { w.accept(); }
        }
        if intent.reject {
            if let Some(w) = state.worker.as_ref() { w.reject(); }
        }
        if intent.reset_worker {
            state.worker = None;
            state.last_error = None;
        }
        if intent.start_worker {
            match PairWorker::start(
                state.identity.clone(),
                state.self_name.clone(),
                state.state_dir.clone(),
                ctx.clone(),
            ) {
                Ok(w) => { state.worker = Some(w); state.last_error = None; }
                Err(e) => {
                    state.last_error = Some(format!(
                        "Could not start pair: {e}\n(port 49152 may already be in use)"
                    ));
                }
            }
        }
        if intent.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn draw_daemon_failed(&mut self, ctx: &egui::Context) {
        let (status, tail): (Option<String>, Vec<String>) = self
            .daemon
            .as_ref()
            .map(|d| (d.exit_status(), d.snapshot_tail()))
            .unwrap_or_default();
        let can_escalate = self.self_exe.is_some();
        let mut intent_retry = false;
        let mut intent_pkexec = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(40.0);
                ui.heading(egui::RichText::new("Daemon stopped").size(48.0));
                ui.add_space(20.0);
                if let Some(s) = &status {
                    ui.label(egui::RichText::new(format!("Exit: {s}")).size(20.0));
                }
                ui.add_space(20.0);
                if tail.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "(no stderr captured — sudo -n likely refused to run)",
                        )
                        .size(18.0),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for line in tail.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
                                ui.label(
                                    egui::RichText::new(line)
                                        .monospace()
                                        .size(14.0),
                                );
                            }
                        });
                }

                if can_escalate {
                    ui.add_space(30.0);
                    ui.horizontal(|ui| {
                        let half = ui.available_width() / 2.0 - 8.0;
                        let retry = egui::Button::new(
                            egui::RichText::new("Restart daemon").size(22.0),
                        )
                        .min_size(egui::vec2(half, 64.0));
                        let pkexec = egui::Button::new(
                            egui::RichText::new("Run with password").size(22.0),
                        )
                        .min_size(egui::vec2(half, 64.0));
                        if ui.add(retry).clicked() { intent_retry = true; }
                        if ui.add(pkexec).clicked() { intent_pkexec = true; }
                    });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Restart re-runs `sudo -n` (relies on the NOPASSWD sudoers\n\
                             entry written during install).\n\
                             Run with password falls back to `pkexec` if the sudoers entry\n\
                             is missing or sudo refuses to run non-interactively.",
                        )
                        .size(13.0),
                    );
                }
            });
        });

        if intent_retry {
            self.respawn_daemon(Escalation::SudoNonInteractive);
        }
        if intent_pkexec {
            self.respawn_daemon(Escalation::Pkexec);
        }
    }

    fn draw_setup(&mut self, ctx: &egui::Context) {
        // Poll the installer child if one is running. Take ownership so we
        // can mutate the state machine; restore unless the child is done.
        if let Some(state) = self.setup.as_mut() {
            if let SetupPhase::Installing(child) = &mut state.phase {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => state.phase = SetupPhase::Done,
                    Ok(Some(status)) => {
                        state.phase = SetupPhase::Failed(format!(
                            "installer exited with {status}. Open a terminal and run\nsudo {} install\nto see the full error.",
                            state.self_exe.display(),
                        ));
                    }
                    Ok(None) => {} // still running
                    Err(e) => state.phase = SetupPhase::Failed(format!("waitpid: {e}")),
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(40.0);
                ui.heading(egui::RichText::new("First-time setup").size(48.0));
                ui.add_space(20.0);

                let Some(state) = self.setup.as_mut() else { return };
                match &state.phase {
                    SetupPhase::Idle => {
                        ui.label(
                            egui::RichText::new(
                                "Tap Install to set up the controller bridge.\n\
                                 You'll be prompted for your sudo password.",
                            )
                            .size(20.0),
                        );
                        ui.add_space(40.0);
                        let btn = egui::Button::new(
                            egui::RichText::new("Install").size(32.0),
                        )
                        .min_size(egui::vec2(ui.available_width(), 100.0));
                        if ui.add(btn).clicked() {
                            match spawn_installer(&state.self_exe) {
                                Ok(child) => state.phase = SetupPhase::Installing(child),
                                Err(e) => {
                                    state.phase = SetupPhase::Failed(format!(
                                        "Could not launch pkexec: {e}\n\nFallback: open a terminal and run\nsudo {} install",
                                        state.self_exe.display(),
                                    ));
                                }
                            }
                        }
                    }
                    SetupPhase::Installing(_) => {
                        ui.label(
                            egui::RichText::new(
                                "Authenticate in the password dialog…\n\
                                 (this can take ~30 s on first run while pacman installs usbip)",
                            )
                            .size(20.0),
                        );
                        ui.add_space(40.0);
                        ui.spinner();
                    }
                    SetupPhase::Done => {
                        ui.label(
                            egui::RichText::new(
                                "Setup complete. Close this window and relaunch from\n\
                                 Steam (Game Mode) or your application launcher.",
                            )
                            .size(20.0),
                        );
                    }
                    SetupPhase::Failed(msg) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, egui::RichText::new(msg).size(18.0));
                        ui.add_space(20.0);
                        let btn = egui::Button::new(
                            egui::RichText::new("Try again").size(28.0),
                        )
                        .min_size(egui::vec2(ui.available_width(), 80.0));
                        if ui.add(btn).clicked() {
                            state.phase = SetupPhase::Idle;
                        }
                    }
                }
            });
        });
    }
}

fn spawn_installer(self_exe: &std::path::Path) -> std::io::Result<Child> {
    // pkexec triggers the polkit auth agent (graphical password prompt on KDE
    // / GNOME / Steam Deck Desktop Mode). The child re-execs us with `install`,
    // which contains the real bootstrap logic.
    //
    // Pin self_exe to a system-owned path before elevating: pkexec runs the
    // target as root, so a user-writable path here is a one-step root.
    let canonical = self_exe.canonicalize().unwrap_or_else(|_| self_exe.to_path_buf());
    if !is_safe_install_source(&canonical) {
        return Err(std::io::Error::other(format!(
            "refusing to pkexec a binary outside the system tree: {}\n\
             re-run from {} or anywhere under /usr/.",
            canonical.display(),
            crate::install::INSTALL_BIN,
        )));
    }
    let pkexec = crate::install::absolute_path_for("pkexec")
        .ok_or_else(|| std::io::Error::other("pkexec not found in /usr/bin or /bin"))?;
    std::process::Command::new(pkexec)
        .arg(&canonical)
        .arg("install")
        .spawn()
}

/// `true` iff `path` is the canonical install location or anywhere under
/// `/usr/`. These trees are root-owned on every supported distro, so the
/// binary about to be elevated by `pkexec` can't have been swapped out by a
/// non-root attacker.
fn is_safe_install_source(path: &std::path::Path) -> bool {
    if path == std::path::Path::new(crate::install::INSTALL_BIN) {
        return true;
    }
    path.starts_with("/usr/")
}

#[derive(Debug, PartialEq, Eq)]
struct View {
    text: String,
    button_label: &'static str,
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
        let with_name = status(Some("desktop"), true, true, false);
        let v = derive_view(Some(&with_name));
        assert_eq!(v.text, "Connected to desktop");
        assert_eq!(v.button_label, "Disconnect");
        assert_eq!(v.toggle_to, Some(true));

        let without_name = status(None, true, true, false);
        let v = derive_view(Some(&without_name));
        assert_eq!(v.text, "Connected to client");
    }
}
