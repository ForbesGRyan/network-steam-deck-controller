//! Pair flow used by the tray when the user clicks "Pair new Deck...".
//!
//! Drives `discovery::pair::run_pair_with` against an existing UDP socket
//! handed in by the caller (so the tray can release its `discovery-recv`
//! thread, hand the socket here, and reclaim it on failure). The
//! accept/reject prompt + result UI use the shared egui dialogs in
//! `crate::dialogs`. On success the caller re-execs the tray so the new
//! trust file is picked up cleanly.

use std::net::UdpSocket;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use discovery::pair::{Decision, PairConfig, PairOutcome, PairUI};
use discovery::TrustedPeer;

use crate::dialogs;

/// Run pair against `sock`, surfacing the accept prompt + result via the
/// shared egui dialogs. Consumes the socket — on success and failure both,
/// the socket is dropped here. The caller decides what to do next based on
/// the returned outcome (typically: re-exec on `Paired`, re-bind + resume
/// on anything else).
pub fn run(
    sock: UdpSocket,
    identity: Arc<discovery::Identity>,
    self_name: String,
    state_dir: &Path,
) -> PairOutcome {
    let cfg = PairConfig {
        identity,
        recv_sock: sock,
        targets: discovery::netifs::broadcast_targets(super::DEFAULT_PORT),
        self_name,
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(120),
    };
    let mut ui = EguiPairUI;
    discovery::pair::run_pair_with(&cfg, &mut ui)
}

struct EguiPairUI;

impl PairUI for EguiPairUI {
    fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision {
        let body = format!(
            "Found Deck \"{name}\".\n\n\
             Fingerprint:\n{fingerprint}\n\n\
             Verify the same fingerprint shows on the Deck, then click Accept.",
        );
        if dialogs::confirm("Confirm Deck pairing", &body, "Accept", "Reject") {
            Decision::Accept
        } else {
            Decision::Reject
        }
    }

    fn on_paired(&mut self, peer: &TrustedPeer) {
        let body = format!(
            "Paired with {}.\n\nThe tray will restart to pick up the new pairing.",
            peer.name,
        );
        dialogs::info("Network Deck", &body);
    }

    fn on_failed(&mut self, reason: &str) {
        dialogs::error("Pair failed", reason);
    }
}
