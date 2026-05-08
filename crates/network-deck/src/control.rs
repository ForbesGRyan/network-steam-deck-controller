//! File-based daemon ↔ kiosk IPC.
//!
//! The daemon writes `<dir>/status.json` on each tick; the kiosk reads it.
//! The kiosk creates/removes `<dir>/paused` to request that the daemon hold
//! off binding the controller. Two writers, no shared mutable state — both
//! sides only mutate their own file. The JSON shape is the contract.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[must_use]
pub fn is_paused(dir: &Path) -> bool {
    paused_flag_path(dir).exists()
}

/// Atomic write of `s` to `<dir>/status.json` via tmp-file + rename.
pub fn write_status(dir: &Path, s: &Status) -> io::Result<()> {
    let tmp = dir.join("status.json.tmp");
    let final_path = dir.join("status.json");
    {
        let f = fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&f, s)?;
    }
    fs::rename(&tmp, &final_path)
}

/// Remove `<dir>/status.json` so the next read sees "daemon not running"
/// rather than a stale snapshot. Missing file is fine.
pub fn clear_status(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join("status.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[must_use]
pub fn read_status(dir: &Path) -> Option<Status> {
    let bytes = fs::read(dir.join("status.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn set_paused(dir: &Path, paused: bool) -> io::Result<()> {
    let path = paused_flag_path(dir);
    if paused {
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map(|_| ())
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Status {
        Status {
            peer_name: Some("desktop".into()),
            peer_present: true,
            bound: true,
            paused: false,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = sample();
        write_status(dir.path(), &s).unwrap();
        assert_eq!(read_status(dir.path()), Some(s));
    }

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
    fn write_status_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        write_status(dir.path(), &sample()).unwrap();
        assert!(!dir.path().join("status.json.tmp").exists());
    }

    #[test]
    fn clear_status_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        write_status(dir.path(), &sample()).unwrap();
        clear_status(dir.path()).unwrap();
        assert!(read_status(dir.path()).is_none());
    }

    #[test]
    fn clear_status_idempotent_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        clear_status(dir.path()).unwrap();
    }

    #[test]
    fn paused_flag_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_paused(dir.path()));
        set_paused(dir.path(), true).unwrap();
        assert!(is_paused(dir.path()));
        set_paused(dir.path(), false).unwrap();
        assert!(!is_paused(dir.path()));
    }

    #[test]
    fn set_paused_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        set_paused(dir.path(), true).unwrap();
        set_paused(dir.path(), true).unwrap();
        set_paused(dir.path(), false).unwrap();
        set_paused(dir.path(), false).unwrap();
    }
}
