//! The headless daemon: discovery beacon + bind/unbind state machine.
//!
//! Invoked by the GUI as `sudo -n network-deck daemon ...`, or directly for
//! debugging. Catches SIGINT/SIGTERM/SIGHUP to unbind the controller and
//! clear the status file before exiting.

use std::net::{Ipv4Addr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use discovery::{BEACON_PORT, DECK_PID, DECK_VID};

use crate::connection::{Action, Connection, RealRunner, State};
use crate::control;
use crate::firewall::PeerLock;
use crate::inhibit::IdleInhibit;
use crate::sysfs::find_deck_busid;

const TICK_INTERVAL: Duration = Duration::from_millis(500);

pub struct Args {
    pub state_dir: PathBuf,
    pub sysfs_root: PathBuf,
    pub control_dir: PathBuf,
}

#[allow(clippy::too_many_lines)] // run() is a top-level boot sequence; splitting it costs more readability than it saves
pub fn run(args: Args) {
    // Mirror the kiosk's boot log: print resolved env up front so we can
    // diagnose path mismatches between GUI and daemon (different
    // control_dir, state_dir, etc.) without re-running by hand.
    eprintln!(
        "daemon boot: USER={} SUDO_USER={} HOME={} XDG_RUNTIME_DIR={}",
        std::env::var("USER").unwrap_or_else(|_| "<unset>".into()),
        std::env::var("SUDO_USER").unwrap_or_else(|_| "<unset>".into()),
        std::env::var("HOME").unwrap_or_else(|_| "<unset>".into()),
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "<unset>".into()),
    );
    eprintln!("daemon boot: control_dir = {}", args.control_dir.display());
    eprintln!("daemon boot: state_dir   = {}", args.state_dir.display());
    eprintln!("daemon boot: sysfs_root  = {}", args.sysfs_root.display());

    let identity = Arc::new(
        discovery::identity::load_or_generate(&args.state_dir).unwrap_or_else(|e| {
            eprintln!("identity load: {e:?}");
            std::process::exit(1);
        }),
    );

    let trusted = match discovery::trust::load(&args.state_dir) {
        Ok(Some(p)) => Arc::new(p),
        Ok(None) => {
            eprintln!("no trusted peer; run `network-deck pair` first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("trust load: {e:?}");
            std::process::exit(1);
        }
    };

    let busid = find_deck_busid(&args.sysfs_root, DECK_VID, DECK_PID).unwrap_or_else(|e| {
        eprintln!(
            "could not find Steam Deck controller (VID {DECK_VID} PID {DECK_PID}) in sysfs: {e:?}"
        );
        std::process::exit(1);
    });
    eprintln!("found Deck controller at busid {busid}");

    let bound = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).unwrap_or_else(|e| {
        eprintln!("bind 0.0.0.0:{BEACON_PORT}: {e}");
        std::process::exit(1);
    });
    // Read timeout so a persistent recv error (interface gone, socket
    // killed) wakes up regularly instead of busy-looping. Also lets the
    // recv thread observe `term` and exit cleanly on shutdown.
    let _ = bound.set_read_timeout(Some(Duration::from_millis(500)));

    let beacon = Arc::new(
        discovery::Beacon::new(
            identity.clone(),
            trusted.clone(),
            discovery::netifs::broadcast_targets(BEACON_PORT),
            super::hostname(),
            BEACON_PORT,
        )
        .unwrap_or_else(|e| {
            eprintln!("beacon init: {e:?}");
            std::process::exit(1);
        }),
    );
    discovery::beacon::spawn_broadcast(beacon.clone());

    let term = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(sig, term.clone());
    }

    // Beacon recv thread: drains incoming beacons so live-peer state updates.
    // Read timeout (set above) periodically wakes the loop so it can observe
    // `term` and exit cleanly. WouldBlock / TimedOut on Linux is just the
    // timeout firing — not an error to log.
    let beacon_recv = beacon.clone();
    let term_recv = term.clone();
    std::thread::Builder::new()
        .name("discovery-recv".into())
        .spawn(move || {
            let mut buf = [0_u8; discovery::packet::PACKET_LEN];
            while !term_recv.load(Ordering::Relaxed) {
                match bound.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        if n >= 4 && buf[0..4] == discovery::BEACON_MAGIC {
                            beacon_recv.handle_packet(src, &buf[..n]);
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        eprintln!("discovery-recv: {e}; sleeping 1s");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })
        .ok();

    eprintln!(
        "supervising bind state for busid {busid} -> peer {} (fingerprint {})",
        trusted.name,
        identity.fingerprint_str(),
    );

    match std::fs::create_dir_all(&args.control_dir) {
        Ok(()) => eprintln!(
            "control_dir ready: {} (will write status.json here every tick)",
            args.control_dir.display(),
        ),
        Err(e) => eprintln!(
            "create_dir_all {}: {e} — kiosk will see 'no status.json' until this is fixed",
            args.control_dir.display(),
        ),
    }

    // Spawn the hotkey listener — toggles `paused` on Steam+QAM or Vol±.
    super::hotkey::spawn(args.control_dir.clone());

    let mut conn = Connection::new(busid.clone());
    let mut runner = RealRunner::new();
    // Tracks the firewall rule lifetime; populated on Action::Bind, dropped
    // on Action::Unbind or when the daemon exits.
    let mut peer_lock: Option<PeerLock> = None;
    // Logind idle:sleep inhibitor lifetime; same shape as peer_lock. Held
    // only while Bound so a paused / unattached Deck still suspends normally.
    let mut idle_inhibit: Option<IdleInhibit> = None;

    eprintln!("entering tick loop (interval = {} ms)", TICK_INTERVAL.as_millis());
    let mut last_status: Option<control::Status> = None;
    let mut wrote_first_status = false;
    while !term.load(Ordering::Relaxed) {
        let beacon_present = beacon
            .current_peer_with_age()
            .is_some_and(|(_, age)| age <= discovery::beacon::STALE_AFTER);
        let paused = control::is_paused(&args.control_dir);
        let effective_peer_present = beacon_present && !paused;
        if let Some(action) = conn.tick(effective_peer_present, &mut runner) {
            eprintln!("connection: {action:?} (state={:?})", conn.state());
            apply_firewall(action.clone(), &beacon, &mut peer_lock);
            apply_inhibit(action, &mut idle_inhibit);
        }
        // Refresh the firewall rule if the peer's DHCP lease renewed under
        // us. Without this the rule still ACCEPTs the old IP and DROPs the
        // new one, so the kiosk reads "Connected" while Windows can't
        // attach. Only act when we have a fresh, definitively different IP
        // — a transient "no IPv4 from beacon" must not churn the rule.
        if matches!(conn.state(), State::Bound) {
            if let (Some(lock), Some(new_ip)) = (peer_lock.as_ref(), current_peer_ipv4(&beacon)) {
                let old_ip = lock.peer();
                if old_ip != new_ip {
                    eprintln!(
                        "firewall: peer IP changed {old_ip} -> {new_ip}, refreshing rule"
                    );
                    // Drop first so the old rule is uninstalled before the
                    // new one goes in — avoids two ACCEPTs on different IPs
                    // simultaneously when the backend is iptables.
                    peer_lock = None;
                    match PeerLock::install(new_ip) {
                        Ok(lock) => peer_lock = lock,
                        Err(e) => eprintln!("firewall: refresh install failed: {e}"),
                    }
                }
            }
        }
        let bind_error = crate::bind_error::from_failure_count(conn.consecutive_bind_failures());
        let status = control::Status {
            peer_name: Some(trusted.name.clone()),
            peer_present: beacon_present,
            bound: matches!(conn.state(), State::Bound),
            paused,
            bind_error,
        };
        // Skip the atomic-rename dance when nothing changed — saves eMMC
        // wear and the kiosk reader's no-op JSON parse on every tick.
        if last_status.as_ref() != Some(&status) {
            match control::write_status(&args.control_dir, &status) {
                Ok(()) => {
                    if !wrote_first_status {
                        eprintln!(
                            "control: first status.json written at {}",
                            args.control_dir.join("status.json").display(),
                        );
                        wrote_first_status = true;
                    }
                    last_status = Some(status);
                }
                // Leave last_status untouched so we retry next tick — otherwise
                // a transient ENOSPC / EROFS pins the kiosk on a stale view.
                Err(e) => eprintln!("control: write_status failed: {e}"),
            }
        }
        std::thread::sleep(TICK_INTERVAL);
    }

    eprintln!("shutdown signal — unbinding");
    let _ = conn.tick(false, &mut runner);
    drop(peer_lock);
    drop(idle_inhibit);
    let _ = control::clear_status(&args.control_dir);
}

fn current_peer_ipv4(beacon: &discovery::Beacon) -> Option<std::net::Ipv4Addr> {
    match beacon.current_peer()? {
        std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
        std::net::SocketAddr::V6(_) => None,
    }
}

fn apply_firewall(
    action: Action,
    beacon: &discovery::Beacon,
    peer_lock: &mut Option<PeerLock>,
) {
    match action {
        Action::Bind => {
            if let Some(peer_ip) = current_peer_ipv4(beacon) {
                match PeerLock::install(peer_ip) {
                    Ok(lock) => *peer_lock = lock,
                    Err(e) => eprintln!("firewall: {e}"),
                }
            } else {
                eprintln!("firewall: no IPv4 peer at bind time; skipping peer-lock");
            }
        }
        Action::Unbind => {
            *peer_lock = None;
        }
    }
}

fn apply_inhibit(action: Action, idle_inhibit: &mut Option<IdleInhibit>) {
    match action {
        Action::Bind => {
            *idle_inhibit = IdleInhibit::acquire();
        }
        Action::Unbind => {
            *idle_inhibit = None;
        }
    }
}

pub fn run_pair(state_dir: &std::path::Path) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind for pair: {e}");
            std::process::exit(1);
        }
    };
    sock.set_broadcast(true).ok();
    let identity = Arc::new(
        discovery::identity::load_or_generate(state_dir).unwrap_or_else(|e| {
            eprintln!("identity load: {e:?}");
            std::process::exit(1);
        }),
    );
    let cfg = discovery::pair::PairConfig {
        identity: identity.clone(),
        recv_sock: sock,
        targets: discovery::netifs::broadcast_targets(BEACON_PORT),
        self_name: super::hostname(),
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(120),
    };
    eprintln!(
        "pairing — fingerprint {}; waiting up to 120 s",
        identity.fingerprint_str()
    );
    let mut stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    match discovery::pair::run_pair(&cfg, &mut stdin, &mut stderr) {
        discovery::pair::PairOutcome::Paired(p) => {
            eprintln!("paired with {} (fingerprint stored)", p.name);
        }
        other => {
            eprintln!("pair did not complete: {other:?}");
            std::process::exit(1);
        }
    }
}
