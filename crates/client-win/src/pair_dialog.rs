//! Native Win32 pair flow used by the tray when the user clicks
//! "Pair new Deck...".
//!
//! Drives `discovery::pair::run_pair_with` against an existing UDP socket
//! handed in by the caller (so the tray can release its `discovery-recv`
//! thread, hand the socket here, and reclaim it on failure). For the
//! accept/reject prompt we use `MessageBoxW` — modal, native, no extra
//! dependency. On success the caller re-execs the tray so the new trust
//! file is picked up cleanly.

#![cfg(windows)]

use std::net::UdpSocket;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use discovery::pair::{Decision, PairConfig, PairOutcome, PairUI};
use discovery::TrustedPeer;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
};

/// Run pair against `sock`, surfacing the accept prompt + result via
/// `MessageBoxW`. Consumes the socket — on success and failure both, the
/// socket is dropped here. The caller decides what to do next based on the
/// returned outcome (typically: re-exec on `Paired`, re-bind + resume on
/// anything else).
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
    let mut ui = WinDialogUI;
    discovery::pair::run_pair_with(&cfg, &mut ui)
}

struct WinDialogUI;

impl PairUI for WinDialogUI {
    fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision {
        let body = format!(
            "Found Deck \"{name}\"\n\n\
             Fingerprint:\n{fingerprint}\n\n\
             Verify the same fingerprint shows on the Deck, then click Yes to accept.",
        );
        if message_box(&body, "Confirm Deck pairing", MB_YESNO | MB_ICONQUESTION) == IDYES {
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
        message_box(&body, "Network Deck", MB_OK | MB_ICONINFORMATION);
    }

    fn on_failed(&mut self, reason: &str) {
        message_box(reason, "Pair failed", MB_OK | MB_ICONERROR);
    }
}

fn message_box(body: &str, title: &str, flags: u32) -> i32 {
    let body_w = wide(body);
    let title_w = wide(title);
    // SAFETY: pointers are NUL-terminated, valid UTF-16. `hwnd = null` =
    // top-level dialog with no parent. flags are valid `MB_*` constants.
    unsafe { MessageBoxW(std::ptr::null_mut(), body_w.as_ptr(), title_w.as_ptr(), flags) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
