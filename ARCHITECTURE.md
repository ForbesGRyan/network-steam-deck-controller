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

- **2026-05-08 (afternoon)** — Merged `server-deck` + `kiosk-deck` into a
  single `network-deck` binary with subcommands (`daemon`, `pair`, `install`,
  default = GUI). The GUI supervises the daemon as a child via `sudo -n`,
  so closing the window unbinds and exits — no systemd unit, no leftover
  background process. The shell installer (`bootstrap-deck.sh`/
  `install-deck.sh`) is replaced by `network-deck install`, which writes
  the sudoers entry, drops a `.desktop`, and copies itself into
  `~/network-deck/`. Control dir defaulted to `$XDG_RUNTIME_DIR/network-deck`
  so no tmpfiles entry is needed.
- **2026-05-08** — Added a Deck-side kiosk UI (`eframe`/`egui` maximized
  window) so the user can pause/resume controller sharing from the
  touchscreen while a Windows game is using the controller. The daemon
  and kiosk talk through a shared control dir: daemon writes
  `status.json` each tick, kiosk creates/removes a `paused` flag file.
  Pause is implemented as "treat peer as absent" in the daemon's tick.
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
| network-deck  (single Rust binary, subcommands)      |
|   gui (default):                                     |
|     - eframe/egui maximized touch UI                 |
|     - reads $XDG_RUNTIME_DIR/network-deck/status.json|
|     - creates/removes paused flag on tap             |
|     - spawns `sudo -n network-deck daemon` child;    |
|       SIGTERM + reaps it on window close             |
|   daemon:                                            |
|     - load identity + paired-peer trust              |
|     - sysfs lookup of Deck controller busid          |
|     - signed-UDP discovery beacon                    |
|     - bind/unbind state machine (shells out to usbip)|
|     - SIGTERM/SIGINT handler unbinds before exit     |
|   pair:    one-shot pair flow                        |
|   install: first-run bootstrap (pacman, sudoers,     |
|            self-copy to ~/network-deck/, .desktop)   |
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
  pair flow. Shared between Deck and Windows binaries. No I/O beyond
  UDP + filesystem.
- `crates/network-deck/` — Single Linux binary. Subcommands: `gui` (default,
  user) supervises a `daemon` child (root) via sudo NOPASSWD; `pair` for
  one-shot pairing; `install` for the first-run bootstrap. Closing the
  GUI window stops the daemon and unbinds the controller — no systemd
  unit involved.
- `crates/client-win/` — Windows binary. Tray app that drives
  `usbip.exe attach` based on beacon state.
- `scripts/install-windows.ps1` — Windows-side installer (usbip-win2 +
  binary drop).

## Wire protocol

Two network channels plus one local-IPC contract:

- **Discovery (UDP 49152, broadcast):** signed beacon every 1 s; magic
  `NDB1`. Receiver verifies Ed25519 signature against the trusted-peer
  pubkey, normalizes the source port to 49152, and exposes the live peer
  address to the data plane.
- **USB/IP (TCP 3240):** stock Linux `usbipd` ↔ `usbip-win2` vhci. We
  don't speak this directly; we just drive the lifecycle.
- **Deck-local IPC (`$XDG_RUNTIME_DIR/network-deck/`):** the daemon is
  the sole writer of `status.json` (atomic tmp+rename, ~2 Hz) — fields
  `peer_name`, `peer_present` (raw beacon presence), `bound`, `paused`.
  The GUI is the sole writer of the `paused` flag file (touch = paused,
  remove = resumed). The JSON shape is the contract; both sides live in
  the same crate now and share a single `control.rs` module.

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
5. ✅ Deck-side touchscreen kiosk added for pause/resume control (later
   merged into the unified `network-deck` binary; plan
   `docs/superpowers/plans/2026-05-08-deck-kiosk-ui.md`).
