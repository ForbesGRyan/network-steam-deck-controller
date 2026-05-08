//! Attach/reattach state machine on the Windows side.
//!
//! Decoupled from the actual `usbip.exe` invocation via the `UsbipDriver`
//! trait so transitions are unit-testable.

use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Idle,
    Attached,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    Attach { host: String, busid: String },
    LostAttachment,
}

pub trait UsbipDriver {
    /// Returns the busid of the first 28de:1205 device exported by `host`,
    /// or None if the lookup fails or no Deck is exported.
    fn discover_busid(&mut self, host: &str) -> Option<String>;

    /// Run `usbip attach -r host -b busid`. Returns true on success.
    fn attach(&mut self, host: &str, busid: &str) -> bool;

    /// Returns the busids currently in `usbip port`.
    fn ported_busids(&mut self) -> Vec<String>;
}

pub struct Attach {
    state: State,
    last_attempt: Option<Instant>,
    /// Remember which busid we attached so we can spot when it disappears
    /// from the port list (= remote dropped us).
    attached_busid: Option<String>,
    /// Min delay between attach attempts. Backoff doubles on failure up to
    /// max_backoff; resets on success.
    backoff: Duration,
    max_backoff: Duration,
    base_backoff: Duration,
}

impl Default for Attach {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30))
    }
}

impl Attach {
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            state: State::Idle,
            last_attempt: None,
            attached_busid: None,
            backoff: base,
            max_backoff: max,
            base_backoff: base,
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Drive the state machine one tick. `now` is injected so tests don't
    /// need a real clock.
    pub fn tick(
        &mut self,
        peer_present: bool,
        peer_host: Option<&str>,
        now: Instant,
        driver: &mut dyn UsbipDriver,
    ) -> Option<Action> {
        match self.state {
            State::Idle => {
                if !peer_present {
                    return None;
                }
                let Some(host) = peer_host else { return None };
                if let Some(last) = self.last_attempt {
                    if now.duration_since(last) < self.backoff {
                        return None;
                    }
                }
                self.last_attempt = Some(now);
                let Some(busid) = driver.discover_busid(host) else {
                    self.bump_backoff();
                    return None;
                };
                if driver.attach(host, &busid) {
                    self.state = State::Attached;
                    self.attached_busid = Some(busid.clone());
                    self.backoff = self.base_backoff;
                    Some(Action::Attach { host: host.to_owned(), busid })
                } else {
                    self.bump_backoff();
                    None
                }
            }
            State::Attached => {
                let still_attached = self
                    .attached_busid
                    .as_deref()
                    .is_some_and(|b| driver.ported_busids().iter().any(|p| p == b));
                if !peer_present || !still_attached {
                    self.state = State::Idle;
                    self.attached_busid = None;
                    Some(Action::LostAttachment)
                } else {
                    None
                }
            }
        }
    }

    fn bump_backoff(&mut self) {
        self.backoff = (self.backoff * 2).min(self.max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockDriver {
        busid_for: Option<String>,
        attach_succeeds: bool,
        ported: Vec<String>,
        attach_calls: Vec<(String, String)>,
    }

    impl UsbipDriver for MockDriver {
        fn discover_busid(&mut self, _host: &str) -> Option<String> {
            self.busid_for.clone()
        }
        fn attach(&mut self, host: &str, busid: &str) -> bool {
            self.attach_calls.push((host.to_owned(), busid.to_owned()));
            self.attach_succeeds
        }
        fn ported_busids(&mut self) -> Vec<String> {
            self.ported.clone()
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn idle_no_peer_does_nothing() {
        let mut a = Attach::default();
        let mut d = MockDriver::default();
        assert_eq!(a.tick(false, None, now(), &mut d), None);
        assert_eq!(a.state(), State::Idle);
    }

    #[test]
    fn idle_peer_present_attaches() {
        let mut a = Attach::default();
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: true,
            ported: vec!["3-3".into()],
            ..Default::default()
        };
        let action = a.tick(true, Some("192.168.1.183"), now(), &mut d);
        assert_eq!(action, Some(Action::Attach {
            host: "192.168.1.183".into(),
            busid: "3-3".into(),
        }));
        assert_eq!(a.state(), State::Attached);
        assert_eq!(d.attach_calls, vec![("192.168.1.183".into(), "3-3".into())]);
    }

    #[test]
    fn attach_failure_keeps_idle_with_backoff() {
        let mut a = Attach::new(Duration::from_secs(1), Duration::from_secs(30));
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: false,
            ..Default::default()
        };
        let t0 = now();
        assert!(a.tick(true, Some("h"), t0, &mut d).is_none());
        // Immediately retry — backoff prevents a second attach call.
        assert!(a.tick(true, Some("h"), t0 + Duration::from_millis(10), &mut d).is_none());
        assert_eq!(d.attach_calls.len(), 1);
        // After 2 s (backoff doubled to 2 s), it tries again.
        assert!(a
            .tick(true, Some("h"), t0 + Duration::from_secs(3), &mut d)
            .is_none());
        assert_eq!(d.attach_calls.len(), 2);
    }

    #[test]
    fn discover_failure_does_not_call_attach() {
        let mut a = Attach::default();
        let mut d = MockDriver { busid_for: None, ..Default::default() };
        assert_eq!(a.tick(true, Some("h"), now(), &mut d), None);
        assert!(d.attach_calls.is_empty());
        assert_eq!(a.state(), State::Idle);
    }

    #[test]
    fn attached_loses_peer_drops_to_idle() {
        let mut a = Attach::default();
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: true,
            ported: vec!["3-3".into()],
            ..Default::default()
        };
        a.tick(true, Some("h"), now(), &mut d);
        assert_eq!(a.state(), State::Attached);
        let action = a.tick(false, None, now() + Duration::from_secs(1), &mut d);
        assert_eq!(action, Some(Action::LostAttachment));
        assert_eq!(a.state(), State::Idle);
    }

    #[test]
    fn attached_busid_disappears_drops_to_idle() {
        let mut a = Attach::default();
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: true,
            ported: vec!["3-3".into()],
            ..Default::default()
        };
        a.tick(true, Some("h"), now(), &mut d);
        assert_eq!(a.state(), State::Attached);
        // Simulate the kernel detaching after a network blip.
        d.ported.clear();
        let action = a.tick(true, Some("h"), now() + Duration::from_secs(1), &mut d);
        assert_eq!(action, Some(Action::LostAttachment));
        assert_eq!(a.state(), State::Idle);
    }

    #[test]
    fn successful_attach_resets_backoff() {
        let mut a = Attach::new(Duration::from_secs(1), Duration::from_secs(30));
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: false,
            ..Default::default()
        };
        let t0 = now();
        a.tick(true, Some("h"), t0, &mut d);
        a.tick(true, Some("h"), t0 + Duration::from_secs(3), &mut d);
        assert_eq!(d.attach_calls.len(), 2);
        // Now succeed.
        d.attach_succeeds = true;
        d.ported = vec!["3-3".into()];
        a.tick(true, Some("h"), t0 + Duration::from_secs(10), &mut d);
        assert_eq!(a.state(), State::Attached);
        // Drop and retry: backoff should be back to base (1 s).
        d.ported.clear();
        a.tick(true, Some("h"), t0 + Duration::from_secs(11), &mut d);
        assert_eq!(a.state(), State::Idle);
        // Re-discover succeeds, attach retries within 1 s of last_attempt.
        d.ported = vec!["3-3".into()];
        a.tick(true, Some("h"), t0 + Duration::from_secs(12), &mut d);
        assert_eq!(d.attach_calls.len(), 4);
    }
}
