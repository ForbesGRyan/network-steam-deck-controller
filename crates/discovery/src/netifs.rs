//! IPv4 broadcast target enumeration.
//!
//! Limited broadcast `255.255.255.255` is unreliable on multi-homed Windows
//! hosts: the OS picks a single egress interface (often a virtual one —
//! Hyper-V, WSL, VPN), so the packet never hits the LAN. We enumerate every
//! non-loopback IPv4 interface and compute its subnet-directed broadcast,
//! then send to each. `255.255.255.255` is appended as a last-resort fallback.

use std::net::{Ipv4Addr, SocketAddr};

/// IPv4 broadcast destinations for `port`, one per local interface plus a
/// limited-broadcast fallback. Interfaces without a usable mask are skipped.
#[must_use]
pub fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() { continue; }
            let if_addrs::IfAddr::V4(v4) = iface.addr else { continue };
            let Some(bcast) = directed_broadcast(v4.ip, v4.netmask) else { continue };
            let target = SocketAddr::new(bcast.into(), port);
            if !out.contains(&target) { out.push(target); }
        }
    }
    let limited = SocketAddr::new(Ipv4Addr::BROADCAST.into(), port);
    if !out.contains(&limited) { out.push(limited); }
    out
}

/// Directed broadcast for `ip` under `mask`: `ip | !mask`. Returns `None`
/// for `/32` (no host bits) or all-ones masks.
fn directed_broadcast(ip: Ipv4Addr, mask: Ipv4Addr) -> Option<Ipv4Addr> {
    let ip_u = u32::from(ip);
    let mask_u = u32::from(mask);
    if mask_u == u32::MAX { return None; }
    Some(Ipv4Addr::from(ip_u | !mask_u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_broadcast_24() {
        let bcast = directed_broadcast(
            Ipv4Addr::new(192, 168, 1, 42),
            Ipv4Addr::new(255, 255, 255, 0),
        );
        assert_eq!(bcast, Some(Ipv4Addr::new(192, 168, 1, 255)));
    }

    #[test]
    fn directed_broadcast_16() {
        let bcast = directed_broadcast(
            Ipv4Addr::new(10, 0, 5, 7),
            Ipv4Addr::new(255, 255, 0, 0),
        );
        assert_eq!(bcast, Some(Ipv4Addr::new(10, 0, 255, 255)));
    }

    #[test]
    fn directed_broadcast_32_returns_none() {
        let bcast = directed_broadcast(Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::BROADCAST);
        assert_eq!(bcast, None);
    }

    #[test]
    fn broadcast_targets_always_includes_limited() {
        let targets = broadcast_targets(49152);
        let limited: SocketAddr = "255.255.255.255:49152".parse().unwrap();
        assert!(targets.contains(&limited));
    }
}
