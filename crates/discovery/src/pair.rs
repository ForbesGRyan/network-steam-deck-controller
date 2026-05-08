//! Interactive pair flow. Time-boxed; broadcasts (or unicasts in tests)
//! signed PAIRING beacons, prompts the user on first valid sighting, then
//! exchanges ACCEPT beacons until both sides confirm.
//!
//! Two layers:
//!   * `run_pair_with(cfg, ui)` — the headless state machine. Calls a
//!     `PairUI` trait to surface events and ask for the accept/reject
//!     decision. Use this from a GUI thread (channel-backed UI) or any
//!     other non-stdin context.
//!   * `run_pair<R, W>` — the original stdin/stdout wrapper, kept for
//!     CLI use. Implemented in terms of `run_pair_with`.

use std::collections::HashSet;
use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::beacon::{is_within_replay_window, REPLAY_WINDOW_US};
use crate::identity::Identity;
use crate::packet::{
    self, fingerprint, fingerprint_str, BeaconPacket, PUBKEY_LEN, FLAG_ACCEPT, FLAG_PAIRING,
    FPR_LEN, PACKET_LEN,
};
use crate::time::{now_iso8601, now_us};
use crate::trust::{save as save_trust, TrustedPeer};

/// Window over which we collect distinct PAIRING beacons before showing the
/// user a chooser. Prevents an attacker who simply broadcasts first from
/// pre-empting the legitimate peer in the prompt.
const CANDIDATE_COLLECT_WINDOW: Duration = Duration::from_millis(1500);

/// A peer observed broadcasting a `FLAG_PAIRING` beacon during the
/// collection window. `pubkey` is sender-controlled but signature-verified;
/// `name` is sender-controlled and untrusted display text.
#[derive(Clone, Debug)]
pub struct PairCandidate {
    pub pubkey: [u8; PUBKEY_LEN],
    pub name: String,
}

pub struct PairConfig {
    pub identity: Arc<Identity>,
    pub recv_sock: UdpSocket,
    /// Where to send each pairing/accept packet. Production: per-interface
    /// directed broadcasts plus `255.255.255.255` (see `netifs`). Tests: the
    /// other side's bound ephemeral port.
    pub targets: Vec<SocketAddr>,
    pub self_name: String,
    pub state_dir: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug)]
pub enum PairOutcome {
    Paired(TrustedPeer),
    Declined,
    Timeout,
    IoError(io::Error),
}

impl From<io::Error> for PairOutcome { fn from(e: io::Error) -> Self { Self::IoError(e) } }

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Decision { Accept, Reject }

/// Surfaces the pair flow's events to whatever UI is driving it. The
/// state machine calls these in order; `prompt_peer` blocks the state
/// machine until the user makes a decision.
pub trait PairUI {
    fn on_started(&mut self, my_fingerprint: &str, _self_name: &str) {
        let _ = my_fingerprint;
    }
    /// User saw a candidate peer. Return `Accept` to proceed to the
    /// confirmation phase; `Reject` to keep listening for others.
    fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision;
    /// Pick one peer from a batch of candidates collected during the
    /// pre-prompt window. Returns `None` to permanently decline every
    /// peer in the batch and keep listening. Default impl falls through
    /// to `prompt_peer` per-candidate so existing UIs keep working.
    fn prompt_candidates(&mut self, candidates: &[PairCandidate]) -> Option<PairCandidate> {
        for c in candidates {
            let fpr = fingerprint_str(&fingerprint(&c.pubkey));
            if matches!(self.prompt_peer(&c.name, &fpr), Decision::Accept) {
                return Some(c.clone());
            }
        }
        None
    }
    fn on_paired(&mut self, _peer: &TrustedPeer) {}
    fn on_failed(&mut self, _reason: &str) {}
}


const TICK: Duration = Duration::from_millis(200);

/// Headless pair state machine. Drives discovery, asks the UI to pick a
/// candidate via `PairUI::prompt_candidates` (default impl falls through to
/// `prompt_peer` per-candidate in arrival order), exchanges ACCEPT,
/// persists trust.
///
/// Buffers every distinct PAIRING peer seen during a short window so an
/// attacker who races the legitimate peer's first beacon can't pre-empt the
/// prompt.
///
/// Sends are issued on a separate thread so a long block in `prompt_peer`
/// (e.g. the user pondering an Accept/Reject button) doesn't stall our
/// outbound beacons. Without this, the first side to prompt would stop
/// broadcasting `FLAG_PAIRING`, the second side would never see it, and
/// only one side ever reached its prompt — the deadlock the user reported
/// on 2026-05-08.
pub fn run_pair_with<U: PairUI>(cfg: &PairConfig, ui: &mut U) -> PairOutcome {
    cfg.recv_sock.set_read_timeout(Some(TICK)).ok();
    ui.on_started(
        &fingerprint_str(&fingerprint(&cfg.identity.pubkey)),
        &cfg.self_name,
    );

    let send_sock = match cfg.recv_sock.try_clone() {
        Ok(s) => s,
        Err(e) => {
            ui.on_failed(&format!("clone send socket: {e}"));
            return PairOutcome::IoError(e);
        }
    };
    let send_ctx = SendCtx {
        sock: send_sock,
        identity: cfg.identity.clone(),
        self_name: cfg.self_name.clone(),
        targets: cfg.targets.clone(),
    };
    // Phase-1 default: blast PAIRING with no peer fingerprint.
    let send_state = Arc::new(SendState {
        flags: AtomicU8::new(FLAG_PAIRING),
        peer_fpr: Mutex::new([0_u8; FPR_LEN]),
        stop: AtomicBool::new(false),
    });
    let _sender = SenderHandle::spawn(send_ctx, send_state.clone());

    let deadline = Instant::now() + cfg.timeout;
    let mut buf = [0_u8; PACKET_LEN];
    let mut declined: HashSet<[u8; PUBKEY_LEN]> = HashSet::new();

    // Phase 1+2: collect distinct PAIRING peers for a short window, then
    // ask the caller to pick one. Repeat if they decline the whole batch.
    let candidate = loop {
        match collect_candidates(cfg, &mut buf, &declined, deadline) {
            CollectOutcome::Candidates(found) => match ui.prompt_candidates(&found) {
                Some(c) => break c,
                None => {
                    for c in found {
                        declined.insert(c.pubkey);
                    }
                }
            },
            CollectOutcome::Timeout => {
                ui.on_failed("timeout waiting for peer");
                return PairOutcome::Timeout;
            }
            CollectOutcome::IoError(e) => {
                ui.on_failed(&format!("recv error: {e}"));
                return PairOutcome::IoError(e);
            }
        }
    };

    // Phase 3: switch sender to ACCEPT mode (200 ms cadence, peer-fpr set).
    let candidate_fpr = fingerprint(&candidate.pubkey);
    let my_fpr = fingerprint(&cfg.identity.pubkey);
    if let Ok(mut slot) = send_state.peer_fpr.lock() {
        *slot = candidate_fpr;
    }
    send_state.flags.store(FLAG_ACCEPT, Ordering::Relaxed);

    loop {
        if Instant::now() >= deadline {
            ui.on_failed("timeout during accept exchange");
            return PairOutcome::Timeout;
        }
        match cfg.recv_sock.recv_from(&mut buf) {
            Ok((n, _src)) if n >= PACKET_LEN => {
                // `recv_from` may deliver more than PACKET_LEN on some paths
                // (jumbo padding, datagram fragmentation reassembly); slice
                // to the canonical body before verify.
                let Ok(p) = packet::verify(&buf[..PACKET_LEN]) else { continue };
                if !packet_in_replay_window(&p) { continue; }
                if p.pubkey == candidate.pubkey
                    && (p.flags & FLAG_ACCEPT) != 0
                    && p.peer_fpr == my_fpr
                {
                    let peer = TrustedPeer {
                        pubkey: candidate.pubkey,
                        name: candidate.name.clone(),
                        paired_at: now_iso8601(),
                        last_seen_addr: None,
                    };
                    if let Err(e) = save_trust(&cfg.state_dir, &peer) {
                        let msg = format!("save trust: {e:?}");
                        ui.on_failed(&msg);
                        return PairOutcome::IoError(io::Error::other("save trust"));
                    }
                    ui.on_paired(&peer);
                    return PairOutcome::Paired(peer);
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => {
                ui.on_failed(&format!("recv error: {e}"));
                return PairOutcome::IoError(e);
            }
        }
    }
    // SenderHandle::Drop signals stop and joins.
}

enum CollectOutcome {
    Candidates(Vec<PairCandidate>),
    Timeout,
    IoError(io::Error),
}

/// Buffer every distinct PAIRING peer seen during `CANDIDATE_COLLECT_WINDOW`
/// once the first one shows up. Returns as soon as the window elapses with
/// at least one candidate that hasn't been previously declined, or on the
/// outer pair-flow deadline.
fn collect_candidates(
    cfg: &PairConfig,
    buf: &mut [u8; PACKET_LEN],
    declined: &HashSet<[u8; PUBKEY_LEN]>,
    deadline: Instant,
) -> CollectOutcome {
    let mut seen: HashSet<[u8; PUBKEY_LEN]> = HashSet::new();
    let mut candidates: Vec<PairCandidate> = Vec::new();
    let mut window_close: Option<Instant> = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return CollectOutcome::Timeout;
        }
        if let Some(close) = window_close {
            if now >= close && !candidates.is_empty() {
                return CollectOutcome::Candidates(candidates);
            }
        }
        match cfg.recv_sock.recv_from(buf) {
            Ok((n, _src)) if n >= PACKET_LEN => {
                let Ok(p) = packet::verify(&buf[..PACKET_LEN]) else { continue };
                if (p.flags & FLAG_PAIRING) == 0 || p.pubkey == cfg.identity.pubkey {
                    continue;
                }
                if declined.contains(&p.pubkey) || !seen.insert(p.pubkey) {
                    continue;
                }
                if !packet_in_replay_window(&p) {
                    continue;
                }
                candidates.push(PairCandidate {
                    pubkey: p.pubkey,
                    name: p.name,
                });
                if window_close.is_none() {
                    window_close = Some(now + CANDIDATE_COLLECT_WINDOW);
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return CollectOutcome::IoError(e),
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn packet_in_replay_window(p: &BeaconPacket) -> bool {
    let now32 = now_us() as u32;
    let pkt32 = p.timestamp_us as u32;
    is_within_replay_window(pkt32, now32, REPLAY_WINDOW_US)
}

struct SendCtx {
    sock: UdpSocket,
    identity: Arc<Identity>,
    self_name: String,
    targets: Vec<SocketAddr>,
}

struct SendState {
    flags: AtomicU8,
    peer_fpr: Mutex<[u8; FPR_LEN]>,
    stop: AtomicBool,
}

struct SenderHandle {
    state: Arc<SendState>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SenderHandle {
    fn spawn(ctx: SendCtx, state: Arc<SendState>) -> Self {
        let state_for_thread = state.clone();
        let handle = thread::Builder::new()
            .name("pair-sender".into())
            .spawn(move || sender_loop(&ctx, &state_for_thread))
            .ok();
        Self { state, handle }
    }
}

impl Drop for SenderHandle {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn sender_loop(ctx: &SendCtx, state: &Arc<SendState>) {
    let mut next = Instant::now();
    while !state.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next {
            let flags = state.flags.load(Ordering::Relaxed);
            let peer_fpr = if flags & FLAG_ACCEPT != 0 {
                state.peer_fpr.lock().map(|g| *g).unwrap_or([0; FPR_LEN])
            } else {
                [0; FPR_LEN]
            };
            send_one_with_sock(ctx, flags, peer_fpr);
            // Phase 1 is fine at 1 Hz; phase 3 wants ~5 Hz to keep the
            // accept-exchange window tight.
            let interval = if flags & FLAG_ACCEPT != 0 {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(1)
            };
            next = now + interval;
        }
        // Coarse polling — we don't need sub-50 ms accuracy and we want to
        // notice the stop flag promptly.
        thread::sleep(Duration::from_millis(50));
    }
}

fn send_one_with_sock(ctx: &SendCtx, flags: u8, peer_fpr: [u8; FPR_LEN]) {
    let pkt = BeaconPacket {
        flags,
        pubkey: ctx.identity.pubkey,
        peer_fpr,
        timestamp_us: now_us(),
        name: ctx.self_name.clone(),
    };
    let mut buf = [0_u8; PACKET_LEN];
    if packet::sign_into(&ctx.identity.signing, &pkt, &mut buf).is_ok() {
        for target in &ctx.targets {
            let _ = ctx.sock.send_to(&buf, target);
        }
    }
}

/// Stdin/stdout wrapper around `run_pair_with`. Kept for CLI use.
pub fn run_pair<R: Read, W: Write>(
    cfg: &PairConfig,
    stdin: &mut R,
    log: &mut W,
) -> PairOutcome {
    struct CliUI<'a, R: Read, W: Write> {
        stdin: &'a mut R,
        log: &'a mut W,
    }
    impl<R: Read, W: Write> PairUI for CliUI<'_, R, W> {
        fn prompt_peer(&mut self, name: &str, fingerprint: &str) -> Decision {
            let _ = writeln!(self.log, "found peer {name} fingerprint {fingerprint}");
            let _ = writeln!(self.log, "accept this peer? [y/N]: ");
            let mut reader = io::BufReader::new(&mut *self.stdin);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return Decision::Reject;
            }
            if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Decision::Accept
            } else {
                Decision::Reject
            }
        }
        fn on_paired(&mut self, peer: &TrustedPeer) {
            let _ = writeln!(
                self.log,
                "paired with {} ({})",
                peer.name,
                fingerprint_str(&fingerprint(&peer.pubkey)),
            );
        }
        fn on_failed(&mut self, reason: &str) {
            let _ = writeln!(self.log, "pair failed: {reason}");
        }
    }
    let mut ui = CliUI { stdin, log };
    run_pair_with(cfg, &mut ui)
}

