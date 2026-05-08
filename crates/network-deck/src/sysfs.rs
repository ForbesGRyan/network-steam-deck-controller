//! Find the Deck controller's USB busid by walking sysfs.

use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub enum SysfsError {
    Io(#[allow(dead_code)] io::Error),
    NotFound,
}

impl From<io::Error> for SysfsError {
    fn from(e: io::Error) -> Self { Self::Io(e) }
}

pub fn find_deck_busid(root: &Path, vid: &str, pid: &str) -> Result<String, SysfsError> {
    let dir = root.join("bus/usb/devices");
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if read_trim(&path.join("idVendor")).as_deref() == Some(vid)
            && read_trim(&path.join("idProduct")).as_deref() == Some(pid)
        {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                return Ok(name.to_owned());
            }
        }
    }
    Err(SysfsError::NotFound)
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_device(root: &Path, busid: &str, vid: &str, pid: &str) {
        let dir = root.join("bus/usb/devices").join(busid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("idVendor"), format!("{vid}\n")).unwrap();
        fs::write(dir.join("idProduct"), format!("{pid}\n")).unwrap();
    }

    #[test]
    fn finds_deck_among_multiple_devices() {
        let root = tempdir().unwrap();
        make_device(root.path(), "1-1", "1234", "5678");
        make_device(root.path(), "3-3", discovery::DECK_VID, discovery::DECK_PID);
        make_device(root.path(), "usb1", "1d6b", "0002");
        let busid = find_deck_busid(root.path(), discovery::DECK_VID, discovery::DECK_PID).unwrap();
        assert_eq!(busid, "3-3");
    }

    #[test]
    fn returns_not_found_when_absent() {
        let root = tempdir().unwrap();
        make_device(root.path(), "1-1", "1234", "5678");
        let err = find_deck_busid(root.path(), discovery::DECK_VID, discovery::DECK_PID);
        assert!(matches!(err, Err(SysfsError::NotFound)));
    }

    #[test]
    fn handles_missing_devices_dir() {
        let root = tempdir().unwrap();
        let err = find_deck_busid(root.path(), discovery::DECK_VID, discovery::DECK_PID);
        assert!(matches!(err, Err(SysfsError::Io(_))));
    }

    #[test]
    fn ignores_directories_missing_vid_pid_files() {
        let root = tempdir().unwrap();
        let dir = root.path().join("bus/usb/devices/usb1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("idVendor"), "1d6b\n").unwrap();
        make_device(root.path(), "3-3", discovery::DECK_VID, discovery::DECK_PID);
        let busid = find_deck_busid(root.path(), discovery::DECK_VID, discovery::DECK_PID).unwrap();
        assert_eq!(busid, "3-3");
    }
}
