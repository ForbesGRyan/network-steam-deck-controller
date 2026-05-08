# Deck Kiosk UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a touchscreen-driven Disconnect/Reconnect UI on the Deck side. User pairs once; thereafter Steam ↔ Deck controller hand-off is automatic, but can be paused at any time by tapping a button on a kiosk window launched from the Steam library.

**Why now:** With the usbip backend live, the Deck's controller is bound to Windows whenever a paired peer is present. The Deck's local controller goes dead during this window — but the touchscreen still works. There is no current way to pause sharing without SSHing into the Deck. This plan fills that gap.

**Architecture:** A new Linux-only crate `kiosk-deck` builds an `eframe` (egui) fullscreen app with two pieces: a status string ("Connected to <host>" / "Searching for client" / "Paused") and a single big toggle button. Kiosk and `server-deck` daemon talk through a shared directory `/run/network-deck/`:

- Daemon writes `status.json` once per tick (peer name, peer present, bound).
- Kiosk reads `status.json` to render.
- Kiosk creates/removes a `paused` flag file to toggle.
- Daemon ANDs `!paused` into the `peer_present` signal driving `Connection::tick`, so flipping the flag triggers an unbind on the next tick.

No socket, no D-Bus, no privileged IPC. The state machine itself is unchanged — pause is implemented as "peer is treated as absent."

**Tech stack:** Rust 2021. New deps: `eframe = "0.32"`, `serde = "1"`, `serde_json = "1"`. Kiosk launched as a non-Steam game shortcut from Steam library so it shows in Game Mode; touchscreen taps drive the toggle.

**Spec:** Conversation 2026-05-08. User picked: toggle + status only (no pair UI), file-flag IPC, Steam non-Steam game shortcut as launch surface.

---

## File Structure

**Create:**

- `crates/kiosk-deck/Cargo.toml` — new crate manifest.
- `crates/kiosk-deck/src/main.rs` — `eframe` entry point.
- `crates/kiosk-deck/src/app.rs` — egui app: render + tick loop.
- `crates/kiosk-deck/src/control.rs` — read `status.json`, toggle `paused` flag. Pure `std`, easily unit-tested with `tempfile`.
- `crates/server-deck/src/control.rs` — write `status.json`, read `paused` flag. Same shape as kiosk's `control.rs` but inverted (writer side).
- `scripts/network-deck.tmpfiles` — systemd-tmpfiles entry to create `/run/network-deck/` mode 0777 at boot.
- `scripts/network-deck-kiosk.desktop` — desktop entry pointing at `/usr/local/bin/network-deck-kiosk`.

**Modify:**

- `Cargo.toml` (workspace) — add `crates/kiosk-deck` member.
- `crates/server-deck/Cargo.toml` — add `serde`, `serde_json`.
- `crates/server-deck/src/main.rs` — wire status writer + pause reader into the run loop.
- `scripts/install-deck.sh` — install kiosk binary, `.desktop` file, tmpfiles config; print "Add to Steam" step.
- `README.md` — short note on launching the kiosk.

**Delete:** none.

---

## Task 1: Shared control surface (status + pause flag)

**Files:**

- Create: `crates/server-deck/src/control.rs`

Define the on-disk contract. One file: `crates/server-deck/src/control.rs`. The kiosk re-uses the same struct via a path-only dependency on `server-deck` is wrong (server-deck is a binary crate); instead, define `Status` here and re-derive an identical struct in `crates/kiosk-deck/src/control.rs` with the same JSON shape. The serialized form is the contract — both sides agree on field names. This keeps the dep graph clean (no shared library crate just for one struct).

Status struct:

```rust
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Status {
    pub peer_name: Option<String>,   // None when no peer ever paired
    pub peer_present: bool,           // beacon fresh
    pub bound: bool,                  // currently bound to usbip-host
    pub paused: bool,                 // pause flag observed
}
```

Functions:

- `pub fn write_status(dir: &Path, s: &Status) -> io::Result<()>` — atomic write: write to `status.json.tmp`, rename to `status.json`. Mode 0644.
- `pub fn paused_flag_path(dir: &Path) -> PathBuf` — returns `dir.join("paused")`.
- `pub fn is_paused(dir: &Path) -> bool` — `paused_flag_path(dir).exists()`. Errors → false (best-effort, daemon would rather mistakenly bind than mistakenly unbind).

- [ ] **Step 1: Define `Status` + functions in `crates/server-deck/src/control.rs`.**
- [ ] **Step 2: Add `serde = { version = "1", features = ["derive"] }` and `serde_json = "1"` to `crates/server-deck/Cargo.toml`.**
- [ ] **Step 3: Inline tests:**
  - `write_status` then deserialize the file back; struct round-trips.
  - `is_paused` returns true when flag file exists, false when absent.
  - Atomic write: `status.json.tmp` does not exist after `write_status` returns successfully.

**Done when:** `cargo test -p server-deck control::` passes.

---

## Task 2: Wire control surface into server-deck main loop

**Files:**

- Modify: `crates/server-deck/src/main.rs`

Hook the new `control` module into `linux::run`:

1. Add `mod control;` at crate root.
2. Resolve a control directory: `/run/network-deck/`. Add CLI override `--control-dir <path>` for testing.
3. After `find_deck_busid` succeeds, on each loop iteration:
   - Read `paused` flag.
   - Compute effective `peer_present = beacon_present && !paused`.
   - Pass effective value to `Connection::tick`.
   - Write `Status { peer_name: trusted.name.clone().into(), peer_present: beacon_present, bound: matches!(conn.state(), State::Bound), paused }`.

Notes:
- `peer_present` reported in `Status` is the **raw** beacon value (so the UI can show "Searching" vs "Connected but paused").
- `bound` is the daemon's reality, not desire — derived from `Connection::state()`.

- [ ] **Step 1: Add `mod control;` + parse `--control-dir` arg (default `/run/network-deck`).**
- [ ] **Step 2: Per-tick: read pause, compute effective peer_present, tick connection, write status.**
- [ ] **Step 3: Manual sanity (without kiosk): `mkdir /tmp/ndc && server-deck --control-dir /tmp/ndc` — verify `cat /tmp/ndc/status.json` updates each tick; `touch /tmp/ndc/paused` triggers an unbind in the journal; `rm /tmp/ndc/paused` triggers a rebind.**

**Done when:** Manual sanity passes, existing tests still green.

---

## Task 3: Kiosk crate scaffold + control reader

**Files:**

- Create: `crates/kiosk-deck/Cargo.toml`
- Create: `crates/kiosk-deck/src/main.rs`
- Create: `crates/kiosk-deck/src/control.rs`
- Modify: `Cargo.toml` (workspace) — add member.

`Cargo.toml`:

```toml
[package]
name = "kiosk-deck"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Steam Deck side: touch-screen kiosk for pausing controller sharing"

[dependencies]
eframe = { version = "0.32", default-features = false, features = ["default_fonts", "glow", "wayland", "x11"] }
egui = "0.32"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[lints]
workspace = true
```

`control.rs` — mirror of server-deck's struct + readers:

- `pub struct Status { ... }` — identical fields to server-deck's.
- `pub fn read_status(dir: &Path) -> Option<Status>` — open + parse, None on any error (file missing, partial write, parse error). UI treats None as "Searching" (see Task 4).
- `pub fn set_paused(dir: &Path, paused: bool) -> io::Result<()>` — touch or remove `paused` flag file.

`main.rs` — minimal `eframe` boot stub that just opens a window with a label. Wired to actual app in Task 4.

- [ ] **Step 1: Add crate to workspace, write `Cargo.toml`.**
- [ ] **Step 2: Implement `control.rs` with `read_status` + `set_paused`.**
- [ ] **Step 3: Inline tests using `tempfile`:**
  - `read_status` returns `None` on missing file.
  - `read_status` returns `None` on garbage JSON.
  - `read_status` returns `Some(Status)` matching what server-deck's `write_status` produces (compose with `server-deck` test by hand-rolling JSON, since we don't share the type).
  - `set_paused(true)` creates flag; `set_paused(false)` removes it; idempotent both ways.
- [ ] **Step 4: Stub `main.rs`: window opens, `cargo run -p kiosk-deck` doesn't panic on a workstation (Linux desktop) — verifies eframe builds. (Won't actually run on Windows dev box; build-only check.)**

**Done when:** `cargo build -p kiosk-deck` succeeds; tests pass.

---

## Task 4: Kiosk app — render + tick

**Files:**

- Create: `crates/kiosk-deck/src/app.rs`
- Modify: `crates/kiosk-deck/src/main.rs`

App flow:
1. On each `update`, call `read_status` from `/run/network-deck/`.
2. Render a top-level vertical panel: large status text + one big button.
3. Map state to UI:

| `read_status` result                            | Status text                  | Button label   | Button action               |
|-------------------------------------------------|------------------------------|----------------|-----------------------------|
| `None` (no daemon / file missing)               | "Daemon not running"         | (disabled)     | —                           |
| `Some { paused: true, .. }`                     | "Paused"                     | "Reconnect"    | `set_paused(false)`         |
| `Some { peer_present: false, .. }`              | "Searching for client…"      | "Pause"        | `set_paused(true)`          |
| `Some { peer_present: true, bound: false, .. }` | "Connecting to <peer_name>…" | "Pause"        | `set_paused(true)`          |
| `Some { peer_present: true, bound: true, .. }`  | "Connected to <peer_name>"   | "Disconnect"   | `set_paused(true)`          |

4. Repaint at 4 Hz (`ctx.request_repaint_after(Duration::from_millis(250))`); the daemon writes status at 2 Hz, so this keeps the UI feeling live without spinning.

5. Window: fullscreen via `NativeOptions { viewport: ViewportBuilder::default().with_fullscreen(true), ..Default::default() }`. Touch input is automatic with egui.

6. Big button: `egui::Button::new(...)` inside a centered layout, sized ~50% of viewport — easy to hit with a thumb.

7. Press handling: ignore button errors silently (best-effort). Log to stderr so journalctl picks it up if launched from a service.

`main.rs`: `eframe::run_native("Network Deck", native_options, Box::new(|_| Ok(Box::new(KioskApp::new()))))`.

- [ ] **Step 1: Implement `KioskApp` with `update` covering all five status branches.**
- [ ] **Step 2: Wire fullscreen viewport + repaint scheduling.**
- [ ] **Step 3: Move stub `main.rs` over to launching the real app.**
- [ ] **Step 4: Screenshot-style sanity: run on a Linux workstation, verify each state by manually creating/editing `/run/network-deck/status.json` while the app is running.**

**Done when:** All five states render correctly when status file is hand-edited; button taps flip the paused flag.

---

## Task 5: Installer + Steam shortcut wiring

**Files:**

- Create: `scripts/network-deck.tmpfiles`
- Create: `scripts/network-deck-kiosk.desktop`
- Modify: `scripts/install-deck.sh`
- Modify: `README.md`

`network-deck.tmpfiles`:

```
d /run/network-deck 0777 root root -
```

Mode 0777 because the daemon runs as root but the kiosk runs as `deck`. Tradeoff: any local user can spoof the paused flag. Acceptable for v0; the Deck is a single-user device. (If we ever change this, switch to a `network-deck` group + setgid.)

`network-deck-kiosk.desktop`:

```
[Desktop Entry]
Type=Application
Name=Network Deck
Comment=Pause/resume Steam Deck controller sharing
Exec=/usr/local/bin/network-deck-kiosk
Icon=input-gaming
Terminal=false
Categories=Utility;
```

`install-deck.sh` additions (after existing pacman/usbip block):

1. Install kiosk binary: `install -m 0755 target/release/network-deck-kiosk /usr/local/bin/`
2. Install tmpfiles: `install -m 0644 scripts/network-deck.tmpfiles /etc/tmpfiles.d/network-deck.conf` then `systemd-tmpfiles --create /etc/tmpfiles.d/network-deck.conf`
3. Install desktop entry: `install -m 0644 scripts/network-deck-kiosk.desktop /usr/share/applications/`
4. Print final instructions:
   ```
   Kiosk installed.
   To add it to Game Mode:
     1. Reboot to Desktop Mode.
     2. Open Steam.
     3. Games → Add a Non-Steam Game to My Library...
     4. Browse to /usr/local/bin/network-deck-kiosk → Add Selected Programs.
     5. Switch back to Game Mode; "Network Deck" appears in your library.
   ```

We don't write into Steam's binary `shortcuts.vdf` — fragile and would need to know the user ID. Document the one-time manual step instead.

`README.md`: under the Install section, add a "Pause/resume from Game Mode" subsection pointing at the manual step above.

- [ ] **Step 1: Write tmpfiles.d entry.**
- [ ] **Step 2: Write `.desktop` entry.**
- [ ] **Step 3: Extend `install-deck.sh` with binary install + tmpfiles + desktop install + final instructions.**
- [ ] **Step 4: Update `README.md` Install section.**
- [ ] **Step 5: Lint check: `bash -n scripts/install-deck.sh`.**

**Done when:** Script lints clean; README explains the one-time Steam-add step.

---

## Task 6: Phase A integration check (manual)

User runs on real hardware:

1. Pull main on Deck, `cargo build --release -p server-deck -p kiosk-deck`.
2. Re-run `sudo bash scripts/install-deck.sh` — verify tmpfiles creates `/run/network-deck/`, kiosk binary lands in `/usr/local/bin/`.
3. Restart `network-deck-server.service`.
4. From another terminal: `cat /run/network-deck/status.json` → should update once per second; `peer_present` flips when `client-win` is on/off.
5. In Desktop Mode: add the kiosk to Steam library per install-deck.sh instructions. Verify it appears.
6. Reboot to Game Mode. Pair Windows side. Launch a game. Once Windows is using the controller:
   - Tap touchscreen to expose the Steam UI.
   - Launch "Network Deck" from library.
   - Verify status reads "Connected to <hostname>".
   - Tap Disconnect. Verify status flips to "Paused" within ~1 s, Steam on Windows reports controller unplug.
   - Tap Reconnect. Verify status flips back to "Connecting…" then "Connected".
7. Edge case: kill `network-deck-server.service`; verify kiosk shows "Daemon not running" within ~1 s.

- [ ] **Manual hardware validation passes.**

**Done when:** All seven manual checks pass.
