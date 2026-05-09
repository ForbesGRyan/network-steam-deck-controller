# Pre-release test automation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the appendix gaps in `docs/superpowers/specs/2026-05-09-pre-release-test-plan.md` with hardware-free Rust regression tests, so the manual checklist gets shorter and CI catches relevant regressions before they reach the lab.

**Architecture:** All work lives inside the existing `crates/discovery`, `crates/network-deck`, `crates/client-win` crates. No new crates. Tests are unit tests where the logic is already isolated behind a trait or pure function; the one small refactor extracts the daemon's `bind_error` threshold rule into a pure helper so it can be tested without spinning up the daemon. Every test runs on Windows and Linux (no platform gates) and runs as part of `cargo make test`.

**Tech Stack:** Rust 2021, `cargo test`, `tempfile` (already a dev-dep in `discovery`), no new crates.

---

## Spec coverage map

These appendix gaps from the test plan are addressed here:

| Spec gap | Closed by task |
| --- | --- |
| B.2 stranger Deck rejection (synthetic, no 2nd Deck) | Task 1 + Task 2 |
| C/D replay-window defense (timestamp tampering) | Task 3 |
| D.1 / D.2 Wi-Fi blip — attach state machine recovers | Task 4 |
| D.5 peer IP change — beacon updates live address | Task 5 |
| D.6 `bind_error` threshold semantics | Task 6 |

These spec items are NOT covered by this plan (and remain manual checklist items): A.1–A.6 install paths, B.1 happy-path pair, C.2–C.4 Steam recognition / input surface / rumble, C.6 soak, C.7/C.8 sleep/resume, D.3 daemon SIGKILL, D.4 GUI close mid-game, D.7 SYN_DROPPED, D.8 firewall recv-thread, E.1/E.2 uninstall.

## File touchlist

- Create: `crates/discovery/tests/stranger_beacon_during_active.rs` (Task 2 — wire-level integration test)
- Modify: `crates/discovery/src/beacon.rs` (Task 1, 3, 5 — extend the existing `mod tests`)
- Modify: `crates/client-win/src/attach.rs` (Task 4 — extend the existing `mod tests`)
- Modify: `crates/network-deck/src/daemon.rs` (Task 6 — extract helper, call site refactor)
- Create: `crates/network-deck/src/bind_error.rs` (Task 6 — pure helper module)
- Modify: `crates/network-deck/src/main.rs` (Task 6 — declare new module)

---

### Task 1: Beacon — stranger packet does not overwrite an established live peer

**Why:** Closes part of B.2. The existing `handle_packet_from_stranger_ignored` test only proves the stranger is rejected when no peer is live yet. The more interesting case is a stranger (or a forged packet from another identity) arriving while the trusted peer is already active — it must not stomp the `live` slot.

**Files:**
- Modify: `crates/discovery/src/beacon.rs` (extend `mod tests` at the bottom)

- [ ] **Step 1: Add the failing test**

Append this test to the `mod tests` block in `crates/discovery/src/beacon.rs`, immediately after `handle_packet_wrong_peer_fpr_dropped`:

```rust
    #[test]
    fn stranger_does_not_overwrite_active_peer() {
        let me = make_identity();
        let them = make_identity();
        let stranger = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();

        // 1) Trusted peer establishes the live slot.
        let trusted_pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &trusted_pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);
        let trusted_addr = beacon.current_peer().unwrap();
        assert_eq!(trusted_addr.ip().to_string(), "192.168.1.42");

        // 2) Stranger arrives from a different IP. Must be dropped; live slot
        //    must still point at the trusted peer's IP.
        let stranger_pkt = BeaconPacket {
            flags: 0,
            pubkey: stranger.pubkey,
            peer_fpr: [0; FPR_LEN],
            timestamp_us: now_us(),
            name: "stranger".into(),
        };
        let mut sbuf = [0_u8; PACKET_LEN];
        packet::sign_into(&stranger.signing, &stranger_pkt, &mut sbuf).unwrap();
        beacon.handle_packet("10.0.0.99:55555".parse().unwrap(), &sbuf);

        let after = beacon.current_peer().unwrap();
        assert_eq!(after, trusted_addr, "stranger must not overwrite live peer");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p discovery --lib beacon::tests::stranger_does_not_overwrite_active_peer`
Expected: PASS (this is a characterization test — the existing pubkey check at `beacon.rs:118` already enforces the behaviour). If it FAILS, that is a real regression and a ship-blocker.

- [ ] **Step 3: Run the full beacon suite to make sure nothing else regressed**

Run: `cargo test -p discovery --lib beacon::`
Expected: 5 passed (4 existing + 1 new), 0 failed.

- [ ] **Step 4: Commit**

```bash
git add crates/discovery/src/beacon.rs
git commit -m "test(discovery): stranger beacon does not overwrite active peer"
```

---

### Task 2: Beacon wire-level — stranger broadcast is dropped end-to-end

**Why:** Closes the rest of B.2. Task 1 proves the in-process `handle_packet` rejects strangers; this test proves the same when bytes arrive from a real UDP socket, so a future refactor of the recv loop cannot accidentally bypass the check. This sits in `tests/` (integration test, separate process) on purpose.

**Files:**
- Create: `crates/discovery/tests/stranger_beacon_during_active.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/discovery/tests/stranger_beacon_during_active.rs`:

```rust
//! Integration test: a stranger broadcasting on UDP at our recv socket
//! does not poison the beacon's live-peer slot, even after the trusted
//! peer has already been seen. Mirrors a "second Deck on the LAN"
//! scenario without a second physical Deck.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use discovery::beacon::Beacon;
use discovery::identity::load_or_generate;
use discovery::packet::{self, fingerprint, BeaconPacket, FPR_LEN, PACKET_LEN};
use discovery::trust::TrustedPeer;
use tempfile::tempdir;

fn ephemeral() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

// `discovery::time` is pub(crate); inline an equivalent helper here rather
// than widening the discovery public API for one test.
fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

#[test]
fn stranger_udp_broadcast_does_not_poison_live_peer() {
    let dir_me = tempdir().unwrap();
    let dir_them = tempdir().unwrap();
    let dir_stranger = tempdir().unwrap();
    let me = Arc::new(load_or_generate(dir_me.path()).unwrap());
    let them = Arc::new(load_or_generate(dir_them.path()).unwrap());
    let stranger = Arc::new(load_or_generate(dir_stranger.path()).unwrap());

    let peer = Arc::new(TrustedPeer {
        pubkey: them.pubkey,
        name: "them".into(),
        paired_at: "2026-05-07T00:00:00Z".into(),
        last_seen_addr: None,
    });

    // The data-plane recv socket the beacon advertises.
    let recv = UdpSocket::bind(ephemeral()).unwrap();
    let recv_port = recv.local_addr().unwrap().port();
    recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    let beacon = Beacon::new(
        me.clone(),
        peer.clone(),
        vec![ephemeral()],
        "me".into(),
        recv_port,
    )
    .unwrap();

    // The trusted peer broadcasts.
    let trusted_pkt = BeaconPacket {
        flags: 0,
        pubkey: them.pubkey,
        peer_fpr: fingerprint(&me.pubkey),
        timestamp_us: now_us(),
        name: "them".into(),
    };
    let mut tbuf = [0_u8; PACKET_LEN];
    packet::sign_into(&them.signing, &trusted_pkt, &mut tbuf).unwrap();
    let trusted_send = UdpSocket::bind(ephemeral()).unwrap();
    trusted_send
        .send_to(&tbuf, recv.local_addr().unwrap())
        .unwrap();

    // Receive the trusted packet and feed it to the beacon.
    let mut rbuf = [0_u8; PACKET_LEN];
    let (n, src) = recv.recv_from(&mut rbuf).unwrap();
    assert_eq!(n, PACKET_LEN);
    beacon.handle_packet(src, &rbuf[..n]);
    let trusted_live = beacon.current_peer().expect("trusted peer should be live");

    // Stranger broadcasts from a different ephemeral socket.
    let stranger_pkt = BeaconPacket {
        flags: 0,
        pubkey: stranger.pubkey,
        peer_fpr: [0; FPR_LEN],
        timestamp_us: now_us(),
        name: "stranger".into(),
    };
    let mut sbuf = [0_u8; PACKET_LEN];
    packet::sign_into(&stranger.signing, &stranger_pkt, &mut sbuf).unwrap();
    let stranger_send = UdpSocket::bind(ephemeral()).unwrap();
    stranger_send
        .send_to(&sbuf, recv.local_addr().unwrap())
        .unwrap();

    let (n2, src2) = recv.recv_from(&mut rbuf).unwrap();
    assert_eq!(n2, PACKET_LEN);
    beacon.handle_packet(src2, &rbuf[..n2]);

    // Live peer slot must be unchanged — stranger did not poison it.
    assert_eq!(
        beacon.current_peer(),
        Some(trusted_live),
        "stranger UDP broadcast must not change live peer"
    );
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p discovery --tests`
Expected: builds cleanly. The test deliberately inlines `now_us` so it doesn't depend on `discovery::time` being public.

- [ ] **Step 3: Run it**

Run: `cargo test -p discovery --test stranger_beacon_during_active`
Expected: 1 passed. If FAIL, the recv path is bypassing the trusted-peer check — ship-blocker.

- [ ] **Step 4: Commit**

```bash
git add crates/discovery/tests/stranger_beacon_during_active.rs
git commit -m "test(discovery): wire-level stranger broadcast does not poison live peer"
```

---

### Task 3: Beacon — stale-timestamp packet outside replay window is dropped

**Why:** `beacon.rs` declares `REPLAY_WINDOW_US = 30_000_000` and rejects packets outside it (line ~127). No test currently exercises this path; if a refactor accidentally removes the check, no other test catches it.

**Files:**
- Modify: `crates/discovery/src/beacon.rs` (extend `mod tests`)

- [ ] **Step 1: Add the failing test**

Append to the `mod tests` block in `crates/discovery/src/beacon.rs`:

```rust
    #[test]
    fn handle_packet_outside_replay_window_dropped() {
        let me = make_identity();
        let them = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();

        // Timestamp far enough in the past that low-32-bit wrap-aware diff
        // exceeds REPLAY_WINDOW_US (30 s). 5 minutes is comfortably outside.
        let stale_us = now_us().saturating_sub(5 * 60 * 1_000_000);
        let pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: stale_us,
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);

        assert_eq!(beacon.current_peer(), None, "stale-timestamp packet must be dropped");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p discovery --lib beacon::tests::handle_packet_outside_replay_window_dropped`
Expected: PASS (characterization). FAIL means the replay-window check has been removed or broken.

- [ ] **Step 3: Commit**

```bash
git add crates/discovery/src/beacon.rs
git commit -m "test(discovery): drop beacon packets outside replay window"
```

---

### Task 4: Attach — full Wi-Fi-blip cycle reattaches cleanly

**Why:** Closes the logic side of D.1 / D.2. Existing tests cover detach (`attached_busid_disappears_drops_to_idle`) and backoff reset (`successful_attach_resets_backoff`), but no single test drives the full `Attached → blip → Idle → reattach` sequence in one timeline. A regression that delays reattach by an extra backoff cycle would slip past the existing tests.

**Files:**
- Modify: `crates/client-win/src/attach.rs` (extend `mod tests`)

- [ ] **Step 1: Add the failing test**

Append to the `mod tests` block in `crates/client-win/src/attach.rs`, after `successful_attach_resets_backoff`:

```rust
    #[test]
    fn detach_then_reattach_within_one_backoff_cycle() {
        let mut a = Attach::new(Duration::from_secs(1), Duration::from_secs(30));
        let mut d = MockDriver {
            busid_for: Some("3-3".into()),
            attach_succeeds: true,
            ported: vec!["3-3".into()],
            ..Default::default()
        };
        let t0 = now();

        // Initial attach.
        let action = a.tick(true, Some("h"), t0, &mut d);
        assert_eq!(action, Some(Action::Attach { host: "h".into(), busid: "3-3".into() }));
        assert_eq!(a.state(), State::Attached);

        // Wi-Fi blip: kernel drops the busid from the port list, peer still seen.
        d.ported.clear();
        let action = a.tick(true, Some("h"), t0 + Duration::from_secs(1), &mut d);
        assert_eq!(action, Some(Action::LostAttachment));
        assert_eq!(a.state(), State::Idle);

        // Wi-Fi recovers next tick. Backoff should be `base` (1 s) because the
        // last attach was a SUCCESS — failures didn't bump it. So a tick at
        // t+2 s must fire a new attach.
        d.ported = vec!["3-3".into()];
        let action = a.tick(true, Some("h"), t0 + Duration::from_secs(2), &mut d);
        assert_eq!(action, Some(Action::Attach { host: "h".into(), busid: "3-3".into() }));
        assert_eq!(a.state(), State::Attached);
        assert_eq!(d.attach_calls.len(), 2, "blip should produce exactly 2 attach calls");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p client-win --lib attach::tests::detach_then_reattach_within_one_backoff_cycle`
Expected: PASS. If FAIL: examine whether `bump_backoff` was incorrectly invoked on the detach path.

- [ ] **Step 3: Run full attach suite**

Run: `cargo test -p client-win --lib attach::`
Expected: 8 passed (7 existing + 1 new), 0 failed.

- [ ] **Step 4: Commit**

```bash
git add crates/client-win/src/attach.rs
git commit -m "test(client-win): full detach-then-reattach cycle"
```

---

### Task 5: Beacon — peer IP change updates the live slot

**Why:** Closes the logic side of D.5. The existing tests assert that a packet from a trusted peer sets the live slot, but none assert that a *second* packet from the *same* trusted peer arriving from a different IP overwrites the slot. The corresponding production code change was committed in `1442e5f` ("refresh peer-lock when peer IP changes") and must not regress.

**Files:**
- Modify: `crates/discovery/src/beacon.rs` (extend `mod tests`)

- [ ] **Step 1: Add the failing test**

Append to the `mod tests` block in `crates/discovery/src/beacon.rs`:

```rust
    #[test]
    fn peer_ip_change_updates_live_addr() {
        let me = make_identity();
        let them = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();

        // First beacon from 192.168.1.42.
        let pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);
        assert_eq!(beacon.current_peer().unwrap().ip().to_string(), "192.168.1.42");

        // Same peer, fresh timestamp, new IP (DHCP renew).
        let pkt2 = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf2 = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt2, &mut buf2).unwrap();
        beacon.handle_packet("192.168.1.99:55555".parse().unwrap(), &buf2);

        let live = beacon.current_peer().unwrap();
        assert_eq!(live.ip().to_string(), "192.168.1.99", "live IP must follow peer's new IP");
        assert_eq!(live.port(), 49152, "port must stay normalized to listen port");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p discovery --lib beacon::tests::peer_ip_change_updates_live_addr`
Expected: PASS. FAIL means the slot is being preserved across IP changes — that's exactly the regression `1442e5f` fixed.

- [ ] **Step 3: Commit**

```bash
git add crates/discovery/src/beacon.rs
git commit -m "test(discovery): peer IP change refreshes live address"
```

---

### Task 6: Daemon — extract `bind_error` threshold into a tested helper

**Why:** Closes D.6 logic. Today the threshold rule lives inline in `daemon.rs:198-205`:

```rust
let bind_failures = conn.consecutive_bind_failures();
let bind_error = if bind_failures >= 3 {
    Some(format!("usbip bind failed {bind_failures} times — is the usbip-host module loaded?"))
} else {
    None
};
```

This is logic, not glue, but it sits in the daemon hot loop where it can't be unit-tested without standing up the daemon. Extracting it into a tiny pure helper makes the rule explicit and testable. The user-visible message must match exactly so the kiosk's existing snapshot expectation in `app.rs::bind_error_propagates_into_view` remains valid.

**Files:**
- Create: `crates/network-deck/src/bind_error.rs`
- Modify: `crates/network-deck/src/main.rs` (declare module)
- Modify: `crates/network-deck/src/daemon.rs` (use helper)

- [ ] **Step 1: Write the failing test (in the new module)**

Create `crates/network-deck/src/bind_error.rs`:

```rust
//! Pure helper: turn a count of consecutive `usbip bind` failures into the
//! optional message the daemon writes into `Status::bind_error`. Threshold
//! is 3 — one or two transient failures stay silent so the kiosk doesn't
//! flicker, but a sustained failure surfaces the diagnostic.

#[must_use]
pub fn from_failure_count(consecutive_failures: u32) -> Option<String> {
    if consecutive_failures >= 3 {
        Some(format!(
            "usbip bind failed {consecutive_failures} times — is the usbip-host module loaded?"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_threshold_yields_none() {
        assert_eq!(from_failure_count(0), None);
        assert_eq!(from_failure_count(1), None);
        assert_eq!(from_failure_count(2), None);
    }

    #[test]
    fn at_threshold_yields_message() {
        let msg = from_failure_count(3).expect("threshold = 3 should yield message");
        assert!(msg.starts_with("usbip bind failed 3 times"));
        assert!(msg.contains("usbip-host module loaded"));
    }

    #[test]
    fn above_threshold_uses_actual_count() {
        let msg = from_failure_count(7).expect("count above threshold should yield message");
        assert!(msg.starts_with("usbip bind failed 7 times"), "got: {msg}");
    }
}
```

- [ ] **Step 2: Declare the new module**

Open `crates/network-deck/src/main.rs`. The module declarations split into two groups: always-compiled modules (with `#[cfg_attr(not(target_os = "linux"), allow(dead_code))]`) and Linux-only modules (with `#[cfg(target_os = "linux")]`). The new `bind_error` module is pure logic with no platform deps but is only *used* by `daemon` (Linux-only), so it joins the always-compiled group with the same `cfg_attr` so its tests run on every platform but it stays quiet on non-Linux. Add it alphabetically:

```rust
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod bind_error;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod connection;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod control;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod sysfs;
```

- [ ] **Step 3: Verify the new module builds + the new tests pass**

Run: `cargo test -p network-deck --lib bind_error::`
Expected: 3 passed, 0 failed.

- [ ] **Step 4: Refactor daemon.rs to use the helper**

Open `crates/network-deck/src/daemon.rs`. Find the block at lines ~195–205 that reads:

```rust
        // Threshold (3) keeps a single transient failure from spamming the
        // kiosk; once we've retried this many times the user almost
        // certainly needs to do something (load usbip-host, fix perms).
        let bind_failures = conn.consecutive_bind_failures();
        let bind_error = if bind_failures >= 3 {
            Some(format!(
                "usbip bind failed {bind_failures} times — is the usbip-host module loaded?"
            ))
        } else {
            None
        };
```

Replace with:

```rust
        let bind_error = crate::bind_error::from_failure_count(conn.consecutive_bind_failures());
```

The threshold-rationale comment is now redundant with the doc-comment on the helper module — drop it.

- [ ] **Step 5: Run the daemon-side compile + test**

Run: `cargo test -p network-deck --lib`
Expected: all existing tests still pass (including `app::bind_error_propagates_into_view`, which depends on the message string format remaining unchanged).

- [ ] **Step 6: Run the workspace test suite**

Run: `cargo make test`
Expected: every test in every crate passes.

- [ ] **Step 7: Run clippy on the touched crate**

Run: `cargo clippy -p network-deck --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/network-deck/src/bind_error.rs crates/network-deck/src/main.rs crates/network-deck/src/daemon.rs
git commit -m "refactor(deck): extract bind_error threshold rule into tested helper"
```

---

## Final verification

After all six tasks, run the full workspace gate one more time before declaring the plan done:

- [ ] `cargo make test` — all crates green.
- [ ] `cargo make clippy` — no warnings on any crate.
- [ ] `cargo make fmt-check` — workspace formatted.
- [ ] `git log --oneline` — six new commits, one per task, in plan order.

Update the test plan spec's appendix to mark the closed gaps:

- [ ] In `docs/superpowers/specs/2026-05-09-pre-release-test-plan.md` appendix, annotate "Stranger Deck rejection (B.2)" with a closing note pointing at `crates/discovery/tests/stranger_beacon_during_active.rs` and the new beacon unit tests.
- [ ] Commit:
```bash
git add docs/superpowers/specs/2026-05-09-pre-release-test-plan.md
git commit -m "docs(spec): note B.2/D.5/D.6 gaps closed by automated tests"
```
