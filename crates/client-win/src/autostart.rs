//! Manage the `HKCU\...\Run` entry that autostarts client-win at login.
//!
//! User-scope registry write — no admin needed.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const VALUE_NAME: &str = "NetworkDeck";
const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Set the autostart entry to the path of the currently-running exe. Idempotent.
///
/// # Errors
/// Returns the Win32 error code on failure.
pub fn enable() -> Result<(), u32> {
    let exe = std::env::current_exe().map_err(|_| 0_u32)?;
    let exe_str = exe.to_string_lossy().into_owned();
    write_run_value(&exe_str)
}

/// Remove our autostart entry. No-op if absent.
///
/// # Errors
/// Returns the Win32 error code on failure (`ERROR_FILE_NOT_FOUND` is treated as success).
#[allow(dead_code)] // public API; tray "disable autostart" toggle not wired yet
pub fn disable() -> Result<(), u32> {
    let subkey = wide(SUBKEY);
    let value = wide(VALUE_NAME);
    let mut key: HKEY = std::ptr::null_mut();
    unsafe {
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &raw mut key);
        if r == 2 {
            // Run key absent — nothing to disable.
            return Ok(());
        }
        if r != ERROR_SUCCESS {
            return Err(r);
        }
        let r = RegDeleteValueW(key, value.as_ptr());
        let _ = RegCloseKey(key);
        // ERROR_FILE_NOT_FOUND = 2; treat as already-disabled.
        if r != ERROR_SUCCESS && r != 2 {
            return Err(r);
        }
    }
    Ok(())
}

/// Returns the current Run-key value if set.
#[must_use]
pub fn current() -> Option<String> {
    let subkey = wide(SUBKEY);
    let value = wide(VALUE_NAME);
    let mut key: HKEY = std::ptr::null_mut();
    unsafe {
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &raw mut key)
            != ERROR_SUCCESS
        {
            return None;
        }
        let mut size: u32 = 0;
        let r = RegQueryValueExW(
            key,
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        );
        if r != ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            return None;
        }
        // Round up: a malformed REG_SZ with odd byte count would otherwise
        // under-allocate by one element and let the second RegQueryValueExW
        // write past the buffer.
        let mut buf = vec![0_u16; size.div_ceil(2) as usize];
        let r = RegQueryValueExW(
            key,
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &raw mut size,
        );
        let _ = RegCloseKey(key);
        if r != ERROR_SUCCESS {
            return None;
        }
        // Trim trailing NUL.
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf16_lossy(&buf))
    }
}

fn write_run_value(exe: &str) -> Result<(), u32> {
    let subkey = wide(SUBKEY);
    let value_name = wide(VALUE_NAME);
    let exe_w = wide(exe);
    let mut key: HKEY = std::ptr::null_mut();
    unsafe {
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &raw mut key);
        if r != ERROR_SUCCESS {
            return Err(r);
        }
        let bytes = exe_w.len() * 2;
        #[allow(clippy::cast_possible_truncation)]
        let len = bytes as u32;
        let r = RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_SZ,
            exe_w.as_ptr().cast(),
            len,
        );
        let _ = RegCloseKey(key);
        if r != ERROR_SUCCESS {
            return Err(r);
        }
    }
    Ok(())
}
