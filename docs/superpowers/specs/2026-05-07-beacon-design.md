# Beacon: LAN discovery + first-time pairing

Status: design approved 2026-05-07. Implementation pending.

## Goal

Remove the two manual steps in today's setup:

1. The Deck-side CLI argument `<windows-ip>:49152`. Users have to know the
   PC's LAN IP and re-edit the systemd unit when DHCP rotates it.
2. The shared `NETWORK_DECK_KEY` env var (64 hex chars) that has to be
   generated, copied, and kept in sync on both ends.

After this work: install both binaries, run `client-win pair` and
`server-deck pair` once, confirm matching fingerprints, done. Reboots,
NIC changes, and DHCP reassignments don't require user action.

## Non-goals

- Multi-peer trust. Project is single-PC ↔ single-Deck per
  `ARCHITECTURE.md`. Trusted-peers file holds exactly one entry.
- Cross-subnet discovery. UDP broadcasts don't traverse subnets; users on
  segmented networks (guest VLAN, AP isolation) will see "no peer
  announced" and need to fix their network or fall back to the dock.
- Internet-facing pairing. LAN only.
- Replacing the existing per-packet HMAC-SHA256 auth. The beacon
  replaces only the *key source*; the auth code itself is reused.

## Decisions (from brainstorm)

- **Scope:** discovery + pairing (key bootstrap). Not just discovery.
- **Roles:** symmetric. Both peers broadcast and both listen.
- **Pair UX:** trust-on-first-use with mutual button-press confirm
  showing fingerprints on both ends. No PIN typed, no QR.
- **Transport:** raw UDP broadcast on the existing data port (49152),
  distinguished from data packets by a different 4-byte magic.
- **Identity:** long-lived Ed25519 keypair per peer, generated on first
  run. Beacon packets are signed.
- **Pair mode:** explicit `pair` subcommand on each binary. Time-boxed
  to 120 s. Normal mode rejects beacons from non-trusted pubkeys.
- **Old key path:** `NETWORK_DECK_KEY` env var is removed entirely.
  Single auth path. Anyone on the old setup re-pairs once.

## Architecture

### Crate layout

```
crates/
  deck-protocol/    (unchanged)
  discovery/        NEW
    src/
      lib.rs        re-exports
      packet.rs     BeaconPacket — wire type + serde + sig verify
      identity.rs   Ed25519 keypair: load_or_generate, fingerprint
      trust.rs      TrustedPeer + trusted-peers.toml read/write
      beacon.rs     async Beacon: broadcast + listen + dedup loop
      pair.rs       run_pair(): time-boxed handshake state machine
      crypto.rs     HKDF derive_session_key over sorted pubkeys
  server-deck/      gains `pair` subcommand, beacon task in normal mode
  client-win/       gains `pair` subcommand, beacon task in normal mode
```

`discovery` owns all I/O for the new feature. `deck-protocol` keeps its
"no I/O, no_std-friendly" charter — beacon's `getrandom` and file I/O do
not bleed into it.

### Component responsibilities

- **`discovery::Beacon`** — owns one UDP socket bound to
  `0.0.0.0:49152` (shared with the data plane via `Arc<UdpSocket>`).
  Sends a signed `BeaconPacket` to `255.255.255.255:49152` every 1 s.
  On recv of a beacon packet, verifies signature, drops anything not
  from the trusted peer (in normal mode), updates an in-memory
  `last_seen` entry. Exposes `current_peer() -> Option<(SocketAddr,
  SessionKey)>` for the data plane.
- **`discovery::pair::run_pair`** — separate entrypoint. Beacons with
  the `PAIRING` flag set, accepts beacons from anyone, prints peer
  fingerprint + name + IP, prompts y/n, on mutual accept writes
  `trusted-peers.toml` and exits. Default time box: 120 s.
- **`discovery::identity`** — `load_or_generate(path) -> Identity`.
  Identity = Ed25519 keypair + cached fingerprint string.
- **`discovery::trust`** — typed read/write of a single-peer TOML file.
- **`discovery::crypto::derive_session_key(my_pub, peer_pub) -> [u8; 32]`** —
  HKDF-SHA256 over `min(a,b) || max(a,b)`, info
  `b"network-deck v1 hmac"`. Replaces today's `NETWORK_DECK_KEY`. Same
  32-byte shape, so the per-packet HMAC code is untouched.

### Runtime data flow (normal, post-pair)

```
discovery::Beacon ── observes peer addr ──▶ shared PeerState ──▶ data plane reads addr+key
       ▲                                                              │
       │  (UDP socket shared)                                          ▼
       └─────────── 49152 ◀────── existing input/output channels ──────┘
```

Data plane stops needing a hard-coded `<windows-ip>:49152` arg on the
Deck and stops needing a port flag on Windows beyond the default —
both come from beacon.

## Wire protocol

Beacon packets share UDP 49152 with data packets. Demux on the 4-byte
magic in byte 0. Data plane keeps its existing magic; beacon uses
`b"NDB1"`. Anything that doesn't match either magic is dropped.

```rust
struct BeaconPacket {                       // 144 bytes fixed
    magic:        [u8; 4],   // b"NDB1"
    version:      u8,        // 1
    flags:        u8,        // bit0 = PAIRING, bit1 = ACCEPT, rest reserved
    name_len:     u8,        // 0..=32
    _pad:         u8,
    pubkey:       [u8; 32],  // sender Ed25519 pub
    peer_fpr:     [u8; 8],   // first 8 B of SHA256(peer pubkey we want);
                             //   zeros in PAIRING beacons; set in ACCEPT
    timestamp_us: u64,       // sender wall-clock; ±30 s replay window
    name:         [u8; 32],  // utf-8, zero-padded; first name_len bytes valid
    sig:          [u8; 64],  // Ed25519 over all preceding bytes
}
```

Notes:

- **Fixed size, no length prefixes.** Trivial parser; one MTU.
- **`peer_fpr` in normal beacons** lets each peer broadcast the
  fingerprint of the partner it expects, blocking an unrelated paired
  device on the same LAN from being mistaken for the live peer.
- **Always signed.** Stops a LAN attacker from spoofing announces and
  redirecting data traffic to a black hole.
- **Replay window reuses the helper** the data plane uses today (±30 s
  on the low-32-bit microsecond timestamp).

## Pair state machine

Both sides run the `pair` subcommand within the 120 s window.

```
            ┌────────────────────────────────────────────────────┐
   start ──▶│ 1. ANNOUNCE   broadcast PAIRING beacons every 1 s  │
            └─────────────────────────┬──────────────────────────┘
                                      │ recv valid PAIRING beacon (peer X)
                                      ▼
            ┌────────────────────────────────────────────────────┐
            │ 2. PROMPT     show peer name + fingerprint + IP    │
            │               ask y/n, blocks                      │
            └─────────────────────────┬──────────────────────────┘
                                      │ user accepts
                                      ▼
            ┌────────────────────────────────────────────────────┐
            │ 3. ACCEPT     broadcast ACCEPT beacon: peer_fpr=X  │
            │               every 200 ms for up to 10 s          │
            └─────────────────────────┬──────────────────────────┘
                                      │ recv ACCEPT from X with peer_fpr = me
                                      ▼
            ┌────────────────────────────────────────────────────┐
            │ 4. COMMIT     write trusted-peers, exit 0          │
            └────────────────────────────────────────────────────┘
```

Failure / timeout edges:

- 120 s overall window expires anywhere → exit 1 with reason printed.
- User declines at PROMPT → return to ANNOUNCE; let another peer show.
- Two valid PAIRING beacons from different peers seen → list both,
  user picks (rare; covers two Decks pairing simultaneously).
- ACCEPT phase doesn't see partner's matching ACCEPT in 10 s → exit 1
  with "the other side never confirmed."

Mutual ACCEPT closes the TOFU window: if I confirm but you don't,
neither side persists trust. A single-side attacker accept is wasted.

## Persistence

### State directory

| Side | Path | Override |
|---|---|---|
| Windows | `%LOCALAPPDATA%\network-deck\` | `--state-dir <path>` |
| Deck (Linux) | `$XDG_STATE_HOME/network-deck/` (default `~/.local/state/network-deck/`) | `--state-dir <path>` |

systemd unit pins `StateDirectory=network-deck` →
`/var/lib/network-deck/`.

### Files

- `identity.key` — raw 32-byte Ed25519 seed, generated on first launch
  via `getrandom`. Mode `0600` on Linux; equivalent ACL on Windows
  (deny everyone except current user).
- `trusted-peers.toml`:

  ```toml
  [peer]
  pubkey         = "base64(32 B)"
  name           = "deck-living-room"
  paired_at      = "2026-05-07T19:23:11Z"
  last_seen_addr = "192.168.1.42:49152"
  ```

  `last_seen_addr` is a hint. Beacon overwrites it in memory on every
  fresh observation. Data plane uses the live in-memory value; the
  on-disk value is only consulted on cold start before the first
  beacon arrives.

### Fingerprint format

First 8 bytes of `SHA256(pubkey)`, rendered as
`aa:bb:cc:dd:ee:ff:gg:hh`. Same format printed on both ends during
pair. Same format used for the on-screen fingerprint comparison.

## Binary integration

### `server-deck`

Today:

```
server-deck /dev/hidrawN <windows-ip>:49152
```

After:

```
server-deck /dev/hidrawN                       # normal: discover via beacon
server-deck pair                               # one-shot pair
server-deck --state-dir /etc/network-deck …    # override state dir
```

The `<windows-ip>:port` positional arg is removed. If
`trusted-peers.toml` is missing, normal mode prints
`no trusted peer; run \`server-deck pair\`` and exits non-zero.

`main` shape:

1. Load `Identity` (`load_or_generate`).
2. Load `TrustedPeer` or bail.
3. Bind UDP socket on `0.0.0.0:49152`.
4. Spawn `Beacon` task on the shared socket.
5. Spawn the existing hidraw → wire task. Instead of taking a target
   addr from argv, ask `Beacon::current_peer()` each send tick. If
   `None`, drop the frame and bump a counter; status line shows
   `peer: searching`.

### `client-win`

Today:

```
client-win.exe 49152
```

After:

```
client-win.exe                                 # normal; port fixed at 49152
client-win.exe pair
client-win.exe --test                          # unchanged synthetic mode
client-win.exe --replay capture.bin            # unchanged
```

Same shape: `Identity` + `TrustedPeer` + shared socket + `Beacon` task
+ existing recv loop. `--test` and `--replay` skip the trusted-peer
check; they don't touch the network.

`NETWORK_DECK_KEY` env var is removed from both binaries. The HMAC key
fed into the existing per-packet auth code now comes from
`discovery::crypto::derive_session_key(my_pub, peer_pub)` — same
32-byte shape, so the auth module is untouched.

### Packaging

- `crates/server-deck/scripts/network-deck-server.service` — drop
  `Environment=NETWORK_DECK_KEY=…`, drop the Windows-IP env line, add
  `StateDirectory=network-deck`.
- `crates/server-deck/scripts/70-steam-deck.rules` — unchanged.
- `driver/scripts/install.ps1` — unchanged. `client-win` runs as a
  normal user-mode service; no driver-side changes needed.
- `README.md` — replace the `NETWORK_DECK_KEY=<64 hex>` paragraph
  with a "Pair once" section: run `client-win pair` and
  `server-deck pair`, confirm matching fingerprints, done.

## Errors

Three classes, each handled at one site:

1. **Disk / state errors.** `identity.rs` and `trust.rs` return
   `io::Error`-wrapping results. Bubbled to `main`, which prints a
   one-line reason and exits non-zero. No retries — a missing or
   corrupt state file is a configuration problem.
2. **Beacon recv errors.** Silently dropped after one
   `tracing::warn` per error class. Bad sig, wrong magic, wrong
   version, replay-window violation, untrusted pubkey: counted
   (`beacon.rejected{reason=…}`), never crashes the loop. Hostile LAN
   traffic must not take down the bridge.
3. **Pair-mode errors.** Surfaced to the user. Timeout, declined,
   conflicting peers all print a clear sentence and exit non-zero. The
   pair binary is interactive; the user is right there.

### Status-line additions

Both binaries' periodic status line gains:

```
peer: searching                       (no beacon seen yet this run)
peer: 192.168.1.42  age 0.7 s         (live, recent)
peer: 192.168.1.42  age 14 s STALE    (>5 s since last beacon)
```

Data plane keeps trying to send to the cached `last_seen_addr` even
when STALE; the status line just warns.

## Tests

Crate-level (`crates/discovery/`):

- `packet`: round-trip serde for every flag combination; sig-verify
  accepts a good packet, rejects a packet with one bit flipped in each
  field.
- `crypto::derive_session_key`: symmetric (`derive(a,b) ==
  derive(b,a)`); two different pairs produce different keys; known
  vector against a hand-computed HKDF.
- `trust`: write-then-read is identity; corrupt TOML returns error,
  doesn't panic.
- `identity`: `load_or_generate` on empty dir creates file with the
  right size and `0600` perms (Linux); second call returns the same
  key.
- `pair` state machine driven by an in-memory transport (no real
  socket): two `run_pair` futures, both accept → both write
  trusted-peers; one declines → neither writes; timeout path → neither
  writes.

Workspace-level smoke (no real hardware):

- `server-deck` and `client-win` each start with empty state → both
  exit with the "run pair" message. No panics, no socket bound.
- A `discovery` integration test spins two `Beacon`s on `127.0.0.1`
  with different ports + a forwarding shim, runs pair end-to-end,
  restarts both as normal-mode beacons, asserts each `current_peer()`
  resolves the other within 2 s.

Manual validation checklist (added to ARCHITECTURE.md as build step 9,
since step 8 polish is already shipped):

- Pair on real hardware (Deck + Windows VM or bare metal). Confirm
  both fingerprints visibly match.
- Reboot both ends; data plane comes back up without re-pair.
- Run `client-win pair` against an old `server-deck` build (still
  expecting the CLI IP arg) → server fails fast with the "run pair"
  message; no silent breakage.
- Run two `server-deck pair` instances on the same LAN against one
  `client-win pair` → Windows shows both, prompts, and the chosen one
  persists.

## Edge cases

- **NIC change / DHCP reassignment.** Beacon picks up the new address
  within 1 s and overwrites `last_seen_addr` in memory. Data plane
  resumes on next packet. No user action.
- **Same fingerprint, two different LANs.** Out of scope per the
  single-pair non-goal. The trusted-peers file just records what it
  last paired with.
- **Subnet-isolated Wi-Fi (guest VLAN, AP isolation).** Broadcasts
  don't traverse, so beacon never sees the other side. Status line
  stays `peer: searching`; pair mode also fails with "no peer
  announced." Documented in README troubleshooting.
- **Hyper-V vSwitch UDP-drop issue from build step 6.** Beacon is UDP
  too and will hit it identically. Not a new risk; resolution path
  (USB ethernet adapter or bare-metal pivot) is shared.
- **Re-pairing replaces the peer.** `pair` overwrites
  `trusted-peers.toml` unconditionally. To "forget," the user deletes
  the file or runs `pair` against a different partner.
