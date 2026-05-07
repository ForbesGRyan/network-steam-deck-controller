//! Platform-specific default state directory. Hand-rolled to avoid pulling
//! the `dirs` crate for two paths.

use std::path::PathBuf;

const APP: &str = "network-deck";

#[derive(Debug)]
pub enum StateDirError {
    NoHome,
    NoLocalAppData,
}

#[cfg(target_os = "linux")]
/// Returns the default state directory for this application on Linux.
///
/// # Errors
///
/// Returns [`StateDirError::NoHome`] if the `HOME` environment variable is
/// not set and `XDG_STATE_HOME` is also absent or empty.
pub fn default_state_dir() -> Result<PathBuf, StateDirError> {
    if let Ok(s) = std::env::var("XDG_STATE_HOME") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s).join(APP));
        }
    }
    let home = std::env::var("HOME").map_err(|_| StateDirError::NoHome)?;
    Ok(PathBuf::from(home).join(".local/state").join(APP))
}

#[cfg(target_os = "windows")]
/// Returns the default state directory for this application on Windows.
///
/// # Errors
///
/// Returns [`StateDirError::NoLocalAppData`] if the `LOCALAPPDATA`
/// environment variable is not set.
pub fn default_state_dir() -> Result<PathBuf, StateDirError> {
    let local = std::env::var("LOCALAPPDATA").map_err(|_| StateDirError::NoLocalAppData)?;
    Ok(PathBuf::from(local).join(APP))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
/// Returns the default state directory for this application on other platforms.
///
/// # Errors
///
/// Returns [`StateDirError::NoHome`] if the `HOME` environment variable is
/// not set.
pub fn default_state_dir() -> Result<PathBuf, StateDirError> {
    let home = std::env::var("HOME").map_err(|_| StateDirError::NoHome)?;
    Ok(PathBuf::from(home).join(".network-deck"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_xdg_overrides_home() {
        std::env::set_var("XDG_STATE_HOME", "/tmp/network-deck-test-xdg");
        let dir = default_state_dir().unwrap();
        assert!(dir.starts_with("/tmp/network-deck-test-xdg"));
        assert!(dir.ends_with(APP));
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_uses_local_appdata() {
        // Save and restore the env var to keep the test idempotent.
        let prev = std::env::var("LOCALAPPDATA").ok();
        std::env::set_var("LOCALAPPDATA", "C:\\test-local");
        let dir = default_state_dir().unwrap();
        assert!(dir.starts_with("C:\\test-local"));
        assert!(dir.ends_with(APP));
        match prev {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
    }
}
