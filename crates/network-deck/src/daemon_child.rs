//! Supervises the daemon subprocess that the GUI launches.
//!
//! Spawns `sudo -n <self> daemon` and parks two helper threads:
//!
//!  * **stderr-tail** — drains the child's stderr into a shared ring buffer
//!    so the kiosk can surface failure messages in the UI.
//!  * **waiter** — blocks on `child.wait()`; flips the shared `alive` flag
//!    and records exit status when the child terminates.
//!
//! The `DaemonChild` handle keeps only the pid plus the shared state; on
//! `Drop` it sends `SIGTERM` by pid and the waiter thread reaps. We don't
//! hold the `Child` after spawn so the polling story stays simple.
//!
//! `sudo -n` is non-interactive: relies on the NOPASSWD sudoers entry that
//! `network-deck install` writes.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::install::absolute_path_for;

/// Shared, GUI-visible view of the daemon child.
#[derive(Default)]
pub struct DaemonState {
    pub alive: AtomicBool,
    pub stderr_tail: Mutex<Vec<String>>,
    pub exit_status: Mutex<Option<String>>,
}

impl DaemonState {
    #[must_use]
    pub fn snapshot_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().map(|v| v.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn exit_status(&self) -> Option<String> {
        self.exit_status.lock().ok().and_then(|s| s.clone())
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn record_exit(&self, status_str: String) {
        if let Ok(mut slot) = self.exit_status.lock() {
            if slot.is_none() {
                *slot = Some(status_str);
            }
        }
        self.alive.store(false, Ordering::Relaxed);
    }

    fn push_stderr_line(&self, line: String) {
        if let Ok(mut buf) = self.stderr_tail.lock() {
            buf.push(line);
            const MAX: usize = 200;
            if buf.len() > MAX {
                let drop = buf.len() - MAX;
                buf.drain(0..drop);
            }
        }
    }
}

pub struct DaemonChild {
    pid: u32,
    /// Waiter thread handle. We keep this so `Drop` can wait for the daemon
    /// to actually exit (= cleanup ran, controller unbound) instead of just
    /// blasting SIGTERM and racing the kernel.
    waiter: Option<std::thread::JoinHandle<()>>,
}

/// How to escalate when launching the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// `sudo -n` — silent, requires the NOPASSWD sudoers entry the
    /// installer writes. Default path post-setuid-removal.
    SudoNonInteractive,
    /// `pkexec` — pops the system-wide polkit auth dialog. Fallback when
    /// sudo isn't usable (no NOPASSWD entry, sudo binary missing, etc.).
    Pkexec,
}

impl DaemonChild {
    /// Spawn the daemon via `sudo -n` (the default escalation path).
    pub fn spawn(self_exe: &Path) -> std::io::Result<(Self, Arc<DaemonState>)> {
        Self::spawn_with(self_exe, Escalation::SudoNonInteractive)
    }

    pub fn spawn_with(
        self_exe: &Path,
        method: Escalation,
    ) -> std::io::Result<(Self, Arc<DaemonState>)> {
        let mut cmd = match method {
            Escalation::SudoNonInteractive => {
                let sudo = absolute_path_for("sudo").ok_or_else(|| {
                    std::io::Error::other("sudo not found in /usr/bin or /bin")
                })?;
                let mut c = Command::new(sudo);
                c.arg("-n").arg(self_exe);
                c
            }
            Escalation::Pkexec => {
                // pkexec runs the target as root — refuse if `self_exe`
                // sits in a user-writable tree (e.g. $HOME or /usr/local),
                // otherwise any local user can trojan the binary and ride
                // pkexec.
                let canonical = self_exe
                    .canonicalize()
                    .unwrap_or_else(|_| self_exe.to_path_buf());
                if !crate::install::is_safe_install_source(&canonical) {
                    return Err(std::io::Error::other(format!(
                        "refusing to pkexec a binary outside the system tree: {}",
                        canonical.display()
                    )));
                }
                let pkexec = absolute_path_for("pkexec").ok_or_else(|| {
                    std::io::Error::other("pkexec not found in /usr/bin or /bin")
                })?;
                let mut c = Command::new(pkexec);
                c.arg(&canonical);
                c
            }
        };
        let mut child = cmd
            .arg("daemon")
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child.id();
        let state = Arc::new(DaemonState::default());
        state.alive.store(true, Ordering::Relaxed);

        // stderr drain thread.
        if let Some(stderr) = child.stderr.take() {
            let state2 = state.clone();
            std::thread::Builder::new()
                .name("daemon-stderr".into())
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        eprintln!("[daemon] {line}");
                        state2.push_stderr_line(line);
                    }
                })
                .ok();
        }

        // Waiter thread — owns the Child for its lifetime and records the
        // exit status when it returns.
        let state3 = state.clone();
        let waiter = std::thread::Builder::new()
            .name("daemon-waiter".into())
            .spawn(move || {
                let mut child = child;
                match child.wait() {
                    Ok(status) => state3.record_exit(format!("{status}")),
                    Err(e) => state3.record_exit(format!("wait error: {e}")),
                }
            })
            .ok();

        Ok((Self { pid, waiter }, state))
    }

    /// Send SIGTERM without blocking. Pair with `is_finished()` polling so
    /// the GUI can paint a "Stopping…" frame instead of freezing on Drop.
    /// Idempotent — extra SIGTERMs to a dead pid are harmless.
    pub fn request_shutdown(&self) {
        unsafe {
            #[allow(clippy::cast_possible_wrap)]
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
    }

    /// Send SIGKILL. Last-resort escalation when the daemon ignored SIGTERM
    /// past the GUI's grace window. Controller may stay bound to usbip-host
    /// in that case.
    pub fn force_kill(&self) {
        unsafe {
            #[allow(clippy::cast_possible_wrap)]
            libc::kill(self.pid as i32, libc::SIGKILL);
        }
    }

    /// True once the waiter thread has reaped the child. Cheap; safe to
    /// call every frame.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.waiter.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let Some(handle) = self.waiter.take() else { return };
        // Fast path: GUI drove shutdown via request_shutdown + polling and
        // only dropped us once is_finished() returned true. Just join.
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        // Fallback path: abnormal exit (panic, untracked Drop). Best-effort
        // SIGTERM, brief wait, then SIGKILL — bounded so we don't freeze on
        // process teardown.
        unsafe {
            #[allow(clippy::cast_possible_wrap)]
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if handle.is_finished() { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !handle.is_finished() {
            eprintln!("daemon ignored SIGTERM after 500ms in Drop — SIGKILL");
            unsafe {
                #[allow(clippy::cast_possible_wrap)]
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
        let _ = handle.join();
    }
}
