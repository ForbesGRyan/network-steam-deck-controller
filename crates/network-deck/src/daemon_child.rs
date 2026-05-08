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
    /// Direct exec. Assumes the installed binary is setuid root, which
    /// the installer ensures. Cleanest path: no sudo or pkexec layer,
    /// no auth prompts, daemon runs as root via the kernel's setuid bit.
    Direct,
    /// `sudo -n` — silent, requires the NOPASSWD sudoers entry the
    /// installer also writes (defense in depth if setuid is stripped).
    #[allow(dead_code)] // reserved fallback; spawn path is wired but no caller selects it yet
    SudoNonInteractive,
    /// `pkexec` — pops the system-wide polkit auth dialog. Last-resort
    /// fallback for when both setuid and sudo are unavailable.
    Pkexec,
}

impl DaemonChild {
    /// Spawn the daemon via direct exec (relies on setuid bit) and start
    /// the helper threads.
    pub fn spawn(self_exe: &Path) -> std::io::Result<(Self, Arc<DaemonState>)> {
        Self::spawn_with(self_exe, Escalation::Direct)
    }

    pub fn spawn_with(
        self_exe: &Path,
        method: Escalation,
    ) -> std::io::Result<(Self, Arc<DaemonState>)> {
        let mut cmd = match method {
            Escalation::Direct => Command::new(self_exe),
            Escalation::SudoNonInteractive => {
                let mut c = Command::new("sudo");
                c.arg("-n").arg(self_exe);
                c
            }
            Escalation::Pkexec => {
                let mut c = Command::new("pkexec");
                c.arg(self_exe);
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
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        // SIGTERM the daemon — its signal handler triggers `usbip unbind`
        // and clears /run/.../status.json before exiting.
        unsafe {
            #[allow(clippy::cast_possible_wrap)]
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
        // Wait up to 5 s for the daemon to actually exit — this is what
        // releases the controller back to the Deck. `usbip unbind` shells
        // out to the userspace tool, so it can take ~hundreds of ms even
        // on a fast machine.
        let Some(handle) = self.waiter.take() else { return };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if handle.is_finished() { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // If the daemon hasn't exited by the deadline, escalate to SIGKILL
        // so we don't hang the GUI on close. Controller may stay bound to
        // usbip-host in that case — pathological and worth logging.
        if !handle.is_finished() {
            eprintln!("daemon ignored SIGTERM after 5s — escalating to SIGKILL");
            unsafe {
                #[allow(clippy::cast_possible_wrap)]
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
        let _ = handle.join();
    }
}
