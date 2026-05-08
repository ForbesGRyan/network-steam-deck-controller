//! Integration test: two `run_pair` calls communicate over a local UDP
//! pair (each binds an ephemeral port; broadcasts redirected via the test
//! harness to the other side's address). Both accept => both write trust
//! files. One declines => neither writes.

use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use discovery::identity::load_or_generate;
use discovery::pair::{run_pair, PairConfig, PairOutcome};
use tempfile::tempdir;

fn ephemeral() -> SocketAddr { (Ipv4Addr::LOCALHOST, 0).into() }

#[test]
fn mutual_accept_writes_both_trust_files() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let id_a = Arc::new(load_or_generate(dir_a.path()).unwrap());
    let id_b = Arc::new(load_or_generate(dir_b.path()).unwrap());

    let sock_a = UdpSocket::bind(ephemeral()).unwrap();
    let sock_b = UdpSocket::bind(ephemeral()).unwrap();
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    let stdin_a = Cursor::new(b"y\n".to_vec());
    let stdin_b = Cursor::new(b"y\n".to_vec());

    let cfg_a = PairConfig {
        identity: id_a.clone(),
        recv_sock: sock_a,
        targets: vec![addr_b],
        self_name: "a".into(),
        state_dir: dir_a.path().to_path_buf(),
        timeout: Duration::from_secs(10),
    };
    let cfg_b = PairConfig {
        identity: id_b.clone(),
        recv_sock: sock_b,
        targets: vec![addr_a],
        self_name: "b".into(),
        state_dir: dir_b.path().to_path_buf(),
        timeout: Duration::from_secs(10),
    };

    let h_a = std::thread::spawn(move || run_pair(&cfg_a, &mut stdin_a.clone(), &mut Vec::new()));
    let h_b = std::thread::spawn(move || run_pair(&cfg_b, &mut stdin_b.clone(), &mut Vec::new()));
    let out_a = h_a.join().unwrap();
    let out_b = h_b.join().unwrap();
    assert!(matches!(out_a, PairOutcome::Paired(_)));
    assert!(matches!(out_b, PairOutcome::Paired(_)));

    assert!(discovery::trust::load(dir_a.path()).unwrap().is_some());
    assert!(discovery::trust::load(dir_b.path()).unwrap().is_some());
}

#[test]
fn one_side_declines_neither_writes_trust() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let id_a = Arc::new(load_or_generate(dir_a.path()).unwrap());
    let id_b = Arc::new(load_or_generate(dir_b.path()).unwrap());

    let sock_a = UdpSocket::bind(ephemeral()).unwrap();
    let sock_b = UdpSocket::bind(ephemeral()).unwrap();
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    let cfg_a = PairConfig {
        identity: id_a.clone(),
        recv_sock: sock_a,
        targets: vec![addr_b],
        self_name: "a".into(),
        state_dir: dir_a.path().to_path_buf(),
        timeout: Duration::from_secs(3),
    };
    let cfg_b = PairConfig {
        identity: id_b.clone(),
        recv_sock: sock_b,
        targets: vec![addr_a],
        self_name: "b".into(),
        state_dir: dir_b.path().to_path_buf(),
        timeout: Duration::from_secs(3),
    };

    let stdin_a = Cursor::new(b"n\n".to_vec());
    let stdin_b = Cursor::new(b"y\n".to_vec());
    let h_a = std::thread::spawn(move || run_pair(&cfg_a, &mut stdin_a.clone(), &mut Vec::new()));
    let h_b = std::thread::spawn(move || run_pair(&cfg_b, &mut stdin_b.clone(), &mut Vec::new()));
    let _ = h_a.join().unwrap();
    let _ = h_b.join().unwrap();

    assert!(discovery::trust::load(dir_a.path()).unwrap().is_none());
    assert!(discovery::trust::load(dir_b.path()).unwrap().is_none());
}

#[test]
fn timeout_with_no_peer_writes_no_trust() {
    let dir_a = tempdir().unwrap();
    let id_a = Arc::new(load_or_generate(dir_a.path()).unwrap());
    let sock_a = UdpSocket::bind(ephemeral()).unwrap();
    // Port 1 is reserved; no one is listening. On POSIX the send succeeds
    // and recv times out; on Windows the OS may return an ICMP-unreachable
    // error on recv_from. Either way no trust file should be written.
    let nowhere: SocketAddr = "127.0.0.1:1".parse().unwrap();

    let stdin_a = Cursor::new(b"y\n".to_vec());
    let cfg_a = PairConfig {
        identity: id_a,
        recv_sock: sock_a,
        targets: vec![nowhere],
        self_name: "a".into(),
        state_dir: dir_a.path().to_path_buf(),
        timeout: Duration::from_secs(1),
    };
    let out = run_pair(&cfg_a, &mut stdin_a.clone(), &mut Vec::new());
    // Accept Timeout OR IoError — either means no peer was found and no trust
    // was committed. The difference is OS-level ICMP behaviour.
    assert!(
        matches!(out, PairOutcome::Timeout | PairOutcome::IoError(_)),
        "expected Timeout or IoError, got {out:?}",
    );
    assert!(discovery::trust::load(dir_a.path()).unwrap().is_none());
}
