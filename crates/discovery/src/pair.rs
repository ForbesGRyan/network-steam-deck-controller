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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::identity::Identity;
use crate::packet::{
    self, fingerprint, fingerprint_str, BeaconPacket, FLAG_ACCEPT, FLAG_PAIRING, FPR_LEN,
    PACKET_LEN,
};
use crate::trust::{save as save_trust, TrustedPeer};

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
    fn on_paired(&mut self, _peer: &TrustedPeer) {}
    fn on_failed(&mut self, _reason: &str) {}
}

const TICK: Duration = Duration::from_millis(200);

/// Headless pair state machine. Drives discovery, asks the UI for a
/// decision via `PairUI::prompt_peer`, exchanges ACCEPT, persists trust.
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
    let mut declined: HashSet<[u8; crate::packet::PUBKEY_LEN]> = HashSet::new();

    // Phase 1+2: discover and prompt. (Sender thread keeps blasting
    // `FLAG_PAIRING` whether or not we're parked at the prompt.)
    let candidate = loop {
        if Instant::now() >= deadline {
            ui.on_failed("timeout waiting for peer");
            return PairOutcome::Timeout;
        }
        match cfg.recv_sock.recv_from(&mut buf) {
            Ok((n, _src)) if n == PACKET_LEN => {
                if let Ok(p) = packet::verify(&buf) {
                    if (p.flags & FLAG_PAIRING) != 0 && p.pubkey != cfg.identity.pubkey {
                        if declined.contains(&p.pubkey) { continue; }
                        let fpr_str = fingerprint_str(&fingerprint(&p.pubkey));
                        match ui.prompt_peer(&p.name, &fpr_str) {
                            Decision::Accept => break p,
                            Decision::Reject => { declined.insert(p.pubkey); }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => {
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
            Ok((n, _src)) if n == PACKET_LEN => {
                if let Ok(p) = packet::verify(&buf) {
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
            .spawn(move || sender_loop(ctx, state_for_thread))
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

fn sender_loop(ctx: SendCtx, state: Arc<SendState>) {
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
            send_one_with_sock(&ctx, flags, peer_fpr);
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

#[allow(clippy::cast_possible_truncation)]
fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

fn now_iso8601() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    #[allow(clippy::cast_possible_wrap)]
    let (y, mo, d, h, mi, s) = civil_from_secs(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_secs(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = 86_400_i64;
    let days = secs.div_euclid(day_secs);
    let tod = secs.rem_euclid(day_secs);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = (doe - (365 * yoe + yoe / 4 - yoe / 100)) as u32;
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (tod / 3600) as u32, ((tod / 60) % 60) as u32, (tod % 60) as u32)
}
