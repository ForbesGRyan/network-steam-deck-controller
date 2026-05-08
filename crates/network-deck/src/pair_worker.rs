//! Background pair flow driven by a worker thread + channels, surfaced as
//! a tab in the kiosk UI.
//!
//! The worker calls `discovery::pair::run_pair_with` (the headless state
//! machine) using a `ChannelPairUI` adapter that:
//!   * publishes phase changes to a `Mutex<Phase>` the GUI reads,
//!   * blocks on a `mpsc::Receiver<Decision>` for the accept/reject step,
//!   * triggers an egui repaint on every transition so the UI updates
//!     without polling.
//!
//! Pair runs in-process (kiosk user). Trust file lands in
//! `~/.local/state/network-deck/`. The daemon, under `sudo -n`, resolves
//! the same path via `$SUDO_USER` (see `default_state_dir` in main).

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use discovery::pair::{Decision, PairConfig, PairOutcome, PairUI};
use discovery::trust::TrustedPeer;

/// What the GUI should currently render. Updated by the worker on every
/// state-machine transition; read by the eframe `update` loop.
#[derive(Debug, Clone)]
pub enum Phase {
    Starting,
    Searching { my_fingerprint: String },
    Prompt { name: String, fingerprint: String, my_fingerprint: String },
    Confirming { peer_name: String, my_fingerprint: String },
    Done(TrustedPeer),
    Failed(String),
}

pub struct PairWorker {
    pub phase: Arc<Mutex<Phase>>,
    decision_tx: mpsc::SyncSender<Decision>,
    handle: Option<JoinHandle<PairOutcome>>,
}

impl PairWorker {
    pub fn start(
        identity: Arc<discovery::Identity>,
        self_name: String,
        state_dir: PathBuf,
        repaint: eframe::egui::Context,
    ) -> std::io::Result<Self> {
        use std::net::{Ipv4Addr, UdpSocket};

        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 49152))?;
        sock.set_broadcast(true).ok();

        let cfg = PairConfig {
            identity,
            recv_sock: sock,
            targets: discovery::netifs::broadcast_targets(49152),
            self_name,
            state_dir,
            timeout: Duration::from_secs(120),
        };

        let phase = Arc::new(Mutex::new(Phase::Starting));
        // SyncSender(1) so the UI's button click never blocks even if the
        // worker has moved past the prompt.
        let (decision_tx, decision_rx) = mpsc::sync_channel::<Decision>(1);

        let phase2 = phase.clone();
        let handle = std::thread::Builder::new()
            .name("pair-worker".into())
            .spawn(move || {
                let mut ui = ChannelPairUI {
                    phase: phase2,
                    decision_rx,
                    repaint,
                };
                discovery::pair::run_pair_with(&cfg, &mut ui)
            })?;

        Ok(Self {
            phase,
            decision_tx,
            handle: Some(handle),
        })
    }

    pub fn accept(&self) {
        let _ = self.decision_tx.try_send(Decision::Accept);
    }

    pub fn reject(&self) {
        let _ = self.decision_tx.try_send(Decision::Reject);
    }

    /// Reaped on Drop; this just exposes whether the thread has exited.
    #[allow(dead_code)] // public observability API for future UI polling
    pub fn finished(&self) -> bool {
        self.handle.as_ref().map(|h| h.is_finished()).unwrap_or(true)
    }
}

impl Drop for PairWorker {
    fn drop(&mut self) {
        // If the worker is parked at a prompt, Reject lets it unwind right
        // away. If it's in Phase 1 (recv loop), the recv socket has a
        // 200 ms read timeout and the timeout itself is bounded — so the
        // worker exits at worst after `cfg.timeout`. We can't wait that
        // long on the GUI thread, so we hand the join off to a short-lived
        // reaper and return immediately.
        let _ = self.decision_tx.try_send(Decision::Reject);
        let Some(handle) = self.handle.take() else { return };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        loop {
            if handle.is_finished() {
                let _ = handle.join();
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Worker still alive — detach the join into a reaper so the GUI
        // thread isn't blocked. The reaper holds the handle until the
        // worker eventually exits (capped by `PairConfig.timeout`, 120 s).
        std::thread::Builder::new()
            .name("pair-reaper".into())
            .spawn(move || {
                let _ = handle.join();
            })
            .ok();
    }
}

struct ChannelPairUI {
    phase: Arc<Mutex<Phase>>,
    decision_rx: mpsc::Receiver<Decision>,
    repaint: eframe::egui::Context,
}

impl PairUI for ChannelPairUI {
    fn on_started(&mut self, my_fingerprint: &str, _self_name: &str) {
        self.set_phase(Phase::Searching {
            my_fingerprint: my_fingerprint.to_owned(),
        });
    }

    fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision {
        let my_fp = self.current_my_fp();
        self.set_phase(Phase::Prompt {
            name: name.to_owned(),
            fingerprint: fingerprint.to_owned(),
            my_fingerprint: my_fp.clone(),
        });
        let decision = self.decision_rx.recv().unwrap_or(Decision::Reject);
        if matches!(decision, Decision::Accept) {
            self.set_phase(Phase::Confirming {
                peer_name: name.to_owned(),
                my_fingerprint: my_fp,
            });
        } else {
            self.set_phase(Phase::Searching {
                my_fingerprint: my_fp,
            });
        }
        decision
    }

    fn on_paired(&mut self, peer: &TrustedPeer) {
        self.set_phase(Phase::Done(peer.clone()));
    }

    fn on_failed(&mut self, reason: &str) {
        self.set_phase(Phase::Failed(reason.to_owned()));
    }
}

impl ChannelPairUI {
    fn set_phase(&self, new: Phase) {
        if let Ok(mut p) = self.phase.lock() {
            *p = new;
        }
        self.repaint.request_repaint();
    }

    fn current_my_fp(&self) -> String {
        match self.phase.lock().ok().as_deref() {
            Some(Phase::Searching { my_fingerprint })
            | Some(Phase::Prompt { my_fingerprint, .. })
            | Some(Phase::Confirming { my_fingerprint, .. }) => my_fingerprint.clone(),
            _ => String::new(),
        }
    }
}
