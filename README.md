# network-steam-deck-controller

Wireless bridge that streams Steam Deck controller inputs over Wi-Fi to a
Windows PC, where they appear as a virtual Steam Deck controller. Steam Input
on Windows handles all gameplay-side mapping (gyro, trackpads, back paddles,
per-game profiles).

> **Status:** early-stage scaffold. Rust crates compile and pass 13 unit
> tests. The Deck → hidraw → wire pipeline has been validated on real
> hardware (`BUTTON_MAP` confirmed for everything the kernel driver
> exposes). The Windows kernel driver carries real Steam Deck USB +
> HID descriptors but its UDE bring-up bodies are still stubbed pending
> WDK install. See [ARCHITECTURE.md](ARCHITECTURE.md) for the full
> design and remaining validations.

## Why

You have a Steam Deck and a Windows gaming PC. You want to use the Deck as a
wireless controller — including its gyro, trackpads, and back paddles —
without plugging anything in. This project does that by:

1. Reading the Deck's internal controller via `/dev/hidraw` on the Deck.
2. Shipping canonical state over UDP to your PC.
3. A small Windows kernel driver presents a virtual Steam Deck controller
   to the OS.
4. Steam Input recognizes it natively — every Steam game that already
   supports the Deck just works.

Why not VirtualHere / USB-over-IP / ViGEmBus / Steam Link?
[ARCHITECTURE.md](ARCHITECTURE.md#why-this-design-decision-history) walks
through each alternative and why this design was chosen.

## Layout

```
crates/
  deck-protocol/    types + HID codec + wire codec (no I/O, no_std-friendly)
  server-deck/      Linux binary: hidraw -> wire -> UDP
  client-win/       Windows binary: UDP -> wire -> HID -> driver IOCTL
driver/             KMDF + UdeCx kernel driver (C++) + INF
tools/              fetch-on-demand reference materials
ARCHITECTURE.md     design history, build sequence, pending validations
```

## Build

The Rust workspace builds anywhere `cargo` runs:

```sh
cargo build --workspace
cargo test --workspace
```

Per-binary platform support:

| Binary | Real platform | Other platforms |
|---|---|---|
| `server-deck` | Linux (uses `/dev/hidraw*`) | builds, exits with "Linux only" |
| `client-win` | Windows (uses Win32 + driver IOCTL) | builds, runs in listen-only mode |

The kernel driver is **not** in the Cargo workspace. It needs Visual Studio
+ the Windows Driver Kit; see [driver/README.md](driver/README.md) for
setup.

## Run

End-to-end use requires real hardware and an installed driver, neither of
which exists yet. What works today:

**Validate the protocol crate:**

```sh
cargo test -p deck-protocol
```

**Run the wire layer without a driver** (the Windows side will warn that
the driver isn't available and fall back to listen-only mode, which still
exercises the full UDP-decode and HID-encode pipeline):

Windows PC (PowerShell):
```powershell
.\target\release\client-win.exe 49152
```

Steam Deck (after the hidraw spike — see
[ARCHITECTURE.md#pending-validations](ARCHITECTURE.md#pending-validations)):
```sh
sudo ./target/release/server-deck /dev/hidrawN <windows-ip>:49152
```

Both ends print live-updating stats; on Windows you'll see packet counts
climbing and decoded controller state mirroring whatever's happening on
the Deck.

## What's left

Tracked in detail in [ARCHITECTURE.md](ARCHITECTURE.md#build-sequence):

1. Lift Deck HID layout into `deck-protocol`. — done
2. Hidraw spike: validate `BUTTON_MAP` against real hardware. — done
3. Static UDE driver: descriptors, plug-in flow, "Steam Deck Controls"
   appears in Steam. — done (Hyper-V VM with test signing)
4. Feature-report path: lizard-mode disable, haptics-config ack. — done
   (Steam recognizes as Deck, `controller_neptune` config loads)
5. User-mode IPC + live HID frames over IOCTL. — done
   (`client-win --test` toggles the A button visible in Steam at 1 Hz)
6. Real Deck bytes → driver → Steam (protocol). — done
   (replayed 1249 captured frames via `client-win --replay`, button
   transitions show in Steam Controller Layout)
7. Output channel: rumble/haptics back to Deck. — done (path wired;
   end-to-end validation pending network and a Deck running `server-deck`)
8. Polish: reconnect, pairing, packaging.

## License

The Rust workspace is dual-licensed under MIT OR Apache-2.0 (see the
`license` field in `Cargo.toml`). The driver will follow the same once
its bodies are written.

Reference materials under `tools/reference/` (Linux kernel sources used to
derive the HID codec) are GPL-2.0+ and intentionally not committed —
fetch them on demand per [tools/README.md](tools/README.md).
