//! Interactive pair flow. Time-boxed; broadcasts (or unicasts in tests)
//! signed PAIRING beacons, prompts the user on first valid sighting, then
//! exchanges ACCEPT beacons until both sides confirm.

use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
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
    /// Address to send pairing/accept packets to. Production: broadcast addr.
    /// Tests: the other side's bound ephemeral port.
    pub unicast_target: SocketAddr,
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

const TICK: Duration = Duration::from_millis(200);

pub fn run_pair<R: Read, W: Write>(
    cfg: &PairConfig,
    stdin: &mut R,
    log: &mut W,
) -> PairOutcome {
    cfg.recv_sock
        .set_read_timeout(Some(TICK))
        .ok();

    let deadline = Instant::now() + cfg.timeout;
    let mut next_send = Instant::now();
    let mut buf = [0_u8; PACKET_LEN];

    // Phase 1+2: discover and prompt.
    let candidate = loop {
        if Instant::now() >= deadline { return PairOutcome::Timeout; }
        if Instant::now() >= next_send {
            send_one(cfg, FLAG_PAIRING, [0; FPR_LEN]);
            next_send = Instant::now() + Duration::from_secs(1);
        }
        match cfg.recv_sock.recv_from(&mut buf) {
            Ok((n, _src)) if n == PACKET_LEN => {
                if let Ok(p) = packet::verify(&buf) {
                    if (p.flags & FLAG_PAIRING) != 0 && p.pubkey != cfg.identity.pubkey {
                        let fpr_str = fingerprint_str(&fingerprint(&p.pubkey));
                        let _ = writeln!(
                            log,
                            "found peer {name} fingerprint {fpr_str}",
                            name = p.name,
                        );
                        if prompt_yes(stdin, log) { break p; }
                    }
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return PairOutcome::IoError(e),
        }
    };

    // Phase 3: exchange ACCEPT for up to 10 s.
    let accept_deadline = (Instant::now() + Duration::from_secs(10)).min(deadline);
    let candidate_fpr = fingerprint(&candidate.pubkey);
    let my_fpr = fingerprint(&cfg.identity.pubkey);
    let mut next_send = Instant::now();

    loop {
        if Instant::now() >= accept_deadline { return PairOutcome::Timeout; }
        if Instant::now() >= next_send {
            send_one(cfg, FLAG_ACCEPT, candidate_fpr);
            next_send = Instant::now() + Duration::from_millis(200);
        }
        match cfg.recv_sock.recv_from(&mut buf) {
            Ok((n, _src)) if n == PACKET_LEN => {
                if let Ok(p) = packet::verify(&buf) {
                    if p.pubkey == candidate.pubkey
                        && (p.flags & FLAG_ACCEPT) != 0
                        && p.peer_fpr == my_fpr
                    {
                        // Phase 4: commit.
                        let peer = TrustedPeer {
                            pubkey: candidate.pubkey,
                            name: candidate.name.clone(),
                            paired_at: now_iso8601(),
                            last_seen_addr: None,
                        };
                        if let Err(e) = save_trust(&cfg.state_dir, &peer) {
                            let _ = writeln!(log, "save trust: {e:?}");
                            return PairOutcome::IoError(io::Error::other("save trust"));
                        }
                        let _ = writeln!(log, "paired with {} ({})",
                            peer.name, fingerprint_str(&candidate_fpr));
                        return PairOutcome::Paired(peer);
                    }
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return PairOutcome::IoError(e),
        }
    }
}

fn send_one(cfg: &PairConfig, flags: u8, peer_fpr: [u8; FPR_LEN]) {
    let pkt = BeaconPacket {
        flags,
        pubkey: cfg.identity.pubkey,
        peer_fpr,
        timestamp_us: now_us(),
        name: cfg.self_name.clone(),
    };
    let mut buf = [0_u8; PACKET_LEN];
    if packet::sign_into(&cfg.identity.signing, &pkt, &mut buf).is_ok() {
        let _ = cfg.recv_sock.send_to(&buf, cfg.unicast_target);
    }
}

fn prompt_yes<R: Read, W: Write>(stdin: &mut R, log: &mut W) -> bool {
    let _ = writeln!(log, "accept this peer? [y/N]: ");
    let mut reader = io::BufReader::new(stdin);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() { return false; }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[allow(clippy::cast_possible_truncation)]
fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

fn now_iso8601() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    // cast_possible_wrap: intentional — dates past year 292_277_026_596 are not a concern.
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
