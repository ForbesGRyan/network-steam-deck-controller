//! Integration test: a stranger broadcasting on UDP at our recv socket
//! does not poison the beacon's live-peer slot, even after the trusted
//! peer has already been seen. Mirrors a "second Deck on the LAN"
//! scenario without a second physical Deck.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use discovery::beacon::Beacon;
use discovery::identity::load_or_generate;
use discovery::packet::{self, fingerprint, BeaconPacket, FPR_LEN, PACKET_LEN};
use discovery::trust::TrustedPeer;
use tempfile::tempdir;

fn ephemeral() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

// `discovery::time` is pub(crate); inline an equivalent helper here rather
// than widening the discovery public API for one test.
#[allow(clippy::cast_possible_truncation)]
fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

#[test]
fn stranger_udp_broadcast_does_not_poison_live_peer() {
    let dir_me = tempdir().unwrap();
    let dir_them = tempdir().unwrap();
    let dir_stranger = tempdir().unwrap();
    let me = Arc::new(load_or_generate(dir_me.path()).unwrap());
    let them = Arc::new(load_or_generate(dir_them.path()).unwrap());
    let stranger = Arc::new(load_or_generate(dir_stranger.path()).unwrap());

    let peer = Arc::new(TrustedPeer {
        pubkey: them.pubkey,
        name: "them".into(),
        paired_at: "2026-05-07T00:00:00Z".into(),
        last_seen_addr: None,
    });

    // The data-plane recv socket the beacon advertises.
    let recv = UdpSocket::bind(ephemeral()).unwrap();
    let recv_port = recv.local_addr().unwrap().port();
    recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    let beacon = Beacon::new(
        me.clone(),
        peer.clone(),
        vec![ephemeral()],
        "me".into(),
        recv_port,
    )
    .unwrap();

    // The trusted peer broadcasts.
    let trusted_pkt = BeaconPacket {
        flags: 0,
        pubkey: them.pubkey,
        peer_fpr: fingerprint(&me.pubkey),
        timestamp_us: now_us(),
        name: "them".into(),
    };
    let mut tbuf = [0_u8; PACKET_LEN];
    packet::sign_into(&them.signing, &trusted_pkt, &mut tbuf).unwrap();
    let trusted_send = UdpSocket::bind(ephemeral()).unwrap();
    trusted_send
        .send_to(&tbuf, recv.local_addr().unwrap())
        .unwrap();

    // Receive the trusted packet and feed it to the beacon.
    let mut rbuf = [0_u8; PACKET_LEN];
    let (n, src) = recv.recv_from(&mut rbuf).unwrap();
    assert_eq!(n, PACKET_LEN);
    beacon.handle_packet(src, &rbuf[..n]);
    let trusted_live = beacon.current_peer().expect("trusted peer should be live");

    // Stranger broadcasts from a different ephemeral socket.
    let stranger_pkt = BeaconPacket {
        flags: 0,
        pubkey: stranger.pubkey,
        peer_fpr: [0; FPR_LEN],
        timestamp_us: now_us(),
        name: "stranger".into(),
    };
    let mut sbuf = [0_u8; PACKET_LEN];
    packet::sign_into(&stranger.signing, &stranger_pkt, &mut sbuf).unwrap();
    let stranger_send = UdpSocket::bind(ephemeral()).unwrap();
    stranger_send
        .send_to(&sbuf, recv.local_addr().unwrap())
        .unwrap();

    let (n2, src2) = recv.recv_from(&mut rbuf).unwrap();
    assert_eq!(n2, PACKET_LEN);
    beacon.handle_packet(src2, &rbuf[..n2]);

    // Live peer slot must be unchanged — stranger did not poison it.
    assert_eq!(
        beacon.current_peer(),
        Some(trusted_live),
        "stranger UDP broadcast must not change live peer"
    );
}
