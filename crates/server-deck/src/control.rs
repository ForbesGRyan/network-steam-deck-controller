//! On-disk contract for daemon ↔ kiosk file-based IPC.
//!
//! The daemon publishes a `Status` JSON file on each tick describing the
//! current pairing/binding state. The kiosk creates a `paused` flag file to
//! request that the daemon hold off binding (or unbind), and removes it to
//! resume normal operation.
//!
//! Why files: zero deps, zero sockets, easy to inspect by hand, and the
//! kiosk crate (separate process, separate user) just needs read/write
//! access to a shared directory.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Snapshot of daemon state, written atomically to `status.json` on each tick.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Status {
    /// Display name of the trusted peer, or `None` if no peer ever paired.
    pub peer_name: Option<String>,
    /// True iff a beacon from the trusted peer was seen recently.
    pub peer_present: bool,
    /// True iff the Deck controller is currently bound to `usbip-host`.
    pub bound: bool,
    /// True iff the kiosk's `paused` flag file is present.
    pub paused: bool,
}

/// Atomically write `s` to `<dir>/status.json` via a tmp-file + rename.
pub fn write_status(dir: &Path, s: &Status) -> io::Result<()> {
    let tmp = dir.join("status.json.tmp");
    let final_path = dir.join("status.json");
    {
        let f = File::create(&tmp)?;
        serde_json::to_writer_pretty(&f, s)?;
        // Drop the file (closes it) before rename.
    }
    std::fs::rename(&tmp, &final_path)
}

/// Path to the `paused` flag file the kiosk creates/removes.
pub fn paused_flag_path(dir: &Path) -> PathBuf {
    dir.join("paused")
}

/// Returns true iff the `paused` flag file exists.
///
/// On any error (rare for `exists()`), returns false: the daemon would
/// rather mistakenly bind than mistakenly unbind.
pub fn is_paused(dir: &Path) -> bool {
    paused_flag_path(dir).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_status() -> Status {
        Status {
            peer_name: Some("ryan-pc".to_owned()),
            peer_present: true,
            bound: true,
            paused: false,
        }
    }

    #[test]
    fn write_status_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = sample_status();
        write_status(dir.path(), &s).unwrap();
        let raw = fs::read_to_string(dir.path().join("status.json")).unwrap();
        let parsed: Status = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn is_paused_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_paused(dir.path()));
    }

    #[test]
    fn is_paused_true_after_creating_flag() {
        let dir = tempfile::tempdir().unwrap();
        File::create(paused_flag_path(dir.path())).unwrap();
        assert!(is_paused(dir.path()));
    }

    #[test]
    fn write_status_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        write_status(dir.path(), &sample_status()).unwrap();
        assert!(!dir.path().join("status.json.tmp").exists());
        assert!(dir.path().join("status.json").exists());
    }
}
