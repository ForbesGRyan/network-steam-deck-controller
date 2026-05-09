//! Per-peer firewall rules pinning `usbipd` (TCP 3240) to the currently
//! paired peer.
//!
//! `usbipd` listens on `0.0.0.0:3240` with no authentication of its own —
//! anyone with TCP access can `usbip attach` and steal the controller.
//! The Ed25519 trust file gates the discovery beacon, not the data plane.
//!
//! Strategy: when the daemon transitions Idle → Bound, install an INPUT
//! rule that ACCEPTs only the peer's source IP on dport 3240, plus a
//! catch-all DROP. On Unbind (or daemon shutdown), tear both rules down.
//!
//! Prefers `nft` (atomic table) when present, falls back to `iptables`.
//! If neither is installed, logs a warning and lets traffic through —
//! installing a firewall isn't worth bricking a working install.

use std::net::Ipv4Addr;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "linux")]
use crate::install::absolute_path_for;

const USBIP_PORT: u16 = 3240;
const NFT_TABLE: &str = "network_deck";

/// One of the two supported backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Nft,
    Iptables,
}

/// An installed firewall rule pinning :3240 to a specific peer IP. `Drop`
/// tears the rule down so unbind is unconditional.
pub struct PeerLock {
    backend: Backend,
    tool: PathBuf,
    peer: Ipv4Addr,
}

impl PeerLock {
    /// Install the rule. Returns `Ok(None)` if no firewall tool is present
    /// (logged); `Ok(Some)` if the rule went in; `Err` if the tool ran but
    /// failed (broken iptables service, etc.).
    #[cfg(target_os = "linux")]
    pub fn install(peer: Ipv4Addr) -> std::io::Result<Option<Self>> {
        let Some((backend, tool)) = pick_backend() else {
            eprintln!(
                "firewall: neither nft nor iptables found; usbipd:{USBIP_PORT} \
                 remains world-reachable"
            );
            return Ok(None);
        };

        let ok = match backend {
            Backend::Nft => install_nft(&tool, peer),
            Backend::Iptables => install_iptables(&tool, peer),
        };

        if !ok {
            return Err(std::io::Error::other(format!(
                "failed to install peer-lock rule for {peer} via {backend:?}"
            )));
        }

        eprintln!("firewall: locked usbipd:{USBIP_PORT} to {peer} via {backend:?}");
        Ok(Some(Self { backend, tool, peer }))
    }

    /// IP this lock is currently pinned to. Used by the daemon to detect
    /// peer DHCP-lease renewals so the rule can be refreshed.
    #[must_use]
    pub fn peer(&self) -> Ipv4Addr {
        self.peer
    }

    #[cfg(target_os = "linux")]
    fn uninstall(&self) {
        let ok = match self.backend {
            Backend::Nft => uninstall_nft(&self.tool),
            Backend::Iptables => uninstall_iptables(&self.tool, self.peer),
        };
        if ok {
            eprintln!("firewall: released usbipd:{USBIP_PORT}");
        } else {
            eprintln!(
                "firewall: failed to remove peer-lock rule for {} via {:?}",
                self.peer, self.backend
            );
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PeerLock {
    fn drop(&mut self) {
        self.uninstall();
    }
}

#[cfg(target_os = "linux")]
fn pick_backend() -> Option<(Backend, PathBuf)> {
    if let Some(p) = absolute_path_for("nft") {
        return Some((Backend::Nft, p));
    }
    if let Some(p) = absolute_path_for("iptables") {
        return Some((Backend::Iptables, p));
    }
    None
}

/// Build the nftables script that pins `usbipd` (TCP 3240) to `peer`.
/// Pure — no I/O. The script is fed to `nft -f -` by `install_nft`.
#[must_use]
pub(super) fn nft_script(peer: Ipv4Addr) -> String {
    let table = NFT_TABLE;
    let port = USBIP_PORT;
    format!(
        "table inet {table} {{\n\
            chain input {{\n\
                type filter hook input priority 0; policy accept;\n\
                tcp dport {port} ip saddr {peer} accept\n\
                tcp dport {port} drop\n\
            }}\n\
        }}\n"
    )
}

/// Iptables args for the two `install` invocations, in order: ACCEPT
/// the peer at INPUT slot 1, then DROP everything else at slot 2.
/// Pure — caller is responsible for shelling out.
#[must_use]
pub(super) fn iptables_install_args(peer: Ipv4Addr) -> [Vec<String>; 2] {
    let port = USBIP_PORT.to_string();
    let peer_s = peer.to_string();
    [
        vec![
            "-I".into(), "INPUT".into(), "1".into(),
            "-p".into(), "tcp".into(),
            "--dport".into(), port.clone(),
            "-s".into(), peer_s,
            "-j".into(), "ACCEPT".into(),
        ],
        vec![
            "-I".into(), "INPUT".into(), "2".into(),
            "-p".into(), "tcp".into(),
            "--dport".into(), port,
            "-j".into(), "DROP".into(),
        ],
    ]
}

/// Iptables args for the two uninstall invocations: -D mirrors of the
/// install pair. Pure.
#[must_use]
pub(super) fn iptables_uninstall_args(peer: Ipv4Addr) -> [Vec<String>; 2] {
    let port = USBIP_PORT.to_string();
    let peer_s = peer.to_string();
    [
        vec![
            "-D".into(), "INPUT".into(),
            "-p".into(), "tcp".into(),
            "--dport".into(), port.clone(),
            "-s".into(), peer_s,
            "-j".into(), "ACCEPT".into(),
        ],
        vec![
            "-D".into(), "INPUT".into(),
            "-p".into(), "tcp".into(),
            "--dport".into(), port,
            "-j".into(), "DROP".into(),
        ],
    ]
}

/// nftables: build a fresh inet table with one allow + default drop. Use
/// `add` semantics so re-running is safe even if the table already exists,
/// and tear down by deleting the whole table on unbind.
#[cfg(target_os = "linux")]
fn install_nft(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    // Best-effort cleanup if a stale table from a previous bind survived.
    let _ = run(tool, &["delete", "table", "inet", NFT_TABLE]);
    let script = nft_script(peer);
    Command::new(tool)
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut s) = c.stdin.take() {
                s.write_all(script.as_bytes())?;
            }
            c.wait()
        })
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn uninstall_nft(tool: &PathBuf) -> bool {
    run(tool, &["delete", "table", "inet", NFT_TABLE])
}

/// iptables: insert at the top of INPUT so we beat any pre-existing
/// permissive rule, then tear down by deleting the same rule.
#[cfg(target_os = "linux")]
fn install_iptables(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    let [allow_args, deny_args] = iptables_install_args(peer);
    let allow_refs: Vec<&str> = allow_args.iter().map(String::as_str).collect();
    let deny_refs: Vec<&str> = deny_args.iter().map(String::as_str).collect();
    let allow = run(tool, &allow_refs);
    let deny = run(tool, &deny_refs);
    if !(allow && deny) {
        // Best-effort rollback so we don't leave half a rule behind.
        uninstall_iptables(tool, peer);
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
fn uninstall_iptables(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    let [a_args, b_args] = iptables_uninstall_args(peer);
    let a_refs: Vec<&str> = a_args.iter().map(String::as_str).collect();
    let b_refs: Vec<&str> = b_args.iter().map(String::as_str).collect();
    let a = run(tool, &a_refs);
    let b = run(tool, &b_refs);
    a && b
}

#[cfg(target_os = "linux")]
fn run(tool: &PathBuf, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_accessor_returns_install_ip() {
        // Hand-construct a PeerLock — install() shells out to nft/iptables,
        // so we can't exercise the real construction in a unit test. The
        // accessor is dumb glue, but the daemon's IP-change refresh logic
        // depends on it returning what install() recorded.
        let lock = PeerLock {
            backend: Backend::Iptables,
            tool: PathBuf::from("/bin/true"),
            peer: Ipv4Addr::new(192, 168, 1, 42),
        };
        assert_eq!(lock.peer(), Ipv4Addr::new(192, 168, 1, 42));
        // Don't run uninstall on Drop — would shell out and fail.
        std::mem::forget(lock);
    }

    #[test]
    fn nft_script_pins_port_and_peer() {
        let script = nft_script(Ipv4Addr::new(192, 168, 1, 42));
        assert!(script.contains("table inet network_deck"), "got: {script}");
        assert!(script.contains("type filter hook input priority 0"));
        assert!(script.contains("tcp dport 3240 ip saddr 192.168.1.42 accept"));
        assert!(script.contains("tcp dport 3240 drop"));
    }

    #[test]
    fn nft_script_orders_accept_before_drop() {
        let script = nft_script(Ipv4Addr::new(10, 0, 0, 1));
        let accept_pos = script.find("ip saddr 10.0.0.1 accept").expect("accept rule present");
        let drop_pos = script.find("dport 3240 drop").expect("drop rule present");
        assert!(
            accept_pos < drop_pos,
            "ACCEPT must come before DROP so the peer's traffic isn't blocked"
        );
    }

    #[test]
    fn iptables_install_inserts_accept_at_slot_1_then_drop_at_slot_2() {
        let [allow, deny] = iptables_install_args(Ipv4Addr::new(192, 168, 1, 42));
        assert_eq!(
            allow,
            vec![
                "-I", "INPUT", "1",
                "-p", "tcp",
                "--dport", "3240",
                "-s", "192.168.1.42",
                "-j", "ACCEPT",
            ]
        );
        assert_eq!(
            deny,
            vec![
                "-I", "INPUT", "2",
                "-p", "tcp",
                "--dport", "3240",
                "-j", "DROP",
            ]
        );
    }

    #[test]
    fn iptables_uninstall_mirrors_install_with_minus_d() {
        let peer = Ipv4Addr::new(10, 0, 0, 7);
        let [install_allow, install_deny] = iptables_install_args(peer);
        let [uninstall_allow, uninstall_deny] = iptables_uninstall_args(peer);

        // -I INPUT 1 ... -> -D INPUT ... (no slot number on delete)
        assert_eq!(install_allow[0], "-I");
        assert_eq!(uninstall_allow[0], "-D");
        // The match-spec part (everything after the slot index) must be identical
        // so iptables can find and delete the rule we installed.
        assert_eq!(&install_allow[3..], &uninstall_allow[2..]);
        assert_eq!(install_deny[0], "-I");
        assert_eq!(uninstall_deny[0], "-D");
        assert_eq!(&install_deny[3..], &uninstall_deny[2..]);
    }

    #[test]
    fn iptables_args_use_peer_string_form() {
        // Validates that the rule binds to the textual IP, not a numeric form,
        // since iptables expects a CIDR/host string.
        let [allow, _] = iptables_install_args(Ipv4Addr::new(255, 254, 253, 252));
        let s_idx = allow.iter().position(|s| s == "-s").expect("-s flag present");
        assert_eq!(allow[s_idx + 1], "255.254.253.252");
    }
}
