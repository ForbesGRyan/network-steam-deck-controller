//! Bind/unbind the Deck controller to `usbip-host` based on beacon state.
//!
//! Decoupled from the actual command invocation via `CommandRunner` so the
//! state transitions are unit-testable. Production wiring uses `RealRunner`,
//! which shells out to `/usr/bin/usbip`.

use std::process::Command;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Idle,
    Bound,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    Bind,
    Unbind,
}

pub trait CommandRunner {
    /// Returns true on success, false on any failure.
    fn run_usbip(&mut self, args: &[&str]) -> bool;
}

pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run_usbip(&mut self, args: &[&str]) -> bool {
        Command::new("usbip")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

pub struct Connection {
    state: State,
    busid: String,
}

impl Connection {
    #[must_use]
    pub fn new(busid: String) -> Self {
        Self { state: State::Idle, busid }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Update the desired state from the latest beacon view, returning the
    /// action taken (if any) so the caller can log it.
    pub fn tick(&mut self, peer_present: bool, runner: &mut dyn CommandRunner) -> Option<Action> {
        match (self.state, peer_present) {
            (State::Idle, true) => {
                if runner.run_usbip(&["bind", "-b", &self.busid]) {
                    self.state = State::Bound;
                    Some(Action::Bind)
                } else {
                    None
                }
            }
            (State::Bound, false) => {
                // Best effort: even if unbind fails, we still mark Idle so
                // we'll try to bind again next time the peer reappears.
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
            self.invocations
                .push(args.iter().map(|s| (*s).to_owned()).collect());
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
    fn bound_with_peer_still_present_does_nothing() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        conn.tick(true, &mut runner);
        assert_eq!(conn.tick(true, &mut runner), None);
        assert_eq!(conn.state(), State::Bound);
        // Only one bind invocation in total.
        assert_eq!(runner.invocations.len(), 1);
    }

    #[test]
    fn failed_bind_keeps_idle() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner { bind_succeeds: false, ..Default::default() };
        assert_eq!(conn.tick(true, &mut runner), None);
        assert_eq!(conn.state(), State::Idle);
    }

    #[test]
    fn failed_unbind_still_marks_idle() {
        let mut conn = Connection::new("3-3".into());
        let mut runner = MockRunner::ok();
        conn.tick(true, &mut runner);
        // Now make the unbind "fail":
        runner.bind_succeeds = false;
        assert_eq!(conn.tick(false, &mut runner), Some(Action::Unbind));
        assert_eq!(conn.state(), State::Idle);
    }
}
