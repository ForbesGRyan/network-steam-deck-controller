//! Idle/sleep inhibitor — keeps the Deck awake while the controller is bound.
//!
//! Without this, SteamOS auto-suspends after ~15 min of no on-Deck input even
//! though the controller is actively bridging URBs to a Windows host. We hold
//! a logind `idle:sleep` block inhibitor for as long as `usbip-host` is bound,
//! and release it the instant we unbind so paused / idle decks sleep normally.
//!
//! Implementation: spawn `systemd-inhibit` via `Command` (no shell, argv-safe)
//! and let it hold the lock for the lifetime of a `sleep infinity` child. A
//! new process group is created via `setsid` so `Drop` can SIGTERM the whole
//! tree — otherwise killing only `systemd-inhibit` would orphan `sleep` and
//! leak the lock until reboot.
//!
//! Compositor-agnostic: logind blocks `systemctl suspend` regardless of
//! whether the user is in KDE Desktop Mode or gamescope Game Mode. Screen
//! blanking is *not* covered — gamescope handles DPMS itself and ignores the
//! `org.freedesktop.ScreenSaver` DBus interface — but per ARCHITECTURE.md
//! Open Risks, that's an accepted non-goal: screen-off is fine, suspend is not.

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::install::absolute_path_for;

/// How long to wait after SIGTERM before escalating to SIGKILL on Drop.
/// `systemd-inhibit` + `sleep` should exit on TERM in milliseconds; this
/// window is generous so genuinely-running shutdown isn't aborted, but
/// finite so a wedged child can't block daemon teardown forever.
#[cfg(target_os = "linux")]
const DROP_TERM_GRACE: Duration = Duration::from_millis(500);

#[cfg(target_os = "linux")]
pub struct IdleInhibit {
    child: Child,
}

#[cfg(target_os = "linux")]
impl IdleInhibit {
    /// Spawn `systemd-inhibit ... sleep infinity` and hold the lock until
    /// `Drop`. Returns `None` if `systemd-inhibit` isn't installed or the
    /// spawn fails — logged, non-fatal: a Deck that occasionally suspends
    /// mid-game is preferable to a daemon that refuses to start.
    #[must_use]
    pub fn acquire() -> Option<Self> {
        let inhibit = match absolute_path_for("systemd-inhibit") {
            Some(p) => p,
            None => {
                eprintln!("inhibit: systemd-inhibit not found; Deck may auto-suspend while bound");
                return None;
            }
        };
        let sleep = absolute_path_for("sleep")
            .unwrap_or_else(|| std::path::PathBuf::from("/usr/bin/sleep"));

        let mut cmd = Command::new(&inhibit);
        cmd.args([
            "--what=idle:sleep",
            "--who=network-deck",
            "--why=Bridging Steam Deck controller to PC",
            "--mode=block",
        ]);
        cmd.arg(sleep).arg("infinity");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // New session/pgrp so Drop's `kill(-pid, ...)` reaches systemd-inhibit
        // *and* its sleep child in one shot. Without this, killing only the
        // parent leaves `sleep infinity` orphaned and the inhibit fd open.
        // SAFETY: pre_exec runs in the child after fork, before exec. setsid
        // is async-signal-safe and touches no allocator state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                eprintln!(
                    "inhibit: acquired idle:sleep lock (systemd-inhibit pid {})",
                    child.id()
                );
                Some(Self { child })
            }
            Err(e) => {
                eprintln!("inhibit: spawn systemd-inhibit failed: {e}");
                None
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for IdleInhibit {
    fn drop(&mut self) {
        let Ok(pgid) = i32::try_from(self.child.id()) else {
            // Linux PIDs fit in i32; this branch is unreachable on real
            // kernels but keeps Drop infallible without an unwrap.
            eprintln!("inhibit: pid out of i32 range, leaking inhibitor");
            return;
        };
        // SIGTERM the whole group; setsid in pre_exec made child.id() == pgid.
        // SAFETY: kill(2) with a negative pid is well-defined; no Rust state
        // is touched.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }

        // Poll try_wait until the grace window elapses. If TERM is honoured
        // (the normal case) we exit within a few ms. If the child is wedged
        // we escalate to SIGKILL so the daemon's shutdown path can't hang.
        let deadline = Instant::now() + DROP_TERM_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    eprintln!("inhibit: SIGTERM ignored after {DROP_TERM_GRACE:?}, sending SIGKILL");
                    // SAFETY: same as above — kill(2) on a negative pgid.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                    let _ = self.child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    eprintln!("inhibit: try_wait failed: {e}");
                    break;
                }
            }
        }
        eprintln!("inhibit: released idle:sleep lock");
    }
}
