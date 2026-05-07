# network-usb

Wireless gamepad bridge: Steam Deck controls → Windows PC, presented as a Steam
Deck controller via a virtual USB device. Steam Input on Windows handles all
gameplay-side mapping (gyro, trackpads, profiles).

## Goal

Run Rust on the Deck. Run Rust + a small KMDF driver on Windows. Deck
controller inputs reach Windows games as if a Deck were physically plugged in.
Wireless, low latency, full-duplex (rumble back to Deck).

## Non-goals

- Generic USB-over-IP. Not building a USB tunnel.
- Cross-controller emulation (Xbox, DualShock). Out of scope unless Deck
  emulation hits an unrecoverable wall.
- Multi-client / multi-Deck. One Deck, one Windows PC.
- Non-Windows clients.

## Why this design (decision history)

Worked through alternatives in order:

1. **USB/IP + usbip-win2.** Generic, works, but Windows side needs a
   test-signed kernel driver users must install. UX hostile.
2. **Userspace via ViGEmBus.** No kernel work on our side, but ViGEmBus is in
   maintenance/EOL and only exposes Xbox / DS4 shapes — would lose Deck
   features (gyro, trackpads, back paddles).
3. **Custom UDE driver, emulate Steam Controller gen 2.** Right shape, but SC2
   protocol is undocumented; reverse-engineering work + risk of an
   anti-counterfeit handshake.
4. **Custom UDE driver, emulate Steam Deck controller.** Selected. Protocol
   documented in Linux `drivers/hid/hid-steam.c` (GPL) and SDL's
   `SDL_hidapi_steamdeck.c` (zlib). Steam Input on Windows already recognizes
   Deck VID/PID. Symmetric: Deck → virtual Deck is identity transform.

## Components

```
+-------------------- Deck (Linux, Rust) --------------------+
| server-deck                                                |
|   read internal controller (hidraw or evdev)               |
|   encode canonical state                                   |
|   send over UDP (input)                                    |
|   receive over reliable channel (rumble out)               |
+-----------------------------|------------------------------+
                              |
                              | custom protocol over Wi-Fi
                              | UDP for inputs (high rate, drop OK)
                              | TCP / rUDP for outputs (rumble — reliable)
                              v
+------------------- Windows (Rust + C++) -------------------+
| client-win  (user-mode Rust service)                       |
|   recv network                                             |
|   jitter buffer (3-5 ms)                                   |
|   DeviceIoControl <-> driver                               |
|                  ^                                         |
|                  v                                         |
| driver  (KMDF + UdeCx, C++)                                |
|   virtual USB device, VID 0x28de PID 0x1205                |
|   HID descriptor + reports byte-identical to real Deck     |
|   feature-report path (lizard-mode disable, haptics ack)   |
+-----------------------------|------------------------------+
                              v
                Steam Input recognizes Deck controller
```

## Crates and dirs

- `crates/deck-protocol/` — Rust types for Deck HID reports, canonical
  controller state, network wire format. No I/O. Shared between Deck server
  and Windows client.
- `crates/server-deck/` — binary, runs on Deck. Reads controller, sends state.
  *(not yet scaffolded)*
- `crates/client-win/` — binary, runs on Windows. Receives state, talks to
  driver via DeviceIoControl. *(not yet scaffolded)*
- `driver/` — KMDF + UdeCx driver, C++. Outside the Cargo workspace.
  *(not yet scaffolded)*

## Wire protocol (sketch, v0)

Shared 16-byte header (magic, version, channel id, sequence number), then a
channel-specific body.

- Channel `INPUT`: UDP, Deck → Windows. Body = canonical controller state at
  ~500 Hz. Receiver drops out-of-order/late packets via sequence number.
- Channel `OUTPUT`: TCP (or reliable UDP later), Windows → Deck. Body = rumble
  / LED commands. Reliable.

Pairing: shared-secret handshake. Out of scope for v0; assume trusted LAN.

## Driver

KMDF driver linked against `UdeCx`. Exposes one virtual USB device matching
real Deck descriptors. User-mode IPC: one IOCTL pair (push input report, pop
output report).

Signing path:

- **Dev:** test-signing mode (`bcdedit /set testsigning on`).
- **Distribution:** EV cert + Microsoft Partner Center attestation signing.
- WHQL not pursued.

## Open risks

- Steam fingerprints something beyond HID descriptor (parent-hub topology,
  device serial). Mitigation: capture real Deck device tree on Windows,
  mirror what matters.
- Wi-Fi jitter spikes blow through the jitter buffer. Mitigation: 5 GHz only,
  DSCP-tag latency-sensitive packets, fall back to wired Deck dock.
- Feature-report sequence subtleties (lizard-mode disable, haptics-config
  ack). Mitigation: read `hid-steam.c` init path carefully, cross-check SDL.

## Build sequence

1. ✅ Lift Deck HID layout into `deck-protocol` (Rust types + codec).
2. ✅ Spike: parse a real Deck input report on Linux via hidraw.
   *Validated against real hardware; `BUTTON_MAP` confirmed for everything
   the Linux kernel driver covers. Capacitive thumbstick touch bits remain
   unknown and need a USB capture if/when they are wired up.*
3. ✅ Static UDE driver: hardcoded descriptors. Steam shows "Steam Deck
   Controls." *Verified in a Hyper-V Windows 11 VM (test-signed). Driver
   loads, virtual USB device enumerates (5 interfaces, COM port for the
   inert CDC ACM pair), HID class init completes, Steam Input opens the
   device.*
4. ✅ Feature-report path: lizard-mode disable + canned replies for the
   Steam Controller request channel (set-then-get over feature reports).
   *Steam recognizes the device as a Deck (`controller_neptune` config set
   loaded). `EvtControlUrb` handles the open-sequence messages
   `CLEAR_DIGITAL_MAPPINGS` / `SET_SETTINGS_VALUES` /
   `GET_ATTRIBUTES_VALUES` / `GET_STRING_ATTRIBUTE`. Haptic / rumble
   payloads are still discarded — that path lights up when step 7 wires
   them through to user-mode.*
5. ✅ User-mode IPC + live HID frames over IOCTL.
   *`IOCTL_DECK_PUSH_INPUT_REPORT` in `queue.cpp` dequeues the host's
   pending interrupt-IN URB on EP 0x83, copies the user-mode-supplied
   64-byte report into the URB buffer, and completes it. Verified
   end-to-end with `client-win --test` synthesizing alternating-A-button
   reports at ~250 Hz: Steam's Controller Layout test screen highlights
   the A button on / off at 1 Hz.*
6. ✅ Real Deck bytes → driver → Steam (protocol path).
   *Captured raw hidraw bytes from a real Deck (1249 frames, every frame
   `0x01 0x00 0x09 0x40` framed), replayed via `client-win --replay`,
   Steam's Controller Layout shows the captured button pattern blinking
   correctly. This caught a 60-vs-64 size bug in the protocol crate —
   `hid::REPORT_LEN` is 64, not 60; an earlier "fix" had set it to 60
   and would have misaligned the byte stream from real hardware.
   The remaining piece of the original step 6 ("over the network") is
   blocked on a Hyper-V external-vSwitch UDP-drop issue with the Intel
   I225-V NIC; the protocol pipeline itself is validated.*
7. ✅ Output path: rumble back to Deck.
   *`SET_REPORT(FEATURE)` payloads carrying the haptic/rumble msg_ids
   (`0xEA TRIGGER_HAPTIC_CMD`, `0x8F TRIGGER_HAPTIC_PULSE`,
   `0xEB TRIGGER_RUMBLE_CMD`) are forwarded from the kernel control
   handler into a manual queue, picked up by user-mode via
   `IOCTL_DECK_PEND_OUTPUT_REPORT`, sent over UDP to the Deck on the same
   port we listen on, and applied via `HIDIOCSFEATURE` on `/dev/hidrawN`.
   The wire `OUTPUT` body is a raw 64-byte feature report — Steam's bytes
   pass through unmodified, since both ends speak the same dialect.
   End-to-end validation requires (a) a real Deck running `server-deck`
   with `O_RDWR` on the hidraw node, and (b) the network drop on the
   Hyper-V external vSwitch resolved or the VM swapped for bare-metal
   Windows.*
8. Polish: reconnect, pairing, packaging.

## Pending validations

Things that need real hardware before the next protocol-extending step lands.

### Capacitive thumbstick touch bits

The Linux kernel driver does not parse capacitive-touch state for the
analog sticks, so `BUTTON_MAP` in `crates/deck-protocol/src/hid.rs` has
no entries for those bits. They need to be discovered from a USB capture
(USBPcap on Windows or `usbmon` on Linux while a real Deck is attached)
before they can be wired up. Mark any newly-discovered bits with a
`// from USB capture YYYY-MM-DD` comment so future readers can tell
kernel-sourced bits from capture-sourced ones.

## Decisions log

- 2026-05-06 — Picked Deck emulation over SC2 emulation. Documented protocol
  vs RE work, no auth-handshake risk.
- 2026-05-06 — Picked UDE over raw KMDF bus driver. Less PnP surface,
  Microsoft-supported framework for this exact use case.
- 2026-05-06 — UDP for inputs, reliable channel for outputs.
