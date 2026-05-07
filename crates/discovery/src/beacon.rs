//! Beacon runtime: send signed announces, accept incoming announces from
//! the trusted peer, expose the live peer address + session key to the data
//! plane.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::crypto::{derive_session_key, SESSION_KEY_LEN};
use crate::identity::Identity;
use crate::packet::{
    self, fingerprint, BeaconPacket, FPR_LEN, PACKET_LEN,
};
use crate::trust::TrustedPeer;
use deck_protocol::auth::REPLAY_WINDOW_US;

pub const BEACON_INTERVAL: Duration = Duration::from_secs(1);
pub const STALE_AFTER: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct LivePeer {
    pub addr: SocketAddr,
    pub last_seen: Instant,
}

#[derive(Default)]
struct PeerState { live: Option<LivePeer> }

/// Runtime state shared between the broadcast tick, the recv-callback, and
/// the data plane's `current_peer()` reader.
pub struct Beacon {
    identity: Arc<Identity>,
    peer: Arc<TrustedPeer>,
    state: Arc<Mutex<PeerState>>,
    send_sock: UdpSocket,
    broadcast_dest: SocketAddr,
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
        broadcast_dest: SocketAddr,
        self_name: String,
        listen_port: u16,
    ) -> Result<Self, BeaconError> {
        let send_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        send_sock.set_broadcast(true)?;
        let session_key = derive_session_key(&identity.pubkey, &peer.pubkey);
        Ok(Self {
            identity, peer,
            state: Arc::new(Mutex::new(PeerState::default())),
            send_sock,
            broadcast_dest,
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
        self.send_sock.send_to(&buf, self.broadcast_dest).map(|_| ())
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
        if !deck_protocol::auth::is_within_replay_window(pkt32, now32, REPLAY_WINDOW_US) { return; }
        // Normalize src to the data-plane listen port (fix #1: port mismatch).
        // The sender's beacon comes from an ephemeral send socket; the actual
        // data-plane port we must reach is self.listen_port.
        let normalized = SocketAddr::new(src.ip(), self.listen_port);
        if let Ok(mut s) = self.state.lock() {
            s.live = Some(LivePeer { addr: normalized, last_seen: Instant::now() });
        }
    }

    /// Current live peer address if a beacon was seen recently.
    #[must_use]
    pub fn current_peer(&self) -> Option<SocketAddr> {
        self.state.lock().ok().and_then(|s| s.live.map(|l| l.addr))
    }

    #[must_use]
    pub fn current_peer_with_age(&self) -> Option<(SocketAddr, Duration)> {
        self.state.lock().ok().and_then(|s| {
            s.live.map(|l| (l.addr, l.last_seen.elapsed()))
        })
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

#[allow(clippy::cast_possible_truncation)]
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
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
        let beacon = Beacon::new(me, peer.clone(), dest, "me".into(), 49152).unwrap();

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
        let beacon = Beacon::new(me, peer, dest, "me".into(), 49152).unwrap();

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
        let beacon = Beacon::new(me, peer.clone(), dest, "me".into(), 49152).unwrap();
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
        let beacon = Beacon::new(me, peer.clone(), dest, "me".into(), 49152).unwrap();
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
}
