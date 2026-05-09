//! Auto-install of usbip-win2 when `usbip.exe` is missing.
//!
//! Prompts the user via an egui dialog, downloads the installer with
//! `curl.exe` (bundled with Windows 10/11), launches it elevated via
//! `ShellExecuteExW` with the `runas` verb, waits for it to exit, then
//! re-checks `locate()`. Status is surfaced through a spinner dialog.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::dialogs;
use crate::usbip_cli;
use crate::util::wide;

const RELEASE_URL: &str =
    "https://github.com/vadimgrn/usbip-win2/releases/download/v.0.9.7.7/USBip-0.9.7.7-x64.exe";

/// If `usbip.exe` is missing, prompt the user and auto-install. Exits the
/// process on declined or failed installs so the caller can assume usbip is
/// available on return.
pub fn ensure_installed_or_exit() {
    if usbip_cli::locate().is_ok() {
        return;
    }
    if !run_install_flow() {
        std::process::exit(1);
    }
}

fn run_install_flow() -> bool {
    let confirmed = dialogs::confirm(
        "Network Deck — install required",
        "usbip-win2 is required to attach the Deck and is not installed.\n\n\
         Install it silently now? You'll see a UAC prompt and a Windows \
         driver-signature dialog; the rest of the install runs in the \
         background.",
        "Install",
        "Cancel",
    );
    if !confirmed {
        return false;
    }

    let install_result: Result<(), String> = dialogs::with_progress(
        "Network Deck — installing usbip-win2",
        "Downloading installer…",
        |handle| {
            let installer = download_installer().map_err(|e| format!("Download failed: {e}"))?;
            handle.set_status("Waiting for UAC approval…");
            run_elevated_and_wait(&installer, &handle)
                .map_err(|e| format!("Installer launch failed: {e}"))?;
            Ok(())
        },
    );

    if let Err(e) = install_result {
        dialogs::error("Network Deck", &e);
        return false;
    }

    if usbip_cli::locate().is_ok() {
        true
    } else {
        dialogs::error(
            "Network Deck",
            "usbip-win2 installer finished but usbip.exe was not found.\n\
             Re-run the installer manually, then re-launch Network Deck.",
        );
        false
    }
}

fn download_installer() -> Result<PathBuf, String> {
    let dest = std::env::temp_dir().join("usbip-win2-installer.exe");
    let status = Command::new("curl.exe")
        .args(["-L", "-fSs", "-o"])
        .arg(&dest)
        .arg(RELEASE_URL)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if !status.success() {
        return Err(format!("curl exited with {:?}", status.code()));
    }
    Ok(dest)
}

fn run_elevated_and_wait(
    installer: &Path,
    progress: &dialogs::ProgressHandle,
) -> Result<(), String> {
    let verb = wide("runas");
    let file = wide(&installer.to_string_lossy());
    // Inno Setup silent flags + the "Compact installation" preset that
    // selects only Main Files + Client (vhci driver + usbip.exe). Skips
    // the wizard, suppresses message boxes, and avoids an automatic
    // reboot. The Windows driver-consent dialog can still surface — that
    // one is enforced by Windows and not suppressible here.
    let params = wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /TYPE=compact");
    // SAFETY: zero-initialized struct is a valid SHELLEXECUTEINFOW; we then
    // populate the fields we use. Trailing fields stay null/zero, which the
    // API accepts.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>())
        .expect("SHELLEXECUTEINFOW size fits in u32");
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    // SAFETY: structure fully initialized; pointers valid for the call.
    let ok = unsafe { ShellExecuteExW(&raw mut info) };
    if ok == 0 {
        return Err(format!(
            "ShellExecuteExW: {}",
            std::io::Error::last_os_error()
        ));
    }
    if info.hProcess.is_null() {
        return Err("installer did not return a process handle".into());
    }

    // Poll the wait so we can keep the UI alive with elapsed-time
    // updates. The Inno Setup silent install runs in the background, but
    // Windows still surfaces a separate driver-consent prompt for the
    // vhci kernel driver — and that prompt sometimes appears behind our
    // window. The status text below tells the user where to look.
    progress.set_status(
        "Installing usbip-win2…\n\n\
         If a Windows driver-install prompt appears, accept it.\n\
         (Check the taskbar — it sometimes opens behind this window.)",
    );

    let start = Instant::now();
    let mut next_tick_secs: u64 = 5;
    let exit_ok = loop {
        // SAFETY: handle came from ShellExecuteExW with
        // SEE_MASK_NOCLOSEPROCESS; we own it and close it below.
        let rc = unsafe { WaitForSingleObject(info.hProcess, 500) };
        if rc == WAIT_OBJECT_0 {
            break true;
        }
        if rc != WAIT_TIMEOUT {
            break false;
        }
        let elapsed = start.elapsed().as_secs();
        if elapsed >= next_tick_secs {
            progress.set_status(&format!(
                "Installing usbip-win2 — {elapsed}s elapsed.\n\n\
                 If a Windows driver-install prompt appears, accept it.\n\
                 (Check the taskbar — it sometimes opens behind this window.)",
            ));
            next_tick_secs = elapsed + 5;
        }
    };

    // SAFETY: same handle, closing exactly once.
    unsafe {
        CloseHandle(info.hProcess);
    }
    if exit_ok {
        Ok(())
    } else {
        Err("WaitForSingleObject failed".into())
    }
}
