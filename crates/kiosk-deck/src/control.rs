//! Kiosk-side mirror of the daemon's file-based control surface.
//!
//! The daemon (`server-deck`) publishes `<dir>/status.json` on each tick.
//! The kiosk reads that file and toggles a `<dir>/paused` flag file to
//! request the daemon hold off binding the controller.
//!
//! The `Status` struct is duplicated from `server-deck::control` on
//! purpose: there is no shared library crate, and the on-disk JSON shape
//! is the contract between the two binaries.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Mirror of `server-deck::control::Status`. The on-disk JSON shape is the contract.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Status {
    pub peer_name: Option<String>,
    pub peer_present: bool,
    pub bound: bool,
    pub paused: bool,
}

#[must_use]
pub fn paused_flag_path(dir: &Path) -> PathBuf {
    dir.join("paused")
}

/// Returns the parsed status, or `None` on any read/parse error.
/// The kiosk treats `None` as "daemon not running" in its UI.
#[must_use]
pub fn read_status(dir: &Path) -> Option<Status> {
    let path = dir.join("status.json");
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Touches or removes the `paused` flag file.
pub fn set_paused(dir: &Path, paused: bool) -> io::Result<()> {
    let path = paused_flag_path(dir);
    if paused {
        // Touch semantics: we only care that the file exists. `truncate(false)`
        // is explicit that we do not want to wipe contents if the file is
        // already there — the daemon only checks existence either way.
        match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()), // already removed
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_status_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_status(dir.path()).is_none());
    }

    #[test]
    fn read_status_returns_none_on_garbage() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("status.json"), b"not json").unwrap();
        assert!(read_status(dir.path()).is_none());
    }

    #[test]
    fn read_status_round_trips_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let s = Status {
            peer_name: Some("desktop".to_owned()),
            peer_present: true,
            bound: true,
            paused: false,
        };
        // Hand-roll the JSON: this test must not depend on server-deck.
        // Shape matches what `serde_json::to_writer_pretty` would emit for
        // the daemon's Status struct.
        let json = r#"{
  "peer_name": "desktop",
  "peer_present": true,
  "bound": true,
  "paused": false
}"#;
        fs::write(dir.path().join("status.json"), json).unwrap();
        let parsed = read_status(dir.path()).expect("should parse");
        assert_eq!(parsed, s);
    }

    #[test]
    fn set_paused_true_creates_flag_file() {
        let dir = tempfile::tempdir().unwrap();
        set_paused(dir.path(), true).unwrap();
        assert!(paused_flag_path(dir.path()).exists());
    }

    #[test]
    fn set_paused_false_removes_flag_file() {
        let dir = tempfile::tempdir().unwrap();
        set_paused(dir.path(), true).unwrap();
        assert!(paused_flag_path(dir.path()).exists());
        set_paused(dir.path(), false).unwrap();
        assert!(!paused_flag_path(dir.path()).exists());
    }

    #[test]
    fn set_paused_false_is_idempotent_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        // No flag file present at all — should still succeed.
        set_paused(dir.path(), false).unwrap();
        assert!(!paused_flag_path(dir.path()).exists());
    }

    #[test]
    fn set_paused_true_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        set_paused(dir.path(), true).unwrap();
        set_paused(dir.path(), true).unwrap();
        assert!(paused_flag_path(dir.path()).exists());
    }
}
