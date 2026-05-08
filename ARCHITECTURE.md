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

- **2026-05-08** — Added a Deck-side kiosk UI (`crates/kiosk-deck`, `eframe`/
  `egui` fullscreen app) so the user can pause/resume controller sharing
  from the touchscreen while a Windows game is using the controller. The
  daemon and kiosk talk through a shared dir `/run/network-deck/` (mode
  0777, single-user device): daemon writes `status.json` each tick,
  kiosk creates/removes a `paused` flag file. Pause is implemented as
  "treat peer as absent" in the daemon's tick — the bind/unbind state
  machine is unchanged. Steam library entry is added once via the
  desktop client (no `shortcuts.vdf` munging).
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
|   - publishes Status + reads paused flag             |
|         via /run/network-deck/                       |
|                                                      |
| kiosk-deck (Rust, eframe/egui) — fullscreen touch UI |
|   - reads /run/network-deck/status.json (4 Hz)       |
|   - creates/removes /run/network-deck/paused on tap  |
|   - launched via Steam library "Add Non-Steam Game"  |
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
  beacon state. Publishes daemon view via `/run/network-deck/status.json`
  and respects `/run/network-deck/paused`.
- `crates/kiosk-deck/` — Linux GUI binary (`eframe`/`egui`). Fullscreen
  touch-screen app for pausing/resuming controller sharing. Reads
  `status.json`; toggles the `paused` flag on tap. Launched via Steam
  library as a non-Steam game.
- `crates/client-win/` — Windows binary. Tray app that drives
  `usbip.exe attach` based on beacon state.
- `scripts/install-deck.sh` — Deck-side installer (pacman + systemd).
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
- **Deck-local IPC (`/run/network-deck/`, mode 0777):** the daemon is
  the sole writer of `status.json` (atomic tmp+rename, ~2 Hz) — fields
  `peer_name`, `peer_present` (raw beacon presence), `bound`, `paused`.
  The kiosk is the sole writer of the `paused` flag file (touch =
  paused, remove = resumed). The JSON shape is the contract; the struct
  is intentionally duplicated across `server-deck` and `kiosk-deck` so
  there is no shared library crate solely to hold it.

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
5. ✅ Deck-side touchscreen kiosk added for pause/resume control (plan
   `docs/superpowers/plans/2026-05-08-deck-kiosk-ui.md`).
