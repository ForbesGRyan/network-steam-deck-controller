//! Beacon runtime: send signed announces, accept incoming announces from
//! the trusted peer, expose the live peer address + session key to the data
//! plane.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::crypto::{derive_session_key, SESSION_KEY_LEN};
use crate::identity::Identity;
use crate::packet::{
    self, fingerprint, BeaconPacket, FPR_LEN, PACKET_LEN,
};
use crate::time::now_us;
use crate::trust::TrustedPeer;

/// ±wall-clock skew tolerated for beacon packets, in microseconds.
/// 30 s is short enough to defang a delayed replay, long enough to absorb
/// NTP wobble between two LAN hosts that haven't slewed in a while.
pub const REPLAY_WINDOW_US: u32 = 30_000_000;

/// True if `packet_us` is within `window_us` of `now_us` (wrap-aware).
/// Both timestamps are the low 32 bits of microseconds since some epoch.
#[allow(clippy::cast_possible_wrap)]
#[must_use]
pub fn is_within_replay_window(packet_us: u32, now_us: u32, window_us: u32) -> bool {
    let dt = (now_us as i32).wrapping_sub(packet_us as i32);
    dt.unsigned_abs() <= window_us
}

pub const BEACON_INTERVAL: Duration = Duration::from_secs(1);
pub const STALE_AFTER: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct LivePeer {
    pub addr: SocketAddr,
    pub last_seen: Instant,
}

/// Runtime state shared between the broadcast tick, the recv-callback, and
/// the data plane's `current_peer()` reader.
pub struct Beacon {
    identity: Arc<Identity>,
    peer: Arc<TrustedPeer>,
    live: Arc<Mutex<Option<LivePeer>>>,
    send_sock: UdpSocket,
    broadcast_dests: Vec<SocketAddr>,
    session_key: [u8; SESSION_KEY_LEN],
    self_name: String,
    listen_port: u16,
}

#[derive(Debug)]
pub enum BeaconError { Io(io::Error) }
impl From<io::Error> for BeaconError { fn from(e: io::Error) -> Self { Self::Io(e) } }

impl Beacon {
    /// Open a broadcast-capable ephemeral socket and prepare runtime state.
    ///
    /// # Errors
    /// `BeaconError::Io` if the ephemeral socket cannot be bound or
    /// `set_broadcast(true)` fails.
    pub fn new(
        identity: Arc<Identity>,
        peer: Arc<TrustedPeer>,
        broadcast_dests: Vec<SocketAddr>,
        self_name: String,
        listen_port: u16,
    ) -> Result<Self, BeaconError> {
        let send_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        send_sock.set_broadcast(true)?;
        let session_key = derive_session_key(&identity.pubkey, &peer.pubkey);
        Ok(Self {
            identity, peer,
            live: Arc::new(Mutex::new(None)),
            send_sock,
            broadcast_dests,
            session_key,
            self_name,
            listen_port,
        })
    }

    #[must_use]
    pub fn session_key(&self) -> [u8; SESSION_KEY_LEN] { self.session_key }

    /// Send one signed announce. Called on a 1 s tick by the spawned thread.
    ///
    /// # Errors
    /// I/O errors from the send. Sign failures are wrapped as `io::Error::other`.
    pub fn broadcast_once(&self) -> io::Result<()> {
        let pkt = BeaconPacket {
            flags: 0,
            pubkey: self.identity.pubkey,
            peer_fpr: self.peer_fpr(),
            timestamp_us: now_us(),
            name: self.self_name.clone(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&self.identity.signing, &pkt, &mut buf)
            .map_err(|_| io::Error::other("sign failed"))?;
        let mut last_err: Option<io::Error> = None;
        let mut sent = 0_usize;
        for dest in &self.broadcast_dests {
            match self.send_sock.send_to(&buf, dest) {
                Ok(_) => sent += 1,
                Err(e) => last_err = Some(e),
            }
        }
        if sent > 0 { Ok(()) } else { Err(last_err.unwrap_or_else(|| io::Error::other("no targets"))) }
    }

    /// Handle one inbound beacon packet. Called by the data-plane recv loop
    /// after a magic-demux match. Drops anything not from the trusted peer.
    pub fn handle_packet(&self, src: SocketAddr, bytes: &[u8]) {
        let Ok(decoded) = packet::verify(bytes) else { return };
        if decoded.pubkey != self.peer.pubkey { return; }
        // Drop packets that name a specific peer other than us (fix #2: peer_fpr validation).
        let my_fpr = fingerprint(&self.identity.pubkey);
        let zero = [0_u8; FPR_LEN];
        if decoded.peer_fpr != zero && decoded.peer_fpr != my_fpr { return; }
        #[allow(clippy::cast_possible_truncation)]
        let now32 = now_us() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let pkt32 = decoded.timestamp_us as u32;
        if !is_within_replay_window(pkt32, now32, REPLAY_WINDOW_US) { return; }
        // Normalize src to the data-plane listen port. The sender's beacon
        // comes from an ephemeral send socket; the data-plane port we must
        // reach is self.listen_port.
        let normalized = SocketAddr::new(src.ip(), self.listen_port);
        if let Ok(mut slot) = self.live.lock() {
            *slot = Some(LivePeer { addr: normalized, last_seen: Instant::now() });
        }
    }

    /// Current live peer address if a beacon was seen recently.
    #[must_use]
    pub fn current_peer(&self) -> Option<SocketAddr> {
        self.live.lock().ok().and_then(|s| s.map(|l| l.addr))
    }

    #[must_use]
    pub fn current_peer_with_age(&self) -> Option<(SocketAddr, Duration)> {
        self.live
            .lock()
            .ok()
            .and_then(|s| s.map(|l| (l.addr, l.last_seen.elapsed())))
    }

    fn peer_fpr(&self) -> [u8; FPR_LEN] { fingerprint(&self.peer.pubkey) }
}

/// Spawn the broadcast tick on a dedicated thread.
pub fn spawn_broadcast(beacon: Arc<Beacon>) {
    std::thread::Builder::new()
        .name("discovery-beacon".into())
        .spawn(move || loop {
            let _ = beacon.broadcast_once();
            std::thread::sleep(BEACON_INTERVAL);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::load_or_generate;
    use crate::packet::PUBKEY_LEN;
    use tempfile::tempdir;

    fn make_identity() -> Arc<Identity> {
        let dir = tempdir().unwrap();
        Arc::new(load_or_generate(dir.path()).unwrap())
    }

    fn make_peer(pubkey: [u8; PUBKEY_LEN]) -> Arc<TrustedPeer> {
        Arc::new(TrustedPeer {
            pubkey,
            name: "peer".into(),
            paired_at: "2026-05-07T00:00:00Z".into(),
            last_seen_addr: None,
        })
    }

    #[test]
    fn handle_packet_from_trusted_peer_updates_live() {
        let me = make_identity();
        let them = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();

        let pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt, &mut buf).unwrap();
        let src: SocketAddr = "192.168.1.42:55555".parse().unwrap();
        beacon.handle_packet(src, &buf);

        // Port must be normalized to the listen port, not the ephemeral src port.
        let expected: SocketAddr = "192.168.1.42:49152".parse().unwrap();
        assert_eq!(beacon.current_peer(), Some(expected));
    }

    #[test]
    fn handle_packet_from_stranger_ignored() {
        let me = make_identity();
        let them = make_identity();
        let stranger = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer, vec![dest], "me".into(), 49152).unwrap();

        let pkt = BeaconPacket {
            flags: 0,
            pubkey: stranger.pubkey,
            peer_fpr: [0; FPR_LEN],
            timestamp_us: now_us(),
            name: "stranger".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&stranger.signing, &pkt, &mut buf).unwrap();
        beacon.handle_packet("1.2.3.4:5".parse().unwrap(), &buf);

        assert_eq!(beacon.current_peer(), None);
    }

    #[test]
    fn handle_packet_normalizes_to_listen_port() {
        let me = make_identity();
        let them = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();
        let pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);
        assert_eq!(beacon.current_peer().unwrap().port(), 49152);
    }

    #[test]
    fn handle_packet_wrong_peer_fpr_dropped() {
        let me = make_identity();
        let them = make_identity();
        let third = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();
        // Beacon names the third party's fingerprint — must be dropped even
        // though the packet is properly signed by the trusted peer.
        let pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&third.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);
        assert_eq!(beacon.current_peer(), None);
    }

    #[test]
    fn stranger_does_not_overwrite_active_peer() {
        let me = make_identity();
        let them = make_identity();
        let stranger = make_identity();
        let peer = make_peer(them.pubkey);
        let dest = "127.0.0.1:1".parse().unwrap();
        let beacon = Beacon::new(me, peer.clone(), vec![dest], "me".into(), 49152).unwrap();

        // 1) Trusted peer establishes the live slot.
        let trusted_pkt = BeaconPacket {
            flags: 0,
            pubkey: them.pubkey,
            peer_fpr: fingerprint(&beacon.identity.pubkey),
            timestamp_us: now_us(),
            name: "them".into(),
        };
        let mut buf = [0_u8; PACKET_LEN];
        packet::sign_into(&them.signing, &trusted_pkt, &mut buf).unwrap();
        beacon.handle_packet("192.168.1.42:55555".parse().unwrap(), &buf);
        let trusted_addr = beacon.current_peer().unwrap();
        assert_eq!(trusted_addr.ip().to_string(), "192.168.1.42");

        // 2) Stranger arrives from a different IP. Must be dropped; live slot
        //    must still point at the trusted peer's IP.
        let stranger_pkt = BeaconPacket {
            flags: 0,
            pubkey: stranger.pubkey,
            peer_fpr: [0; FPR_LEN],
            timestamp_us: now_us(),
            name: "stranger".into(),
        };
        let mut sbuf = [0_u8; PACKET_LEN];
        packet::sign_into(&stranger.signing, &stranger_pkt, &mut sbuf).unwrap();
        beacon.handle_packet("10.0.0.99:55555".parse().unwrap(), &sbuf);

        let after = beacon.current_peer().unwrap();
        assert_eq!(after, trusted_addr, "stranger must not overwrite live peer");
    }
}
