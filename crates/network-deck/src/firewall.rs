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
use std::process::Command;

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

impl Drop for PeerLock {
    fn drop(&mut self) {
        self.uninstall();
    }
}

fn pick_backend() -> Option<(Backend, PathBuf)> {
    if let Some(p) = absolute_path_for("nft") {
        return Some((Backend::Nft, p));
    }
    if let Some(p) = absolute_path_for("iptables") {
        return Some((Backend::Iptables, p));
    }
    None
}

/// nftables: build a fresh inet table with one allow + default drop. Use
/// `add` semantics so re-running is safe even if the table already exists,
/// and tear down by deleting the whole table on unbind.
fn install_nft(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    // Best-effort cleanup if a stale table from a previous bind survived.
    let _ = run(tool, &["delete", "table", "inet", NFT_TABLE]);
    let table = NFT_TABLE;
    let port = USBIP_PORT;
    let script = format!(
        "table inet {table} {{\n\
            chain input {{\n\
                type filter hook input priority 0; policy accept;\n\
                tcp dport {port} ip saddr {peer} accept\n\
                tcp dport {port} drop\n\
            }}\n\
        }}\n"
    );
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

fn uninstall_nft(tool: &PathBuf) -> bool {
    run(tool, &["delete", "table", "inet", NFT_TABLE])
}

/// iptables: insert at the top of INPUT so we beat any pre-existing
/// permissive rule, then tear down by deleting the same rule.
fn install_iptables(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    let port = USBIP_PORT.to_string();
    let peer_s = peer.to_string();
    let allow = run(
        tool,
        &["-I", "INPUT", "1", "-p", "tcp", "--dport", &port, "-s", &peer_s, "-j", "ACCEPT"],
    );
    let deny = run(
        tool,
        &["-I", "INPUT", "2", "-p", "tcp", "--dport", &port, "-j", "DROP"],
    );
    if !(allow && deny) {
        // Best-effort rollback so we don't leave half a rule behind.
        uninstall_iptables(tool, peer);
        return false;
    }
    true
}

fn uninstall_iptables(tool: &PathBuf, peer: Ipv4Addr) -> bool {
    let port = USBIP_PORT.to_string();
    let peer_s = peer.to_string();
    let a = run(
        tool,
        &["-D", "INPUT", "-p", "tcp", "--dport", &port, "-s", &peer_s, "-j", "ACCEPT"],
    );
    let b = run(
        tool,
        &["-D", "INPUT", "-p", "tcp", "--dport", &port, "-j", "DROP"],
    );
    a && b
}

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
}
