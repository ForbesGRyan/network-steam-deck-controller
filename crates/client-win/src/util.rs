//! Small Win32 helpers shared by the tray, autostart, and pair-dialog
//! modules.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// `&str` → NUL-terminated UTF-16 buffer, suitable for any `*const u16`
/// Win32 entry point. Goes through `OsStr::encode_wide` so unpaired
/// surrogates round-trip the same way Win32 itself encodes them.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Spawn the current exe as a fresh process and exit this one. Used after
/// pair completion so the new process re-reads the trust file from a clean
/// state. Diverges; never returns. Caller is responsible for dropping any
/// state whose `Drop` matters (tray icon, sockets) BEFORE calling — this
/// path uses `process::exit`, which skips destructors.
pub fn reexec_self() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}
