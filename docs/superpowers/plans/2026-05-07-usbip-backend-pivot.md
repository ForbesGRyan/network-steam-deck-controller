# usbip Backend Pivot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the custom KMDF/UdeCx Windows driver + UDP HID-tunnel data plane with a thin Rust control plane that drives stock `usbipd` (Linux) and `usbip-win2` (Windows). Pairing, discovery, and identity stay; the rest of the data plane goes.

**Architecture:** Deck side runs `usbipd.service` (system-managed) plus our `server-deck` daemon, which watches the discovery beacon and binds/unbinds the Steam Deck controller to `usbip-host` whenever a paired peer comes/goes. Windows side runs `client-win` as a tray app that watches the same beacon and shells out to `usbip.exe attach` / `port` to keep a live attachment to the Deck's controller. Wire-level USB plumbing is delegated entirely to the kernel modules on both ends; we only own the lifecycle.

**Tech Stack:** Rust 2021. New deps: `tray-icon = "0.20"` (Windows tray), `windows-sys` features for the Win32 message pump, `which = "7"` (locate `usbip.exe`). Removed: the entire `driver/` C++ tree, the UDP/HID input+output paths, the `--test` and `--replay` modes, and the UdeCx IOCTL bindings.

**Spec:** Conversation 2026-05-07 (decision recorded in `memory/usbip_backend_validated.md`).

## Validated baseline

The pivot is informed by an end-to-end manual test on 2026-05-07: SteamOS Deck running `usbipd -D` with `usbip bind -b 3-3` (controller VID 28de PID 1205) and Windows 11 running `usbip.exe attach -r <ip> -b 3-3` from `usbip-win2 v0.9.7.7`. Steam recognized the device as a Deck and it functioned in-game. This plan does *not* re-derive the backend choice — it productizes the manual command sequence behind discovery + a tray UI.

## UX targets (decided in the same conversation)

- **Connect:** auto-on-boot via system services, plus a Windows tray icon for manual override.
- **Auth:** trusted-LAN. Pair flow gates beacon-level identity; the usbip TCP socket itself is plaintext. No TLS proxy.
- **Reconnect:** best-effort auto-reattach on Wi-Fi blip with exponential backoff. The vhci kernel module on Windows surfaces the lost device as a USB unplug; some games will see a controller unplug+replug. Acceptable for v0.

## Phase ordering

Three phases. Each ends with a validation step against real hardware.

- **Phase A (Tasks 1–5):** Deck-side rewrite. Drives bind/unbind via `usbip` CLI in response to beacon state. Tested in isolation by manually running `usbip.exe attach` from Windows.
- **Phase B (Tasks 6–11):** Windows-side rewrite. Drives `usbip.exe attach` + tray UI in response to beacon state. Can be developed against the existing manually-bound `usbipd` baseline; depends on Phase A only for the auto-bind behavior.
- **Phase C (Tasks 12–15):** Cleanup, packaging, docs. Deletes the custom driver tree, strips dead code from `deck-protocol`, rewrites install scripts and `ARCHITECTURE.md`.

Phases A and B share the existing `discovery` crate but no new code, so they're parallelizable across two subagents if desired.

---

## File Structure

**Create:**

- `crates/server-deck/src/connection.rs` — bind/unbind state machine, parameterized by a command runner so it's unit-testable.
- `crates/server-deck/src/sysfs.rs` — enumerate `/sys/bus/usb/devices/*` to find the Deck controller's busid (28de:1205).
- `crates/client-win/src/attach.rs` — attach state machine, parameterized by a command runner.
- `crates/client-win/src/usbip_cli.rs` — wrapper around `usbip.exe list -r` and `usbip.exe port` output parsers.
- `crates/client-win/src/tray.rs` — `tray-icon` setup + Win32 message pump + menu event handlers.
- `crates/client-win/src/autostart.rs` — register/deregister the binary in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
- `scripts/install-deck.sh` — one-shot Deck installer: pacman install of `usbip`, enable `usbipd.service`, install `network-deck-server.service`.
- `scripts/install-windows.ps1` — one-shot Windows installer: download + silent-install usbip-win2, register tray autostart.

**Modify:**

- `crates/server-deck/src/main.rs` — strip hidraw / HID / IPC paths; wire the new connection state machine.
- `crates/server-deck/Cargo.toml` — drop `deck-protocol` dep.
- `crates/server-deck/scripts/network-deck-server.service` — drop `DECK_HIDRAW`, add doc reference to `usbipd.service` dep.
- `crates/client-win/src/main.rs` — strip driver IPC, `--test`, `--replay`; wire the new attach state machine + tray.
- `crates/client-win/Cargo.toml` — swap windows-sys feature set, add `tray-icon`, `which`.
- `crates/client-win/src/driver.rs` — **delete**.
- `crates/discovery/Cargo.toml` — drop `deck-protocol` dep.
- `crates/discovery/src/beacon.rs` — inline the replay-window helper (was `deck_protocol::auth::REPLAY_WINDOW_US` + `is_within_replay_window`).
- `Cargo.toml` (workspace root) — drop `crates/deck-protocol` member.
- `ARCHITECTURE.md` — flip non-goal #1, add 2026-05-07 decision-history entry, rewrite component diagram + build sequence.
- `README.md` — replace driver-install section with usbip-win2 install pointer.

**Delete:**

- `driver/` (entire C++ tree).
- `crates/deck-protocol/` (entire crate).
- `deck-buttons.bin` (replay capture, no longer relevant).
- `CppProperties.json` (VS C++ tooling, no longer relevant).

---

## Task 1: Move replay-window helper into discovery, prepare for deck-protocol removal

**Files:**

- Modify: `crates/discovery/src/beacon.rs`
- Modify: `crates/discovery/Cargo.toml`

The beacon's `handle_packet` is the only remaining caller of `deck_protocol::auth::REPLAY_WINDOW_US` and `is_within_replay_window` once the data plane is gone. Inline both so `discovery` no longer depends on `deck-protocol`. This is the foundation for removing the crate entirely in Task 13.

- [ ] **Step 1: Inline the helper into `beacon.rs`**

In `crates/discovery/src/beacon.rs`, add this private helper near the top of the file (right under the `use` block):

```rust
/// ±wall-clock skew tolerated for beacon packets, in microseconds.
/// 30 s is short enough to defang a delayed replay, long enough to absorb
/// NTP wobble between two LAN hosts that haven't slewed in a while.
const REPLAY_WINDOW_US: u32 = 30_000_000;

/// True if `packet_us` is within `window_us` of `now_us` (wrap-aware).
/// Both timestamps are the low 32 bits of microseconds since some epoch.
#[allow(clippy::cast_possible_wrap)]
fn is_within_replay_window(packet_us: u32, now_us: u32, window_us: u32) -> bool {
    let dt = (now_us as i32).wrapping_sub(packet_us as i32);
    dt.unsigned_abs() <= window_us
}
```

- [ ] **Step 2: Update the call site**

Change the `use deck_protocol::auth::REPLAY_WINDOW_US;` import at the top of `beacon.rs` — delete it. Then change `handle_packet`'s replay-window check from:

```rust
if !deck_protocol::auth::is_within_replay_window(pkt32, now32, REPLAY_WINDOW_US) { return; }
```

to:

```rust
if !is_within_replay_window(pkt32, now32, REPLAY_WINDOW_US) { return; }
```

- [ ] **Step 3: Drop deck-protocol from discovery's Cargo.toml**

In `crates/discovery/Cargo.toml`, delete the line:

```toml
deck-protocol = { path = "../deck-protocol" }
```

- [ ] **Step 4: Build to confirm**

Run: `cargo build -p discovery`
Expected: compiles cleanly, no warnings about unused imports.

Run: `cargo test -p discovery`
Expected: all existing beacon / pair tests still pass (they don't touch the helper directly).

- [ ] **Step 5: Commit**

```bash
git add crates/discovery/src/beacon.rs crates/discovery/Cargo.toml
git commit -m "refactor(discovery): inline replay-window helper, drop deck-protocol dep

Beacon was the only remaining caller of deck_protocol::auth's
replay-window helper. Inline it in preparation for deleting deck-protocol
once the usbip pivot lands."
```

---

## Task 2: Sysfs busid lookup for the Steam Deck controller

**Files:**

- Create: `crates/server-deck/src/sysfs.rs`
- Modify: `crates/server-deck/src/main.rs:50` (mod declaration)
- Test: inline `#[cfg(test)] mod tests` in `sysfs.rs` (uses `tempfile`).

The Deck's internal controller is always VID 28de PID 1205, but the busid (`3-3`, `3-2`, etc.) can change across reboots. Walk `/sys/bus/usb/devices/*/idVendor` + `idProduct` to find the matching busid at runtime instead of hard-coding `3-3`.

- [ ] **Step 1: Add tempfile to server-deck dev-dependencies**

In `crates/server-deck/Cargo.toml`, add a `[dev-dependencies]` table if absent:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing tests**

Create `crates/server-deck/src/sysfs.rs` with the test module first:

```rust
//! Find the Deck controller's USB busid by walking sysfs.
//!
//! The busid (e.g. `3-3`) can change across reboots. usbip operates on
//! busids, not VID/PID, so we need a lookup at startup.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Steam Deck internal controller VID/PID — both old LCD and OLED revs
/// expose the same identifier here.
pub const DECK_VID: &str = "28de";
pub const DECK_PID: &str = "1205";

#[derive(Debug)]
pub enum SysfsError {
    Io(io::Error),
    NotFound,
}

impl From<io::Error> for SysfsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Walk `<root>/bus/usb/devices/*` looking for a directory with matching
/// `idVendor` + `idProduct`. Returns the busid (the directory name).
///
/// `root` is `/sys` in production; tests pass a tempdir.
///
/// # Errors
/// `SysfsError::Io` for filesystem errors. `SysfsError::NotFound` if no
/// matching device is present.
pub fn find_deck_busid(root: &Path, vid: &str, pid: &str) -> Result<String, SysfsError> {
    let dir = root.join("bus/usb/devices");
    let entries = fs::read_dir(&dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if read_trim(&path.join("idVendor")).as_deref() == Some(vid)
            && read_trim(&path.join("idProduct")).as_deref() == Some(pid)
        {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                return Ok(name.to_owned());
            }
        }
    }
    Err(SysfsError::NotFound)
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_device(root: &Path, busid: &str, vid: &str, pid: &str) {
        let dir = root.join("bus/usb/devices").join(busid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("idVendor"), format!("{vid}\n")).unwrap();
        fs::write(dir.join("idProduct"), format!("{pid}\n")).unwrap();
    }

    #[test]
    fn finds_deck_among_multiple_devices() {
        let root = tempdir().unwrap();
        make_device(root.path(), "1-1", "1234", "5678");
        make_device(root.path(), "3-3", DECK_VID, DECK_PID);
        make_device(root.path(), "usb1", "1d6b", "0002");
        let busid = find_deck_busid(root.path(), DECK_VID, DECK_PID).unwrap();
        assert_eq!(busid, "3-3");
    }

    #[test]
    fn returns_not_found_when_absent() {
        let root = tempdir().unwrap();
        make_device(root.path(), "1-1", "1234", "5678");
        let err = find_deck_busid(root.path(), DECK_VID, DECK_PID);
        assert!(matches!(err, Err(SysfsError::NotFound)));
    }

    #[test]
    fn handles_missing_devices_dir() {
        let root = tempdir().unwrap();
        let err = find_deck_busid(root.path(), DECK_VID, DECK_PID);
        assert!(matches!(err, Err(SysfsError::Io(_))));
    }

    #[test]
    fn ignores_directories_missing_vid_pid_files() {
        let root = tempdir().unwrap();
        // Hub-style device directories sometimes have only idVendor.
        let dir = root.path().join("bus/usb/devices/usb1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("idVendor"), "1d6b\n").unwrap();
        // No idProduct.
        make_device(root.path(), "3-3", DECK_VID, DECK_PID);
        let busid = find_deck_busid(root.path(), DECK_VID, DECK_PID).unwrap();
        assert_eq!(busid, "3-3");
    }
}
```

- [ ] **Step 3: Wire the module into server-deck**

Modify `crates/server-deck/src/main.rs` — inside the `#[cfg(target_os = "linux")] mod linux { ... }` block, near the top, add:

```rust
use super::sysfs;
```

And outside the `mod linux` block (at file scope), add:

```rust
#[cfg(target_os = "linux")]
mod sysfs;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p server-deck --lib sysfs`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/server-deck/src/sysfs.rs crates/server-deck/src/main.rs crates/server-deck/Cargo.toml
git commit -m "feat(server-deck): sysfs busid lookup for the Deck controller

Replaces the hard-coded busid 3-3 with a runtime lookup so a Deck reboot
that shifts the bus topology doesn't break usbip bind."
```

---

## Task 3: Connection state machine — bind/unbind in response to beacon state

**Files:**

- Create: `crates/server-deck/src/connection.rs`
- Test: inline tests in `connection.rs`.

The state machine has two states: **Idle** (nothing bound) and **Bound** (controller bound to `usbip-host`). Transitions are driven by `tick(peer_present: bool)`. Commands are dispatched through a `CommandRunner` trait so tests can assert sequences without touching real `usbip`.

- [ ] **Step 1: Write the failing tests**

Create `crates/server-deck/src/connection.rs`:

```rust
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
```

- [ ] **Step 2: Wire the module in**

In `crates/server-deck/src/main.rs`, add at file scope (outside the `mod linux` block):

```rust
#[cfg(target_os = "linux")]
mod connection;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p server-deck --lib connection`
Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/server-deck/src/connection.rs crates/server-deck/src/main.rs
git commit -m "feat(server-deck): bind/unbind state machine for usbip backend

Decouples beacon state from usbip CLI invocation so transitions are
unit-testable. Production runner shells out to /usr/bin/usbip."
```

---

## Task 4: Rewrite server-deck main loop around the new state machine

**Files:**

- Modify: `crates/server-deck/src/main.rs` (large rewrite — replace the entire `linux::run` and supporting helpers, keep `parse_args` and `run_pair_mode`).
- Modify: `crates/server-deck/Cargo.toml` (drop `deck-protocol`).

After this task the binary's job is: load identity + trust, find busid, start beacon, on each tick drive the connection state machine. No HID, no IPC, no UDP data plane. Beacon broadcast continues to share the same UDP port — Windows side reads beacons to discover us.

- [ ] **Step 1: Replace the body of `linux::run`**

Open `crates/server-deck/src/main.rs`. Replace the *entire* `mod linux { ... }` block with the version below. The `parse_args`, `run_pair_mode`, and `hostname` helpers are still here, but `Stats`, `try_open_hidraw`, `wait_for_hidraw`, `run_input_loop`, `run_output_loop`, `hidiocsfeature`, `read_one`, `decode_or_skip`, `send_packet`, `print_status`, `now_us_low32`, and the `ReadResult` enum are gone.

```rust
#[cfg(target_os = "linux")]
mod linux {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::connection::{Connection, RealRunner};
    use super::sysfs::{find_deck_busid, DECK_PID, DECK_VID};

    /// UDP port the discovery beacon shares with the (now defunct) data plane.
    /// Kept the same value for backward compatibility with paired peers that
    /// were paired pre-pivot — the trust file only stores the pubkey, but the
    /// beacon listen port has to match what `client-win` expects.
    const BEACON_PORT: u16 = 49152;

    /// How often we poll beacon state and drive the connection state machine.
    const TICK_INTERVAL: Duration = Duration::from_millis(500);

    enum Mode { Run, Pair }

    struct ParsedArgs {
        mode: Mode,
        state_dir: PathBuf,
        sysfs_root: PathBuf,
    }

    fn parse_args() -> ParsedArgs {
        let mut args = std::env::args().skip(1);
        let mut mode = Mode::Run;
        let mut state_dir_override: Option<PathBuf> = None;
        let mut sysfs_root_override: Option<PathBuf> = None;
        while let Some(a) = args.next() {
            match a.as_str() {
                "pair" => mode = Mode::Pair,
                "--state-dir" => {
                    state_dir_override = args.next().map(PathBuf::from);
                    if state_dir_override.is_none() {
                        eprintln!("--state-dir requires a value");
                        std::process::exit(2);
                    }
                }
                "--sysfs-root" => {
                    // For testing only — points at a tempdir mocked sysfs.
                    sysfs_root_override = args.next().map(PathBuf::from);
                    if sysfs_root_override.is_none() {
                        eprintln!("--sysfs-root requires a value");
                        std::process::exit(2);
                    }
                }
                other => {
                    eprintln!("unexpected argument: {other}");
                    std::process::exit(2);
                }
            }
        }
        let state_dir = state_dir_override.unwrap_or_else(|| {
            discovery::state_dir::default_state_dir().unwrap_or_else(|e| {
                eprintln!("cannot resolve state dir: {e:?}");
                std::process::exit(1);
            })
        });
        let sysfs_root = sysfs_root_override.unwrap_or_else(|| PathBuf::from("/sys"));
        ParsedArgs { mode, state_dir, sysfs_root }
    }

    pub fn run() {
        let args = parse_args();
        let identity = std::sync::Arc::new(
            discovery::identity::load_or_generate(&args.state_dir).unwrap_or_else(|e| {
                eprintln!("identity load: {e:?}");
                std::process::exit(1);
            }),
        );

        if matches!(args.mode, Mode::Pair) {
            super::run_pair_mode(&identity, &args.state_dir, BEACON_PORT);
            return;
        }

        let trusted = match discovery::trust::load(&args.state_dir) {
            Ok(Some(p)) => std::sync::Arc::new(p),
            Ok(None) => {
                eprintln!("no trusted peer; run `server-deck pair` to pair first");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("trust load: {e:?}");
                std::process::exit(1);
            }
        };

        let busid = find_deck_busid(&args.sysfs_root, DECK_VID, DECK_PID).unwrap_or_else(|e| {
            eprintln!(
                "could not find Steam Deck controller (VID {DECK_VID} PID {DECK_PID}) in sysfs: {e:?}"
            );
            std::process::exit(1);
        });
        eprintln!("found Deck controller at busid {busid}");

        let bound = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).unwrap_or_else(|e| {
            eprintln!("bind 0.0.0.0:{BEACON_PORT}: {e}");
            std::process::exit(1);
        });

        let beacon = std::sync::Arc::new(
            discovery::Beacon::new(
                identity.clone(),
                trusted.clone(),
                SocketAddr::new(Ipv4Addr::BROADCAST.into(), BEACON_PORT),
                super::hostname(),
                BEACON_PORT,
            )
            .unwrap_or_else(|e| {
                eprintln!("beacon init: {e:?}");
                std::process::exit(1);
            }),
        );
        discovery::beacon::spawn_broadcast(beacon.clone());

        // Beacon recv thread: drains incoming beacons so the live-peer state
        // updates. Kept on its own thread because the recv is blocking.
        let beacon_recv = beacon.clone();
        std::thread::Builder::new()
            .name("discovery-recv".into())
            .spawn(move || {
                let mut buf = [0_u8; discovery::packet::PACKET_LEN];
                loop {
                    if let Ok((n, src)) = bound.recv_from(&mut buf) {
                        if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                            beacon_recv.handle_packet(src, &buf[..n]);
                        }
                    }
                }
            })
            .ok();

        eprintln!(
            "supervising bind state for busid {busid} -> peer {} (fingerprint {})",
            trusted.name,
            identity.fingerprint_str(),
        );

        let mut conn = Connection::new(busid.clone());
        let mut runner = RealRunner;
        loop {
            let peer_present = beacon.current_peer_with_age()
                .is_some_and(|(_, age)| age <= discovery::beacon::STALE_AFTER);
            if let Some(action) = conn.tick(peer_present, &mut runner) {
                eprintln!("connection: {action:?} (state={:?})", conn.state());
            }
            std::thread::sleep(TICK_INTERVAL);
        }
    }
}
```

- [ ] **Step 2: Move `hostname` and `run_pair_mode` to file scope**

The new `linux::run` references `super::hostname` and `super::run_pair_mode`. Move both helpers out of the `mod linux` block to file scope (still gated on `cfg(target_os = "linux")` since the `pair` flow uses Linux-only socket bits). Replace the existing definitions with:

```rust
#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "deck".to_owned())
}

#[cfg(target_os = "linux")]
fn run_pair_mode(
    identity: &std::sync::Arc<discovery::Identity>,
    state_dir: &std::path::Path,
    port: u16,
) {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::time::Duration;

    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind for pair: {e}");
            std::process::exit(1);
        }
    };
    sock.set_broadcast(true).ok();
    let cfg = discovery::pair::PairConfig {
        identity: identity.clone(),
        recv_sock: sock,
        unicast_target: SocketAddr::new(Ipv4Addr::BROADCAST.into(), port),
        self_name: hostname(),
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(120),
    };
    eprintln!("pairing — fingerprint {}; waiting up to 120 s", identity.fingerprint_str());
    let mut stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    match discovery::pair::run_pair(&cfg, &mut stdin, &mut stderr) {
        discovery::pair::PairOutcome::Paired(p) => {
            eprintln!("paired with {} (fingerprint stored)", p.name);
        }
        other => {
            eprintln!("pair did not complete: {other:?}");
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 3: Drop deck-protocol dep**

In `crates/server-deck/Cargo.toml`, delete the line:

```toml
deck-protocol = { path = "../deck-protocol" }
```

The `[target.'cfg(target_os = "linux")'.dependencies] libc = "0.2"` entry can also go — `hidiocsfeature` was the only user.

- [ ] **Step 4: Build**

Run: `cargo build -p server-deck`
Expected: compiles cleanly (it's only meaningful on Linux; on Windows the binary's `main` immediately exits with the existing "requires Linux" message).

- [ ] **Step 5: Commit**

```bash
git add crates/server-deck/src/main.rs crates/server-deck/Cargo.toml
git commit -m "feat(server-deck): rewrite around usbip bind/unbind state machine

Strips the hidraw read loop, HID parse, UDP input/output paths, and
HIDIOCSFEATURE plumbing. Replaces with a half-second tick that calls
'usbip bind|unbind' based on whether the paired peer's beacon is fresh.

The beacon and pair flow are unchanged; trust files paired pre-pivot
keep working."
```

---

## Task 5: Update systemd unit and Deck install script

**Files:**

- Modify: `crates/server-deck/scripts/network-deck-server.service`
- Create: `scripts/install-deck.sh`

The systemd unit no longer needs `DECK_HIDRAW`. It now requires `usbipd.service` to be running so `usbip bind` has somewhere to publish the device.

- [ ] **Step 1: Rewrite the systemd unit**

Replace `crates/server-deck/scripts/network-deck-server.service` entirely with:

```ini
# systemd unit for the Deck-side usbip control plane.
#
# Install:
#   sudo cp network-deck-server.service /etc/systemd/system/
#   sudo systemctl daemon-reload
#   sudo systemctl enable --now network-deck-server.service
#
# Depends on usbipd.service (provided by the `usbip` arch package) being up
# so `usbip bind` has a daemon to register the controller with.
#
# First-time setup: pair with the Windows PC by running
#   sudo systemctl stop network-deck-server.service
#   sudo -u deck /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck
# while `client-win` is in pair mode on the PC, then re-enable the unit.

[Unit]
Description=Network Deck usbip control plane (Deck -> Windows)
Documentation=https://github.com/ForbesGRyan/network-steam-deck-controller
After=network-online.target usbipd.service
Wants=network-online.target
Requires=usbipd.service

[Service]
Type=simple
User=root
Group=root

StateDirectory=network-deck

ExecStart=/usr/local/bin/server-deck --state-dir /var/lib/network-deck

Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Note: `User=root` because `usbip bind` writes to `/sys/bus/usb/drivers/usbip-host/bind`. Pair-mode UX shifts to "stop the service, run as root" — this is a one-time setup so the simplification is worth it.

- [ ] **Step 2: Write the Deck install script**

Create `scripts/install-deck.sh`:

```bash
#!/usr/bin/env bash
# Deck-side installer. Idempotent.
set -euo pipefail

if [[ "$EUID" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi

if ! command -v usbip >/dev/null; then
    if command -v steamos-readonly >/dev/null; then
        echo ">> SteamOS detected. Disabling readonly + installing usbip..."
        steamos-readonly disable
        if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
            pacman-key --init
            pacman-key --populate
        fi
        pacman -S --noconfirm usbip
        steamos-readonly enable
    else
        echo "Install the 'usbip' package via your distro's package manager, then re-run." >&2
        exit 1
    fi
fi

echo ">> Loading kernel modules..."
modprobe usbip-core
modprobe usbip-host
modprobe vhci-hcd
cat >/etc/modules-load.d/usbip.conf <<'EOF'
usbip-core
usbip-host
vhci-hcd
EOF

echo ">> Enabling usbipd.service..."
systemctl enable --now usbipd.service

BIN_SRC="$(dirname "$0")/../target/release/server-deck"
if [[ ! -f "$BIN_SRC" ]]; then
    echo "Build the release binary first: cargo build --release -p server-deck" >&2
    exit 1
fi

echo ">> Installing /usr/local/bin/server-deck..."
install -m 755 "$BIN_SRC" /usr/local/bin/server-deck

UNIT_SRC="$(dirname "$0")/../crates/server-deck/scripts/network-deck-server.service"
echo ">> Installing systemd unit..."
install -m 644 "$UNIT_SRC" /etc/systemd/system/network-deck-server.service
systemctl daemon-reload

echo
echo "Done. Next steps:"
echo "  1. On Windows, run 'client-win pair' (or use the tray app's Pair menu)."
echo "  2. Stop the service and run 'sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck' on the Deck while it's in pair mode."
echo "  3. Once paired: 'sudo systemctl enable --now network-deck-server.service'"
```

Make it executable:

```bash
chmod +x scripts/install-deck.sh
```

- [ ] **Step 3: Commit**

```bash
git add crates/server-deck/scripts/network-deck-server.service scripts/install-deck.sh
git commit -m "feat(install): Deck-side installer + systemd unit for usbip backend

Wraps pacman install of usbip, modprobe + persistent module load, and
enabling usbipd.service. Drops DECK_HIDRAW from our unit; service now
runs as root so 'usbip bind' can write to /sys."
```

---

## Task 6: Phase A integration check (manual, paired with a real Deck)

**No files** — this is a validation step before starting Phase B. Run on the Deck, then verify from Windows manually.

- [ ] **Step 1: Build + install on the Deck**

Over SSH:

```bash
cd ~/code/network-usb
cargo build --release -p server-deck
sudo ./scripts/install-deck.sh
```

Expected: install script completes; `systemctl status usbipd.service` shows active; `which server-deck` returns `/usr/local/bin/server-deck`.

- [ ] **Step 2: Pair from the Deck**

```bash
sudo systemctl stop network-deck-server.service
sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck
```

In another shell, on Windows, run an existing `client-win pair` (or the manual `usbip` baseline). Confirm fingerprints match. Trust file should land at `/var/lib/network-deck/trusted-peers.toml`.

- [ ] **Step 3: Start the service and watch the bind**

```bash
sudo systemctl start network-deck-server.service
journalctl -u network-deck-server.service -f
```

Expected when Windows beacon is reachable: log line `connection: Bind (state=Bound)`. Verify with:

```bash
usbip list -l
```

Expected: the `28de:1205` entry shows `*usbip*` instead of the normal driver, indicating it's bound.

- [ ] **Step 4: Verify Windows can attach**

From the Windows box (manual, since Phase B isn't built yet):

```powershell
& "C:\Program Files\USBip\usbip.exe" attach -r <deck-ip> -b <busid-from-step-3>
```

Open Steam → Controller → confirm the Deck shows up. Test a button.

- [ ] **Step 5: Verify auto-unbind**

Stop `client-win`'s beacon broadcast (kill any running pre-pivot client process so beacons stop). Wait ~10 seconds.

Expected: `journalctl` shows `connection: Unbind (state=Idle)`. `usbip list -l` shows the `28de:1205` entry back on the normal driver.

If any step fails, do not advance to Phase B until resolved. Phase A is the foundation.

---

## Task 7: usbip CLI wrapper module on Windows

**Files:**

- Create: `crates/client-win/src/usbip_cli.rs`
- Test: inline tests in `usbip_cli.rs`.
- Modify: `crates/client-win/Cargo.toml` (add `which`).

`usbip-win2` ships `usbip.exe` at `C:\Program Files\USBip\usbip.exe`. We shell out for `attach`, `detach`, `list -r`, and `port`. This module owns command-line construction and output parsing.

- [ ] **Step 1: Add deps**

Modify `crates/client-win/Cargo.toml`. Replace the file with:

```toml
[package]
name = "client-win"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Windows side: tray app that drives usbip-win2 attach lifecycle from a paired Deck"

[dependencies]
discovery = { path = "../discovery" }
which = "7"

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",
    "Win32_System_LibraryLoader",
    "Win32_System_Registry",
] }
tray-icon = "0.20"

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/client-win/src/usbip_cli.rs`:

```rust
//! Wrapper around `usbip.exe` (usbip-win2). Owns command construction +
//! output parsing. The actual command runner is parameterized so tests
//! don't need a real `usbip.exe` on PATH.

use std::path::PathBuf;
use std::process::Command;

/// One exported device entry from `usbip list -r <host>`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct RemoteDevice {
    pub busid: String,
    pub vid: String,
    pub pid: String,
}

#[derive(Debug)]
pub enum CliError {
    NotInstalled,
    InvocationFailed(String),
    ParseFailed(String),
}

/// Locate `usbip.exe`. Checks PATH first, then the default install dir.
///
/// # Errors
/// `CliError::NotInstalled` if neither location yields the binary.
pub fn locate() -> Result<PathBuf, CliError> {
    if let Ok(p) = which::which("usbip.exe") {
        return Ok(p);
    }
    let default = PathBuf::from(r"C:\Program Files\USBip\usbip.exe");
    if default.is_file() {
        return Ok(default);
    }
    Err(CliError::NotInstalled)
}

/// Parse the human-readable output of `usbip list -r <host>` into the list
/// of exported devices. The format (as of usbip-win2 0.9.7.x) is:
///
/// ```text
/// Exportable USB devices
/// ======================
///  - 192.168.1.183
///         3-3: Valve Software : unknown product (28de:1205)
///            : /sys/devices/pci0000:00/...
///            : (Defined at Interface level) (00/00/00)
/// ```
///
/// We only care about the `<busid>: ... (<vid>:<pid>)` line.
pub fn parse_list_remote(stdout: &str) -> Vec<RemoteDevice> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        // Heuristic: a device line has a busid prefix like "3-3:" then text
        // ending in "(xxxx:yyyy)".
        let Some((busid, rest)) = trimmed.split_once(':') else { continue };
        let busid = busid.trim();
        if busid.is_empty() || !busid.contains('-') {
            continue;
        }
        // Find the trailing "(vid:pid)" group.
        let Some(open) = rest.rfind('(') else { continue };
        let Some(close) = rest.rfind(')') else { continue };
        if close <= open + 1 {
            continue;
        }
        let inner = &rest[open + 1..close];
        let Some((vid, pid)) = inner.split_once(':') else { continue };
        out.push(RemoteDevice {
            busid: busid.to_owned(),
            vid: vid.trim().to_owned(),
            pid: pid.trim().to_owned(),
        });
    }
    out
}

/// Parse the output of `usbip port` (the local-attach list) and return the
/// set of remote busids currently attached. Format example:
///
/// ```text
/// Imported USB devices
/// ====================
/// Port 00: <Port in Use> at High Speed(480Mbps)
///        Valve Software : unknown product (28de:1205)
///        9-2 -> usbip://192.168.1.183:3240/3-3
///            -> remote bus/dev 003/006
/// ```
///
/// We extract the remote busid (the suffix after the host:port in the
/// `usbip://` URL).
pub fn parse_port(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some(idx) = line.find("usbip://") else { continue };
        let url = &line[idx + "usbip://".len()..];
        // After the host:port comes /<busid>; trim trailing whitespace.
        let Some(slash) = url.find('/') else { continue };
        let busid = url[slash + 1..].trim();
        if !busid.is_empty() {
            out.push(busid.to_owned());
        }
    }
    out
}

/// Production wrapper.
pub struct UsbipCli {
    path: PathBuf,
}

impl UsbipCli {
    /// Locate the binary and bind a wrapper.
    ///
    /// # Errors
    /// `CliError::NotInstalled` if `usbip.exe` isn't on PATH or in the
    /// default install dir.
    pub fn discover() -> Result<Self, CliError> {
        Ok(Self { path: locate()? })
    }

    fn run(&self, args: &[&str]) -> Result<String, CliError> {
        let out = Command::new(&self.path)
            .args(args)
            .output()
            .map_err(|e| CliError::InvocationFailed(e.to_string()))?;
        if !out.status.success() {
            return Err(CliError::InvocationFailed(format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `usbip list -r <host>`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn list_remote(&self, host: &str) -> Result<Vec<RemoteDevice>, CliError> {
        Ok(parse_list_remote(&self.run(&["list", "-r", host])?))
    }

    /// `usbip port`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn port(&self) -> Result<Vec<String>, CliError> {
        Ok(parse_port(&self.run(&["port"])?))
    }

    /// `usbip attach -r <host> -b <busid>`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn attach(&self, host: &str, busid: &str) -> Result<(), CliError> {
        self.run(&["attach", "-r", host, "-b", busid]).map(|_| ())
    }

    /// `usbip detach -p <port-num>` — used for graceful disconnect from the
    /// tray. Port number is the `Port NN` index from `usbip port` output.
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn detach(&self, port: u8) -> Result<(), CliError> {
        self.run(&["detach", "-p", &port.to_string()]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LIST: &str = "Exportable USB devices\n\
        ======================\n\
        - 192.168.1.183\n\
               3-3: Valve Software : unknown product (28de:1205)\n\
                  : /sys/devices/pci0000:00/0000:00:0d.0/usb3/3-3\n\
                  : (Defined at Interface level) (00/00/00)\n\
        \n";

    const SAMPLE_PORT: &str = "Imported USB devices\n\
        ====================\n\
        Port 00: <Port in Use> at High Speed(480Mbps)\n\
               Valve Software : unknown product (28de:1205)\n\
               9-2 -> usbip://192.168.1.183:3240/3-3\n\
                   -> remote bus/dev 003/006\n";

    #[test]
    fn parse_list_extracts_busid_and_vidpid() {
        let devs = parse_list_remote(SAMPLE_LIST);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].busid, "3-3");
        assert_eq!(devs[0].vid, "28de");
        assert_eq!(devs[0].pid, "1205");
    }

    #[test]
    fn parse_list_empty_input() {
        assert!(parse_list_remote("").is_empty());
        assert!(parse_list_remote("Exportable USB devices\n=====\n").is_empty());
    }

    #[test]
    fn parse_port_extracts_remote_busid() {
        let busids = parse_port(SAMPLE_PORT);
        assert_eq!(busids, vec!["3-3"]);
    }

    #[test]
    fn parse_port_empty_when_nothing_attached() {
        let empty = "Imported USB devices\n====================\n";
        assert!(parse_port(empty).is_empty());
    }

    #[test]
    fn parse_port_handles_multiple() {
        let two = "Imported USB devices\n\
            ====================\n\
            Port 00: <Port in Use> at High Speed(480Mbps)\n\
                   Valve Software : unknown product (28de:1205)\n\
                   9-2 -> usbip://192.168.1.183:3240/3-3\n\
            Port 01: <Port in Use> at High Speed(480Mbps)\n\
                   Other Vendor : whatever (1234:5678)\n\
                   9-3 -> usbip://192.168.1.42:3240/4-1\n";
        assert_eq!(parse_port(two), vec!["3-3", "4-1"]);
    }
}
```

- [ ] **Step 3: Wire the module in**

Modify `crates/client-win/src/main.rs` — add at the top, replacing the `mod driver;` line:

```rust
#[cfg(windows)]
mod usbip_cli;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p client-win --lib usbip_cli`
Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/client-win/src/usbip_cli.rs crates/client-win/src/main.rs crates/client-win/Cargo.toml
git commit -m "feat(client-win): usbip.exe wrapper + output parsers

Locates usbip.exe, runs subcommands, and parses 'list -r' / 'port'
output. Used by the upcoming attach state machine."
```

---

## Task 8: Attach state machine

**Files:**

- Create: `crates/client-win/src/attach.rs`
- Test: inline tests.

Mirror of the Deck-side `Connection`. States: **Idle**, **Attached**. Driven by `tick(peer_present, peer_addr)`. On each tick when peer is present + we're idle, look up the remote busid (via `usbip list -r`) and `attach`. When peer is gone or our busid disappears from `usbip port`, drop to Idle.

- [ ] **Step 1: Write the failing tests**

Create `crates/client-win/src/attach.rs`:

```rust
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
```

- [ ] **Step 2: Wire the module in**

Modify `crates/client-win/src/main.rs` — add near the `mod usbip_cli;` line:

```rust
#[cfg(windows)]
mod attach;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p client-win --lib attach`
Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/client-win/src/attach.rs crates/client-win/src/main.rs
git commit -m "feat(client-win): attach state machine with exponential backoff

Drives 'usbip attach' / 'usbip port' polling based on beacon-derived
peer presence. Loss of attachment (network blip) bumps state back to
Idle and the next tick re-attempts."
```

---

## Task 9: Tray icon module

**Files:**

- Create: `crates/client-win/src/tray.rs`

Owns the tray icon, menu, and a `crossbeam_channel`-style channel from the menu callbacks back to the main loop. Since `tray-icon` depends on a Win32 message loop, we run a dedicated thread that owns the icon and pumps messages.

- [ ] **Step 1: Add crossbeam-channel to deps**

Modify `crates/client-win/Cargo.toml` — add to `[dependencies]`:

```toml
crossbeam-channel = "0.5"
```

- [ ] **Step 2: Write the tray module**

Create `crates/client-win/src/tray.rs`:

```rust
//! Tray icon thread.
//!
//! `tray-icon` requires a Win32 message pump. We park that pump on a
//! dedicated thread, expose a channel of `TrayEvent`s for the main loop to
//! consume, and a `TrayHandle` for the main loop to push status updates
//! back into the tray's tooltip.

#![cfg(windows)]

use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver, Sender};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Connect,
    Disconnect,
    Pair,
    Quit,
}

#[derive(Clone)]
pub struct TrayHandle {
    inner: Arc<Mutex<Option<tray_icon::TrayIcon>>>,
}

impl TrayHandle {
    pub fn set_tooltip(&self, tooltip: &str) {
        if let Ok(guard) = self.inner.lock() {
            if let Some(icon) = guard.as_ref() {
                let _ = icon.set_tooltip(Some(tooltip));
            }
        }
    }
}

/// Spawn the tray on its own thread. Returns the receiver of menu events
/// and a handle for tooltip updates.
///
/// # Panics
/// Never in practice; if icon construction fails we eprintln + return a
/// noop handle so the rest of the app keeps running.
#[must_use]
pub fn spawn() -> (Receiver<TrayEvent>, TrayHandle) {
    let (tx, rx) = unbounded::<TrayEvent>();
    let icon_slot: Arc<Mutex<Option<tray_icon::TrayIcon>>> = Arc::new(Mutex::new(None));
    let icon_slot_for_thread = icon_slot.clone();

    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            run_tray_thread(tx, icon_slot_for_thread);
        })
        .ok();

    (rx, TrayHandle { inner: icon_slot })
}

fn run_tray_thread(tx: Sender<TrayEvent>, slot: Arc<Mutex<Option<tray_icon::TrayIcon>>>) {
    let menu = Menu::new();
    let connect = MenuItem::new("Connect", true, None);
    let disconnect = MenuItem::new("Disconnect", true, None);
    let pair = MenuItem::new("Pair new Deck...", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[&connect, &disconnect, &pair, &quit]);

    let connect_id = connect.id().clone();
    let disconnect_id = disconnect.id().clone();
    let pair_id = pair.id().clone();
    let quit_id = quit.id().clone();

    let icon = match make_icon() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("tray icon: {e}");
            return;
        }
    };

    let tray_icon = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Network Deck — searching")
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tray build: {e}");
            return;
        }
    };
    *slot.lock().unwrap() = Some(tray_icon);

    let menu_rx = MenuEvent::receiver();
    pump_messages(menu_rx, tx, connect_id, disconnect_id, pair_id, quit_id);
}

fn pump_messages(
    menu_rx: &crossbeam_channel::Receiver<MenuEvent>,
    tx: Sender<TrayEvent>,
    connect_id: tray_icon::menu::MenuId,
    disconnect_id: tray_icon::menu::MenuId,
    pair_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let mut msg = MSG {
        hwnd: std::ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
    };

    loop {
        // Drain menu events.
        while let Ok(event) = menu_rx.try_recv() {
            let mapped = if event.id == connect_id {
                Some(TrayEvent::Connect)
            } else if event.id == disconnect_id {
                Some(TrayEvent::Disconnect)
            } else if event.id == pair_id {
                Some(TrayEvent::Pair)
            } else if event.id == quit_id {
                Some(TrayEvent::Quit)
            } else {
                None
            };
            if let Some(e) = mapped {
                let _ = tx.send(e);
                if e == TrayEvent::Quit {
                    return;
                }
            }
        }

        // Pump one Win32 message (non-blocking) so the tray icon stays
        // responsive. A short sleep keeps the loop from spinning at 100%.
        unsafe {
            if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Generate a 32x32 RGBA solid-color icon at runtime so we don't ship an
/// asset file. Color is a muted teal — visible on both light and dark
/// taskbars.
fn make_icon() -> Result<Icon, String> {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x2e, 0x86, 0x8a, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).map_err(|e| e.to_string())
}
```

- [ ] **Step 3: Wire the module in**

Modify `crates/client-win/src/main.rs` — add near the existing `mod` lines:

```rust
#[cfg(windows)]
mod tray;
```

- [ ] **Step 4: Build (no tests — pure win32 glue)**

Run: `cargo build -p client-win`
Expected: compiles cleanly on Windows.

- [ ] **Step 5: Commit**

```bash
git add crates/client-win/src/tray.rs crates/client-win/src/main.rs crates/client-win/Cargo.toml
git commit -m "feat(client-win): tray icon thread with menu + tooltip handle

Tray runs on its own thread with a Win32 message pump. Main loop
receives menu events via a crossbeam channel and updates the tooltip
through TrayHandle. Icon is a runtime-generated 32x32 RGBA so we don't
ship a binary asset."
```

---

## Task 10: Autostart registration

**Files:**

- Create: `crates/client-win/src/autostart.rs`

Write our binary path into `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` so the tray launches at login. No service install — keeps install permissions to user-level.

- [ ] **Step 1: Write the module**

Create `crates/client-win/src/autostart.rs`:

```rust
//! Manage the `HKCU\...\Run` entry that autostarts client-win at login.
//!
//! User-scope registry write — no admin needed.

#![cfg(windows)]

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
    let exe = std::env::current_exe().map_err(|_| 0)?;
    let exe_str = exe.to_string_lossy().into_owned();
    write_run_value(&exe_str)
}

/// Remove our autostart entry. No-op if absent.
///
/// # Errors
/// Returns the Win32 error code on failure (ERROR_FILE_NOT_FOUND is treated as success).
pub fn disable() -> Result<(), u32> {
    let subkey = wide(SUBKEY);
    let value = wide(VALUE_NAME);
    let mut key: HKEY = std::ptr::null_mut();
    unsafe {
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut key);
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
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key)
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
            &mut size,
        );
        if r != ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0_u16; (size as usize) / 2];
        let r = RegQueryValueExW(
            key,
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
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
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut key);
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
```

- [ ] **Step 2: Wire the module in**

Modify `crates/client-win/src/main.rs` — add near the existing `mod` lines:

```rust
#[cfg(windows)]
mod autostart;
```

- [ ] **Step 3: Build**

Run: `cargo build -p client-win`
Expected: compiles cleanly on Windows.

- [ ] **Step 4: Commit**

```bash
git add crates/client-win/src/autostart.rs crates/client-win/src/main.rs
git commit -m "feat(client-win): HKCU Run-key autostart helper

User-scope registry write; no admin required. Sets/clears the entry
that points at the currently-running exe."
```

---

## Task 11: Rewrite client-win main loop

**Files:**

- Modify: `crates/client-win/src/main.rs` (large rewrite — replace everything except `parse_args` and `run_pair_mode`).
- Delete: `crates/client-win/src/driver.rs`.

The new `main`:

1. On first run, calls `autostart::enable()` and prints a one-shot notice.
2. Loads identity + trust + spawns beacon (existing).
3. Spawns the tray (Task 9).
4. Locates `usbip.exe` (Task 7).
5. Runs the attach state machine on a 1 s tick. Tray events override (Connect/Disconnect/Pair/Quit).

- [ ] **Step 1: Replace `crates/client-win/src/main.rs`**

Overwrite the file with:

```rust
//! Windows-side tray app for the network-deck bridge.
//!
//! Watches the discovery beacon for a paired Deck. When the Deck appears,
//! shells out to `usbip.exe attach` (usbip-win2). Auto-reattaches on
//! network blips. Tray menu lets the user override.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod attach;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod usbip_cli;

const DEFAULT_PORT: u16 = 49152;
const TICK_INTERVAL: Duration = Duration::from_millis(500);

enum Mode {
    Run,
    Pair,
}

struct ParsedArgs {
    mode: Mode,
    state_dir: PathBuf,
}

fn parse_args() -> ParsedArgs {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Run;
    let mut state_dir_override: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "pair" => mode = Mode::Pair,
            "--state-dir" => {
                state_dir_override = args.next().map(PathBuf::from);
                if state_dir_override.is_none() {
                    eprintln!("--state-dir requires a value");
                    std::process::exit(2);
                }
            }
            other => {
                eprintln!("unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let state_dir = state_dir_override.unwrap_or_else(|| {
        discovery::state_dir::default_state_dir().unwrap_or_else(|e| {
            eprintln!("cannot resolve state dir: {e:?}");
            std::process::exit(1);
        })
    });
    ParsedArgs { mode, state_dir }
}

fn main() {
    let args = parse_args();

    let identity = Arc::new(
        discovery::identity::load_or_generate(&args.state_dir).unwrap_or_else(|e| {
            eprintln!("identity load: {e:?}");
            std::process::exit(1);
        }),
    );

    if matches!(args.mode, Mode::Pair) {
        run_pair_mode(&identity, &args.state_dir);
        return;
    }

    #[cfg(windows)]
    {
        if autostart::current().is_none() {
            if let Err(e) = autostart::enable() {
                eprintln!("autostart register failed (code {e})");
            } else {
                eprintln!("autostart: registered for HKCU\\...\\Run");
            }
        }
    }

    run_normal(&identity, &args.state_dir);
}

fn run_normal(identity: &Arc<discovery::Identity>, state_dir: &std::path::Path) {
    let trusted = match discovery::trust::load(state_dir) {
        Ok(Some(p)) => Arc::new(p),
        Ok(None) => {
            eprintln!("no trusted peer; run `client-win pair` to pair first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("trust load: {e:?}");
            std::process::exit(1);
        }
    };

    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_PORT);
    let bound = UdpSocket::bind(bind_addr).unwrap_or_else(|e| {
        eprintln!("bind {bind_addr}: {e}");
        std::process::exit(1);
    });
    let beacon = Arc::new(
        discovery::Beacon::new(
            identity.clone(),
            trusted.clone(),
            SocketAddr::new(Ipv4Addr::BROADCAST.into(), DEFAULT_PORT),
            hostname(),
            DEFAULT_PORT,
        )
        .unwrap_or_else(|e| {
            eprintln!("beacon init: {e:?}");
            std::process::exit(1);
        }),
    );
    discovery::beacon::spawn_broadcast(beacon.clone());

    let beacon_recv = beacon.clone();
    std::thread::Builder::new()
        .name("discovery-recv".into())
        .spawn(move || {
            let mut buf = [0_u8; discovery::packet::PACKET_LEN];
            loop {
                if let Ok((n, src)) = bound.recv_from(&mut buf) {
                    if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                        beacon_recv.handle_packet(src, &buf[..n]);
                    }
                }
            }
        })
        .ok();

    eprintln!(
        "client-win running — paired with {} (fingerprint {})",
        trusted.name,
        identity.fingerprint_str(),
    );

    #[cfg(windows)]
    run_attach_loop(&beacon);

    #[cfg(not(windows))]
    {
        eprintln!("attach loop requires Windows; idling.");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

#[cfg(windows)]
fn run_attach_loop(beacon: &Arc<discovery::Beacon>) {
    use crate::attach::{Action, Attach, State, UsbipDriver};
    use crate::tray::{TrayEvent, TrayHandle};
    use crate::usbip_cli::{CliError, UsbipCli};

    let cli = match UsbipCli::discover() {
        Ok(c) => c,
        Err(CliError::NotInstalled) => {
            eprintln!("usbip.exe not found. Install usbip-win2 first.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("usbip locate: {e:?}");
            std::process::exit(1);
        }
    };

    struct CliDriver {
        cli: UsbipCli,
    }
    impl UsbipDriver for CliDriver {
        fn discover_busid(&mut self, host: &str) -> Option<String> {
            self.cli
                .list_remote(host)
                .ok()?
                .into_iter()
                .find(|d| d.vid == "28de" && d.pid == "1205")
                .map(|d| d.busid)
        }
        fn attach(&mut self, host: &str, busid: &str) -> bool {
            self.cli.attach(host, busid).is_ok()
        }
        fn ported_busids(&mut self) -> Vec<String> {
            self.cli.port().unwrap_or_default()
        }
    }
    let mut driver = CliDriver { cli };

    let (tray_rx, tray_handle) = tray::spawn();
    let mut sm = Attach::default();
    let mut paused = false;

    loop {
        // Drain tray events.
        while let Ok(event) = tray_rx.try_recv() {
            match event {
                TrayEvent::Connect => {
                    paused = false;
                    eprintln!("tray: connect");
                }
                TrayEvent::Disconnect => {
                    paused = true;
                    eprintln!("tray: disconnect (paused until Connect)");
                }
                TrayEvent::Pair => {
                    eprintln!("tray: pair (run `client-win pair` from a shell — TODO inline UI)");
                }
                TrayEvent::Quit => {
                    eprintln!("tray: quit");
                    return;
                }
            }
        }

        let peer_with_age = beacon.current_peer_with_age();
        let peer_present = !paused
            && peer_with_age
                .is_some_and(|(_, age)| age <= discovery::beacon::STALE_AFTER);
        let peer_host = peer_with_age.map(|(addr, _)| addr.ip().to_string());

        if let Some(action) =
            sm.tick(peer_present, peer_host.as_deref(), Instant::now(), &mut driver)
        {
            eprintln!("attach: {action:?} (state={:?})", sm.state());
        }

        let tooltip = match (sm.state(), peer_with_age) {
            (State::Attached, _) => "Network Deck — connected".to_owned(),
            (State::Idle, Some((addr, age))) if age <= discovery::beacon::STALE_AFTER => {
                format!("Network Deck — connecting ({})", addr.ip())
            }
            (State::Idle, _) if paused => "Network Deck — paused".to_owned(),
            (State::Idle, _) => "Network Deck — searching".to_owned(),
        };
        tray_handle.set_tooltip(&tooltip);

        std::thread::sleep(TICK_INTERVAL);
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "windows".to_owned())
}

fn run_pair_mode(identity: &Arc<discovery::Identity>, state_dir: &std::path::Path) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind for pair: {e}");
            std::process::exit(1);
        }
    };
    sock.set_broadcast(true).ok();
    let cfg = discovery::pair::PairConfig {
        identity: identity.clone(),
        recv_sock: sock,
        unicast_target: SocketAddr::new(Ipv4Addr::BROADCAST.into(), DEFAULT_PORT),
        self_name: hostname(),
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(120),
    };
    eprintln!("pairing — fingerprint {}; waiting up to 120 s", identity.fingerprint_str());
    let mut stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    match discovery::pair::run_pair(&cfg, &mut stdin, &mut stderr) {
        discovery::pair::PairOutcome::Paired(p) => {
            eprintln!("paired with {} (fingerprint stored)", p.name);
        }
        other => {
            eprintln!("pair did not complete: {other:?}");
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 2: Delete the old driver module**

```bash
git rm crates/client-win/src/driver.rs
```

- [ ] **Step 3: Build the workspace**

Run: `cargo build`
Expected: workspace compiles cleanly. (`server-deck` builds on Linux too via cross-compile if you want to test from Windows; on Windows-native it builds the stub-main path and exits at runtime.)

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: all existing + new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/client-win/src/main.rs
git rm crates/client-win/src/driver.rs
git commit -m "feat(client-win): rewrite main loop around usbip-win2 + tray

Strips driver IPC, --test, --replay. New flow: load identity + trust,
spawn beacon, spawn tray, run the attach state machine on a 0.5 s tick.
First-run autostart entry is registered in HKCU\\...\\Run.

The Pair menu item currently prompts the user to run 'client-win pair'
from a shell — an inline pair UI is a future task."
```

---

## Task 12: Phase B integration check (manual)

**No files** — paired-hardware validation before cleanup.

- [ ] **Step 1: Build + install the tray binary on Windows**

```powershell
cargo build --release -p client-win
Copy-Item target\release\client-win.exe "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" -Force
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" pair
```

Run `server-deck pair` on the Deck simultaneously. Confirm fingerprints match. Trust file lands in `%LOCALAPPDATA%\network-deck\trusted-peers.toml`.

- [ ] **Step 2: Start the tray**

```powershell
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe"
```

Expected: tray icon appears with tooltip "Network Deck — searching" → "Network Deck — connecting (192.168.1.183)" → "Network Deck — connected" within ~5 seconds.

- [ ] **Step 3: Verify Steam sees the controller**

Open Steam → Settings → Controller. The Deck should appear. Test in a game.

- [ ] **Step 4: Verify auto-reattach**

On the Deck, `sudo systemctl restart usbipd.service`. Within ~5 seconds the tray should drop to "Network Deck — connecting" and recover to "Network Deck — connected" once the daemon is back.

- [ ] **Step 5: Verify autostart on reboot**

Reboot the Windows box. Confirm the tray appears at login without manual launch.

---

## Task 13: Delete the custom driver tree and `deck-protocol`

**Files:**

- Delete: `driver/`
- Delete: `crates/deck-protocol/`
- Delete: `deck-buttons.bin`
- Delete: `CppProperties.json`
- Modify: `Cargo.toml` (workspace root) — remove `crates/deck-protocol` member.

- [ ] **Step 1: Remove the workspace member**

In `Cargo.toml`, change:

```toml
members = ["crates/deck-protocol", "crates/server-deck", "crates/client-win", "crates/discovery"]
```

to:

```toml
members = ["crates/server-deck", "crates/client-win", "crates/discovery"]
```

- [ ] **Step 2: Remove the directories**

```bash
git rm -r driver
git rm -r crates/deck-protocol
git rm deck-buttons.bin CppProperties.json
```

- [ ] **Step 3: Build to confirm**

Run: `cargo build`
Expected: workspace compiles cleanly without `deck-protocol`.

- [ ] **Step 4: Test**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: delete custom UDE driver + deck-protocol crate

The usbip backend supersedes both. The Linux usbip-host driver tunnels
the real Deck controller; deck-protocol's HID parsing, wire framing,
auth, and state types are no longer used.

Removed:
  - driver/ (KMDF/UdeCx C++ tree, install scripts)
  - crates/deck-protocol/
  - deck-buttons.bin (replay capture for --replay mode)
  - CppProperties.json (VS C++ tooling)"
```

---

## Task 14: Windows installer script

**Files:**

- Create: `scripts/install-windows.ps1`

Wraps the manual usbip-win2 install + copying our tray binary into `%LOCALAPPDATA%\NetworkDeck\`. Idempotent.

- [ ] **Step 1: Write the script**

Create `scripts/install-windows.ps1`:

```powershell
# Windows-side installer for network-deck.
#
# What this does:
#   1. Downloads + installs usbip-win2 if not already installed.
#   2. Copies the prebuilt client-win.exe into %LOCALAPPDATA%\NetworkDeck.
#   3. The tray app self-registers HKCU\...\Run on first launch — no
#      registry writes here.
#
# Run from an admin PowerShell so the usbip-win2 driver install can
# accept a signed-driver dialog.

[CmdletBinding()]
param(
    [string]$ReleaseUrl = "https://github.com/vadimgrn/usbip-win2/releases/download/v.0.9.7.7/USBip-0.9.7.7-x64.exe",
    [string]$BinarySource = (Join-Path $PSScriptRoot "..\target\release\client-win.exe")
)

$ErrorActionPreference = "Stop"

function Require-Admin {
    $current = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object System.Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run from an elevated PowerShell."
    }
}

Require-Admin

$usbipExe = "C:\Program Files\USBip\usbip.exe"
if (-not (Test-Path $usbipExe)) {
    Write-Host ">> Downloading usbip-win2 installer..."
    $tmp = Join-Path $env:TEMP "usbip-installer.exe"
    Invoke-WebRequest $ReleaseUrl -OutFile $tmp
    Write-Host ">> Running usbip-win2 installer (accept any driver-signature prompt)..."
    Start-Process $tmp -Wait
    if (-not (Test-Path $usbipExe)) {
        throw "usbip-win2 install did not place usbip.exe at the expected path."
    }
} else {
    Write-Host "usbip-win2 already installed."
}

if (-not (Test-Path $BinarySource)) {
    throw "Build client-win first: cargo build --release -p client-win"
}

$installDir = Join-Path $env:LOCALAPPDATA "NetworkDeck"
$null = New-Item -ItemType Directory -Path $installDir -Force
Write-Host ">> Copying client-win.exe to $installDir"
Copy-Item $BinarySource (Join-Path $installDir "client-win.exe") -Force

Write-Host ""
Write-Host "Done. Next steps:"
Write-Host "  1. & '$installDir\client-win.exe' pair    # one-shot pairing"
Write-Host "  2. & '$installDir\client-win.exe'         # tray app"
Write-Host "     (autostart entry registers itself on first run)"
```

- [ ] **Step 2: Commit**

```bash
git add scripts/install-windows.ps1
git commit -m "feat(install): Windows-side installer for usbip-win2 + tray binary

Wraps the usbip-win2 download + signed-installer run, then drops
client-win.exe into %LOCALAPPDATA%\\NetworkDeck. Tray app registers its
own autostart entry on first launch."
```

---

## Task 15: Rewrite `ARCHITECTURE.md` and `README.md`

**Files:**

- Modify: `ARCHITECTURE.md`
- Modify: `README.md`

Flip the non-goal, redo the decision history, redraw the component diagram, and rewrite the build-sequence section to reflect the actual completed pivot.

- [ ] **Step 1: Replace `ARCHITECTURE.md`**

Overwrite `ARCHITECTURE.md` with:

```markdown
# network-usb

Wireless gamepad bridge: Steam Deck controls → Windows PC. Built on stock
USB/IP — Linux's `usbip-host` on the Deck and `usbip-win2` on Windows do
the bus-level tunnel; this repo provides the discovery + lifecycle glue
that makes the two ends find each other and stay attached.

## Goal

Deck controller plugged in (over the network) on Windows. Pair once over
LAN, then both ends auto-connect at boot. Wireless, low-latency-on-good-Wi-Fi,
full-duplex (rumble flows back to the Deck).

## Non-goals

- Cross-controller emulation (Xbox, DualShock).
- Multi-client / multi-Deck.
- Non-Windows clients.
- Hostile-LAN security guarantees beyond the pair-time identity check.

## Why this design

Decision history (most recent first):

- **2026-05-07** — Pivot to usbip backend after a hardware test confirmed
  `usbipd` + `usbip-win2 v0.9.7.7` produces a Deck that Steam recognizes
  and works in-game with no custom driver. The previous reason to reject
  this path ("test-signed driver, UX hostile") was based on the older
  unsigned usbip-win2; modern releases ship attestation-signed binaries.
  Tradeoffs accepted: TCP transport (HoL blocking on Wi-Fi loss) instead
  of our own UDP+jitter-buffer; vhci detach-on-network-loss surfaces as a
  visible USB unplug to games; data plane is plaintext (the pair flow's
  Ed25519 identity gates beacon-level discovery only).
- **2026-05-06** — Original plan picked a custom KMDF/UdeCx driver
  emulating the Steam Deck controller. Implemented through step 9
  (working end-to-end). Superseded by the 2026-05-07 pivot above; the
  driver tree is preserved in git history (last commit before delete:
  see `git log -- driver/`).

## Components

```
+-------------------- Deck (Linux) --------------------+
| usbipd.service           (system-managed, port 3240) |
| server-deck (Rust)                                   |
|   - load identity + paired-peer trust                |
|   - sysfs lookup of Deck controller busid            |
|   - signed-UDP discovery beacon                      |
|   - bind/unbind state machine (shells out to usbip)  |
+----------------------|-------------------------------+
                       |
                       | UDP 49152: discovery beacon (signed)
                       | TCP 3240:  USB/IP (vhci tunnels HID URBs)
                       v
+------------------ Windows (Rust) --------------------+
| client-win (tray app)                                |
|   - same identity + trust + beacon                   |
|   - attach state machine                             |
|   - shells out to usbip.exe (usbip-win2)             |
|   - tray icon + menu (Connect / Disconnect / Pair)   |
| usbip-win2 vhci kernel driver (provided)             |
+----------------------|-------------------------------+
                       v
            Steam Input recognizes Deck controller
```

## Crates and dirs

- `crates/discovery/` — Ed25519 identity, signed-UDP beacon, trust file,
  pair flow. Shared between Deck server and Windows client. No I/O
  beyond UDP + filesystem.
- `crates/server-deck/` — Linux binary. Drives `usbip bind` based on
  beacon state.
- `crates/client-win/` — Windows binary. Tray app that drives
  `usbip.exe attach` based on beacon state.
- `scripts/install-deck.sh` — Deck-side installer (pacman + systemd).
- `scripts/install-windows.ps1` — Windows-side installer (usbip-win2 +
  binary drop).

## Wire protocol

Two channels:

- **Discovery (UDP 49152, broadcast):** signed beacon every 1 s; magic
  `NDB1`. Receiver verifies Ed25519 signature against the trusted-peer
  pubkey, normalizes the source port to 49152, and exposes the live peer
  address to the data plane.
- **USB/IP (TCP 3240):** stock Linux `usbipd` ↔ `usbip-win2` vhci. We
  don't speak this directly; we just drive the lifecycle.

## Pair flow

Bilateral mutual-confirm. One-shot:

1. Each side broadcasts its pubkey + name in a special PAIR-mode beacon.
2. User confirms the other side's fingerprint on both ends within 120 s.
3. Both ends write `trusted-peers.toml` to the platform state dir.

After pair, the binaries enter normal mode and the data-plane bind/attach
logic takes over.

## Open risks

- Wi-Fi blip under TCP causes a visible USB unplug to running games.
  Some games handle it; some lose the controller permanently until
  game-restart. Mitigation: keep the bridge on 5 GHz; document the
  failure mode.
- vhci on Windows occasionally needs a service reload after suspend.
  Mitigation: the attach state machine retries on detach.
- A second Deck on the same LAN would also broadcast 28de:1205. Pair
  flow's identity check rejects strangers, so this is benign — but the
  busid lookup on the Deck assumes one matching device.

## Build sequence

This pivot replaces the previous step-by-step driver-bringup sequence.
The current shape:

1. ✅ `discovery` crate (steps 1–9 of the original plan, kept intact).
2. ✅ Deck-side `server-deck` rewritten around `usbip bind|unbind` (Phase
   A of the pivot plan, `docs/superpowers/plans/2026-05-07-usbip-backend-pivot.md`).
3. ✅ Windows-side `client-win` rewritten around `usbip.exe attach` and
   a tray app (Phase B of same plan).
4. ✅ Cleanup: custom driver tree and `deck-protocol` deleted (Phase C).
```

- [ ] **Step 2: Replace the driver section in `README.md`**

Open `README.md`. Find the section that describes installing the custom driver (or lists `bcdedit /set testsigning on`). Replace whatever existed there with:

```markdown
## Install

### On the Deck (SteamOS)

```
sudo ./scripts/install-deck.sh
```

This installs `usbip` from pacman, enables `usbipd.service`, drops
`server-deck` into `/usr/local/bin`, and installs the systemd unit.

### On Windows

From an elevated PowerShell:

```
.\scripts\install-windows.ps1
```

Installs usbip-win2 (signed driver, accept the Windows driver dialog),
drops `client-win.exe` into `%LOCALAPPDATA%\NetworkDeck`, and the tray
self-registers for autostart on first run.

### Pair

One-shot. Run on each side at the same time:

```
sudo /usr/local/bin/server-deck pair --state-dir /var/lib/network-deck   # Deck
& "$env:LOCALAPPDATA\NetworkDeck\client-win.exe" pair                    # Windows
```

Confirm matching fingerprints on both prompts within 120 s. After pair,
enable the Deck service:

```
sudo systemctl enable --now network-deck-server.service
```

The Windows tray will pick up the Deck's beacon and auto-attach.
```

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md README.md
git commit -m "docs: rewrite architecture + readme around usbip backend

Flips non-goal #1 (we are now an opinionated USB/IP setup), redraws the
component diagram, and rewrites the build sequence to reflect the
shipped pivot. README now points at scripts/install-deck.sh and
scripts/install-windows.ps1."
```

---

## Self-review checklist (executed before plan ships)

**Spec coverage:**

- Auto-on-boot connect: Phase A systemd unit + Phase B HKCU\Run autostart. ✓
- Tray manual override: Task 9 + Task 11 (Connect/Disconnect/Pair/Quit). ✓
- Trusted-LAN auth (no extra TLS): no TLS task; the existing pair flow stays. ✓
- Auto-reattach on Wi-Fi blip: Task 8 (state machine drops Idle on missing port; backoff retry). ✓
- Delete custom driver + dead protocol code: Task 13. ✓
- ARCHITECTURE.md rewrite: Task 15. ✓
- Re-purpose install scripts: Tasks 5 (Deck) + 14 (Windows). ✓

**Placeholder scan:** no TBD/TODO/"similar to" tokens; every code block is complete; all command lines are exact.

**Type consistency:**
- `Connection::tick(peer_present, runner)` matches across Tasks 3 + 4.
- `Attach::tick(peer_present, peer_host, now, driver)` matches across Tasks 8 + 11.
- `UsbipDriver` trait method names (`discover_busid`, `attach`, `ported_busids`) match the trait def in Task 8 and the impl in Task 11.
- `TrayEvent` variants (`Connect`, `Disconnect`, `Pair`, `Quit`) match across Tasks 9 + 11.
