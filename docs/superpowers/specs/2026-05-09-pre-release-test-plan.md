# Pre-release test plan — network-usb

Manual checklist run before tagging a release. Walk it top to bottom; do
not skip phases — later phases assume earlier ones passed. Mark each
scenario `[ ] PASS / [ ] FAIL / [ ] SKIP (reason)`. A FAIL in any phase
is a ship-blocker unless explicitly waived in the sign-off section.

## Scope

Covers the two shipped binaries (`network-deck` on Linux, `client-win` on
Windows) and the one shared crate (`discovery`). Verifies the user
journey end-to-end on real hardware: install → pair → play → recover →
uninstall. Excludes hostile-LAN and multi-Deck scenarios per
`ARCHITECTURE.md` non-goals; those are documented as gaps in the appendix.

## Hardware and pre-conditions

Required for this run:

- 1× Steam Deck running stock SteamOS, with developer mode enabled and
  passwd set (Konsole works, sudo prompts).
- 1× Windows 10 or 11 PC with admin rights and Steam installed.
- 1× Wi-Fi network reachable from both machines (5 GHz strongly
  preferred — 2.4 GHz Wi-Fi blip behaviour is documented but won't be
  certified by this run).
- Both machines off any VPN, on the same broadcast domain (UDP 49152
  must traverse).
- Latest workspace built locally:
  - `cargo make build-deck` (release artifact under `target/release/`).
  - `cargo make build-win`  (release artifact under `target/release/`).
- Clean state on both sides (see Phase 0 below).

## Evidence to capture for each phase

For every scenario, capture enough so a failed run is reproducible:

- Deck: `journalctl --user -u <whatever> -b` is N/A (no systemd unit) —
  instead, run the GUI from a terminal so stderr is visible, and copy
  `$XDG_RUNTIME_DIR/network-deck/status.json` at the moment of interest.
- Windows: tray app stderr (run from `cmd.exe` in dev runs), tray menu
  text, `usbip.exe port` output.
- Both: git rev (`git rev-parse --short HEAD`), `usbip --version` (Linux),
  `usbip.exe --version` (Win), Deck OS build number, Win build number.

# Phase 0 — Pre-conditions

Goal: both sides start from a known-clean slate so install scenarios
exercise first-run paths, not upgrade paths.

- **0.1 Clean Deck**
  - [ ] `sudo rm -rf /var/lib/network-deck /etc/sudoers.d/network-deck /etc/modules-load.d/usbip.conf`
  - [ ] `rm -rf ~/.local/share/applications/network-deck-kiosk.desktop ~/.local/share/network-deck ~/.config/network-deck`
  - [ ] `sudo systemctl disable --now network-deck-server.service` (no-op if absent — verifies idempotency)
  - [ ] Reboot.

- **0.2 Clean Windows**
  - [ ] Uninstall any prior `usbip-win2` via "Apps & features".
  - [ ] Delete `%LOCALAPPDATA%\network-deck` (config + autostart marker).
  - [ ] Confirm no `client-win.exe` in `Run` registry key.
  - [ ] Reboot.

- **0.3 Versions captured**
  - [ ] git rev, Deck OS build, Win build, `usbip.exe` version field
        recorded in the sign-off section.

# Phase A — Install / first-run

Goal: a user with no prior state can get to a runnable system on each
side without reading docs (beyond the README quickstart).

- **A.1 Deck `network-deck install`**
  - [ ] `sudo /path/to/network-deck install` exits 0.
  - [ ] `/etc/sudoers.d/network-deck` exists, mode 0440, owned root:root.
  - [ ] `/var/lib/network-deck/network-deck` exists, mode 0755, owned
        root:root (NOT setuid).
  - [ ] `/etc/modules-load.d/usbip.conf` lists `usbip-core`,
        `usbip-host`, `vhci-hcd`.
  - [ ] `systemctl is-enabled usbipd.service` → `enabled`.
  - [ ] `~/.local/share/applications/network-deck-kiosk.desktop` exists
        and `Exec=` points at `/var/lib/network-deck/network-deck`.

- **A.2 Deck GUI launch from .desktop**
  - [ ] Tap the Network Deck icon in Game Mode (or run via Dolphin in
        Desktop Mode).
  - [ ] Window opens fullscreen, shows "Not paired" or pair prompt.
  - [ ] No password prompt (sudoers NOPASSWD path is wired).
  - [ ] Closing the window leaves no orphaned daemon process
        (`pgrep -af 'network-deck daemon'` empty).

- **A.3 Deck install idempotency**
  - [ ] Run `sudo network-deck install` a second time.
  - [ ] Exits 0; no duplicate sudoers entry; no duplicate `.desktop`;
        no duplicate `modules-load.d` lines.

- **A.4 Windows first launch self-install**
  - [ ] Run `client-win.exe`. Egui dialog "install required" appears.
  - [ ] Click Install. UAC prompt appears (driver signature dialog
        also appears — accept it).
  - [ ] Spinner advances through Download → Wait UAC → Run installer.
  - [ ] Tray icon appears after installer exits.
  - [ ] `usbip.exe` is on PATH or locatable by `usbip_cli::locate`.

- **A.5 Windows re-launch after install**
  - [ ] Quit tray app via menu → Quit.
  - [ ] Re-launch `client-win.exe`. No install dialog. Tray appears
        within 2 s.

- **A.6 Tray "Start at login" toggle**
  - [ ] Toggle ON. Reboot. Tray app appears in tray after login with
        no manual launch.
  - [ ] Toggle OFF. Reboot. Tray app does NOT appear.

# Phase B — Pair flow

Goal: the bilateral mutual-confirm pair flow works happy-path and
handles the explicit failure paths it advertises (timeout, cancel,
re-pair).

- **B.1 Pair happy path**
  - [ ] On Deck: tap Pair (or `network-deck pair` from terminal).
  - [ ] On Windows: tray → Pair.
  - [ ] Each side displays the other's fingerprint within 5 s.
  - [ ] Confirm on Deck, then on Windows. Both sides flip to "Paired".
  - [ ] Deck `~/.config/network-deck/trusted-peers.toml` written.
  - [ ] Win `%LOCALAPPDATA%\network-deck\trusted-peers.toml` written.
  - [ ] Fingerprints in both files match.

- **B.2 Stranger rejection** — DEFERRED (single Deck only; see appendix).

- **B.3 120 s timeout**
  - [ ] Start pair on Deck only; do not initiate on Windows.
  - [ ] After 120 s, Deck pair UI exits cleanly with a "no peer" message;
        no trust file written.

- **B.4 Pair cancel**
  - [ ] Start pair on both sides; cancel on Deck before confirming.
  - [ ] Windows pair UI returns to idle, no trust file on either side.
  - [ ] Repeat with cancel originating on Windows.

- **B.5 Re-pair after trust deletion**
  - [ ] Delete `trusted-peers.toml` on Deck only.
  - [ ] Steady-state stops working (peer-present false on Deck).
  - [ ] Run pair on both sides; trust restored; bind resumes.

# Phase C — Steady-state play

Goal: the controller behaves like a wired Steam Deck controller while
the user plays a game.

- **C.1 Bind reaches steady state**
  - [ ] Both binaries running, paired.
  - [ ] Deck `status.json` shows `peer_present=true`, `bound=true`,
        `paused=false`, `bind_error=null`.
  - [ ] Win tray tooltip / status reads "Ready for `<peer_name>`"
        (per commit 91bca7d).

- **C.2 Steam recognises Deck controller**
  - [ ] On Win: open Steam → Settings → Controller. A "Steam Deck
        Controller" entry appears.
  - [ ] Big Picture controller test view: button presses register;
        analog sticks track; triggers register full range.

- **C.3 Full input surface**
  Use `Test Devices` view (or any controller-tester). Verify each
  control reports a state change:
  - [ ] A / B / X / Y
  - [ ] D-pad up/down/left/right
  - [ ] L1 / R1 / L2 / R2 (full analog)
  - [ ] L3 / R3 (stick clicks)
  - [ ] L4 / R4 / L5 / R5 (back grip buttons)
  - [ ] Steam button, Quick Access (…) button, Start, Select
  - [ ] Left + right thumbsticks (X and Y)
  - [ ] Left + right trackpads (X, Y, click, touch)
  - [ ] Gyro (yaw, pitch, roll)
  - [ ] Capacitive thumbstick touch — known unknown per memory
        `deck_hidraw_spike_validated`; record observation but do not
        fail the run on this control alone.

- **C.4 Rumble round-trip**
  - [ ] Launch a known-rumble title (e.g., a Steam Input config with
        rumble-on-test, or any title that vibrates on damage).
  - [ ] Rumble fires on the Deck speakers/haptics, not just on a
        ghost virtual device.

- **C.5 Pause / resume controls**
  - [ ] Tap pause on the Deck touchscreen → kiosk shows paused;
        `paused` flag file present; `bound` flips false within ~1 s;
        Steam shows controller disconnect.
  - [ ] Tap resume → flag removed, `bound` true within ~2 s, Steam
        re-acquires controller.
  - [ ] Vol+/Vol- chord hotkey toggles pause from Game Mode (commit
        1328564 surfaces this hint). Cooldown shared across listeners
        (commit 3f1fa7e) — rapid-fire chord does not cause flapping.

- **C.6 1-hour soak**
  - [ ] Leave a game running with the controller bound for 60 min.
  - [ ] At 15 / 30 / 60 min checkpoints: `status.json` shape unchanged,
        no bind errors, tray status unchanged, no rumble queue stalls,
        no input lag growth.
  - [ ] No daemon RSS growth >50 MB over the hour.

- **C.7 Windows sleep/resume**
  - [ ] Suspend Windows for ~60 s, resume.
  - [ ] vhci re-establishes; tray shows reconnect within ~10 s; Steam
        re-acquires controller (may need game-restart for some titles —
        document if so).

- **C.8 Deck sleep/resume**
  - [ ] Press Deck power button to sleep; wait 30 s; wake.
  - [ ] Daemon re-binds (`status.json` shows `bound=true` again);
        Win tray reconnects.

# Phase D — Failure / recovery

Goal: the documented failure modes behave as advertised, and no failure
leaves the system in a state that needs a reboot to recover.

- **D.1 Wi-Fi blip <5 s**
  - [ ] While playing, disable+re-enable the Win Wi-Fi adapter inside
        5 s.
  - [ ] Best case: TCP recovers, game retains controller.
  - [ ] Acceptable case: brief disconnect, auto-reattach, game resumes
        on next input.
  - [ ] Unacceptable: tray stuck in "attaching", bind/attach loop —
        FAIL.

- **D.2 Wi-Fi blip >30 s**
  - [ ] Disable Win Wi-Fi adapter 30+ s, re-enable.
  - [ ] Tray surfaces "peer lost" / "reconnecting".
  - [ ] On reconnect: clean reattach within ~10 s. Game may need
        restart per `ARCHITECTURE.md` "Open risks" — document which
        titles need this.

- **D.3 Daemon SIGKILL**
  - [ ] `sudo pkill -9 -f 'network-deck daemon'`.
  - [ ] Kiosk surfaces "daemon stopped" or similar.
  - [ ] Re-launching the GUI restarts the daemon; no leftover bind
        on `usbip list -l`.

- **D.4 GUI close mid-game**
  - [ ] With a game running and controller bound, close the kiosk
        window from Game Mode.
  - [ ] Daemon receives SIGTERM, unbinds before exit (commit 13e6301
        non-blocking shutdown — progress visible).
  - [ ] `usbip list -l` shows controller no longer bound.
  - [ ] Win tray surfaces detach.

- **D.5 Peer IP change**
  - [ ] Force a DHCP renew on the Win box (`ipconfig /release` then
        `/renew`) so its LAN IP changes.
  - [ ] Within ~2 beacons (≈2 s) the Deck peer-lock refreshes (commit
        1442e5f); reattach completes without re-pair.

- **D.6 Repeat bind failure**
  - [ ] Synthetic: `sudo systemctl stop usbipd.service` while bound.
  - [ ] Daemon retries; after the retry budget, `bind_error` field on
        `status.json` populates and kiosk renders the diagnostic
        (commit ef978ab).
  - [ ] Restart `usbipd`; bind recovers without daemon restart.

- **D.7 SYN_DROPPED hotkey resync**
  - [ ] Synthetic stress: hold Vol+/Vol- combos rapidly while running
        another input-heavy app to encourage dropped sync.
  - [ ] After any drop, hotkey state re-reads (commit 38f66cd) — pause
        toggle remains responsive, no stuck modifier.

- **D.8 Recv-thread persistent error**
  - [ ] Block UDP 49152 inbound on the Deck firewall for 30 s.
  - [ ] Daemon recv thread sleeps and observes term flag (commit
        8141550) — no busy loop, no CPU spike.
  - [ ] Unblock; beacon recovers.

# Phase E — Uninstall / cleanup

Goal: a user can fully revert without leaving privileged sudoers entries
or driver state behind.

- **E.1 Deck uninstall**
  - [ ] Run uninstall path (per commit 663d49d "diagnostics panel,
        uninstall, ALL-user sudoers"; in GUI Diagnostics → Uninstall).
  - [ ] `/etc/sudoers.d/network-deck` removed.
  - [ ] `/var/lib/network-deck/` removed.
  - [ ] `~/.local/share/applications/network-deck-kiosk.desktop` removed.
  - [ ] `usbipd.service` left in user-chosen state (document expected
        behaviour; do not silently disable).

- **E.2 Windows uninstall**
  - [ ] Tray menu → Quit, then uninstall via "Apps & features"
        (or whatever path the README documents).
  - [ ] Tray icon does not reappear after reboot.
  - [ ] Autostart `Run` registry entry absent.
  - [ ] usbip-win2 uninstall is OPTIONAL (third-party); document the
        expectation but do not require it.

# Appendix — Documented gaps (single-hardware constraints)

These scenarios are not run because the lab in this pass has only one
Deck and one Win PC. Each is a known coverage gap, not a tested pass:

- **Stranger Deck rejection (B.2)** — Requires a second Deck (or a
  fabricated mismatched-pubkey beacon). **Closed (logic level)** by
  automated regression tests landed 2026-05-09: see `beacon::tests::
  stranger_does_not_overwrite_active_peer` in `crates/discovery/src/
  beacon.rs` (in-process) and the wire-level integration test
  `crates/discovery/tests/stranger_beacon_during_active.rs`. A real
  second-Deck run remains a follow-up but the rejection path has
  hardware-free coverage.

- **Peer IP change behaviour (D.5)** — **Closed (logic level)** by
  `beacon::tests::peer_ip_change_updates_live_addr` in
  `crates/discovery/src/beacon.rs`. Hardware test of a real DHCP renew
  mid-session remains a follow-up but the bind-refresh path is now
  pinned by a regression test.

- **Repeat bind failure surfacing (D.6)** — **Closed (logic level)** by
  the extracted `bind_error::from_failure_count` helper and its three
  tests in `crates/network-deck/src/bind_error.rs`. Hardware test of the
  threshold being reached during a live `usbipd` outage remains a
  follow-up.

- **Wi-Fi blip recovery (D.1 / D.2)** — **Closed (logic level)** for the
  Windows attach state machine by `attach::tests::detach_then_reattach_
  within_one_backoff_cycle` in `crates/client-win/src/attach.rs`. Real
  Wi-Fi-blip-on-the-wire timing remains a hardware-only check.

- **Replay-window defense** — **Closed** by `beacon::tests::handle_
  packet_outside_replay_window_dropped` in `crates/discovery/src/
  beacon.rs`. Not a spec scenario but called out in the implementation
  plan as defense-in-depth.
- **Multi-LAN segment** — UDP broadcast scope is not exercised across
  routed segments.
- **Multi-hour soak (>1 h)** — Phase C.6 caps at 60 min. A 4–8 h run is
  optional follow-up if time allows.
- **2.4 GHz Wi-Fi blip behaviour** — Phase D requires 5 GHz; 2.4 GHz
  is documented as worse but not certified by this run.
- **Game-specific reconnect behaviour** — Phase C.7/D.2 note that some
  games never re-acquire the controller after a USB unplug. Cataloguing
  per-game behaviour is out of scope.

# Sign-off

Fill in at end of run.

| Field | Value |
| --- | --- |
| Tester | |
| Date | |
| Git rev (network-usb) | |
| Deck OS build | |
| Win build | |
| `usbip-win2` version | |
| `usbip` (Linux) version | |

Phase results:

| Phase | Pass | Fail | Skip |
| --- | --- | --- | --- |
| 0 — Pre-conditions | | | |
| A — Install | | | |
| B — Pair | | | |
| C — Play | | | |
| D — Failure / recovery | | | |
| E — Uninstall | | | |

Blockers (each must be cleared or explicitly waived before tagging):

- 

Waived issues (issue link + rationale):

- 

Verdict: **[ ] SHIP / [ ] HOLD**.
