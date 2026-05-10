//! Bind/unbind the Deck controller to `usbip-host` based on beacon state.
//!
//! Decoupled from the actual command invocation via `CommandRunner` so the
//! state transitions are unit-testable. Production wiring uses `RealRunner`,
//! which shells out to `/usr/bin/usbip`.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Idle,
    Bound,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    Bind,
    Unbind,
}

pub trait CommandRunner {
    fn run_usbip(&mut self, args: &[&str]) -> bool;
}

pub struct RealRunner {
    usbip: PathBuf,
}

#[cfg(target_os = "linux")]
impl RealRunner {
    /// Resolve `usbip` once via the same $PATH-hardening helper used by the
    /// rest of the codebase. Falls back to a bare `usbip` if absent so dev
    /// environments without a system install still work — logged so it's
    /// obvious why a later bind might fail.
    ///
    /// Linux-only because `crate::install` is gated to Linux (the rest of
    /// the install logic depends on libc/sudoers semantics). Tests use the
    /// `MockRunner` and don't need this constructor.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn new() -> Self {
        let usbip = crate::install::absolute_path_for("usbip").unwrap_or_else(|| {
            eprintln!("warning: usbip not found in standard bin dirs; falling back to PATH lookup");
            PathBuf::from("usbip")
        });
        Self { usbip }
    }
}

#[cfg(target_os = "linux")]
impl Default for RealRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for RealRunner {
    fn run_usbip(&mut self, args: &[&str]) -> bool {
        Command::new(&self.usbip)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

pub struct Connection {
    state: State,
    busid: String,
    consecutive_bind_failures: u32,
}

impl Connection {
    #[must_use]
    pub fn new(busid: String) -> Self {
        Self {
            state: State::Idle,
            busid,
            consecutive_bind_failures: 0,
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// How many bind attempts in a row have failed while a peer is present.
    /// Resets on the first successful bind. The daemon surfaces this in the
    /// status file once it crosses a small threshold so the kiosk can show
    /// a real diagnostic instead of "Connecting…" forever.
    #[must_use]
    pub fn consecutive_bind_failures(&self) -> u32 {
        self.consecutive_bind_failures
    }

    pub fn tick(&mut self, peer_present: bool, runner: &mut dyn CommandRunner) -> Option<Action> {
        match (self.state, peer_present) {
            (State::Idle, true) => {
                if runner.run_usbip(&["bind", "-b", &self.busid]) {
                    self.state = State::Bound;
                    self.consecutive_bind_failures = 0;
                    Some(Action::Bind)
                } else {
                    self.consecutive_bind_failures =
                        self.consecutive_bind_failures.saturating_add(1);
                    None
                }
            }
            (State::Bound, false) => {
                let _ = runner.run_usbip(&["unbind", "-b", &self.busid]);
                self.state = State::Idle;
                Some(Action::Unbind)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockRunner {
        invocations: Vec<Vec<String>>,
        bind_succeeds: bool,
    }

    impl MockRunner {
        fn ok() -> Self {
            Self { invocations: Vec::new(), bind_succeeds: true }
        }
    }

    impl CommandRunner for MockRunner {
        fn run_usbip(&mut self, args: &[&str]) -> bool {
            self.invocations.push(args.iter().map(|s| (*s).to_owned()).collect());
            self.bind_succeeds
        }
    }

    #[test]
    fn idle_with_no_peer_does_nothing() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        assert_eq!(conn.tick(false, &mut runner), None);
        assert_eq!(conn.state(), State::Idle);
        assert!(runner.invocations.is_empty());
    }

    #[test]
    fn idle_with_peer_binds() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        assert_eq!(conn.tick(true, &mut runner), Some(Action::Bind));
        assert_eq!(conn.state(), State::Bound);
        assert_eq!(runner.invocations, vec![vec!["bind", "-b", "3-3"]]);
    }

    #[test]
    fn bound_with_peer_idle_unbinds() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        conn.tick(true, &mut runner);
        assert_eq!(conn.tick(false, &mut runner), Some(Action::Unbind));
        assert_eq!(conn.state(), State::Idle);
        assert_eq!(
            runner.invocations,
            vec![vec!["bind", "-b", "3-3"], vec!["unbind", "-b", "3-3"]]
        );
    }

    #[test]
    fn failed_bind_keeps_idle() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner { bind_succeeds: false, ..Default::default() };
        assert_eq!(conn.tick(true, &mut runner), None);
        assert_eq!(conn.state(), State::Idle);
    }

    #[test]
    fn consecutive_bind_failures_count_and_reset() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner { bind_succeeds: false, ..Default::default() };
        assert_eq!(conn.consecutive_bind_failures(), 0);
        conn.tick(true, &mut runner);
        conn.tick(true, &mut runner);
        conn.tick(true, &mut runner);
        assert_eq!(conn.consecutive_bind_failures(), 3);
        // First success clears the counter.
        runner.bind_succeeds = true;
        assert_eq!(conn.tick(true, &mut runner), Some(Action::Bind));
        assert_eq!(conn.consecutive_bind_failures(), 0);
    }

    #[test]
    fn failed_unbind_still_marks_idle() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        conn.tick(true, &mut runner);
        runner.bind_succeeds = false;
        assert_eq!(conn.tick(false, &mut runner), Some(Action::Unbind));
        assert_eq!(conn.state(), State::Idle);
    }
}
