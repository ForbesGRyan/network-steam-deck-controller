//! On-disk record of the single trusted peer. TOML so it's user-readable.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::packet::PUBKEY_LEN;

const FILENAME: &str = "trusted-peers.toml";

#[derive(Debug)]
pub enum TrustError {
    Io(io::Error),
    Toml(toml::de::Error),
    PubkeyDecode,
    PubkeyLen(usize),
    AddrParse(String),
}

impl From<io::Error> for TrustError { fn from(e: io::Error) -> Self { Self::Io(e) } }
impl From<toml::de::Error> for TrustError { fn from(e: toml::de::Error) -> Self { Self::Toml(e) } }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedPeer {
    pub pubkey: [u8; PUBKEY_LEN],
    pub name: String,
    pub paired_at: String,          // ISO-8601, opaque string
    pub last_seen_addr: Option<SocketAddr>,
}

#[derive(Serialize, Deserialize)]
struct OnDisk { peer: PeerSection }

#[derive(Serialize, Deserialize)]
struct PeerSection {
    pubkey: String,
    name: String,
    paired_at: String,
    #[serde(default)]
    last_seen_addr: Option<String>,
}

/// Load the trusted peer from `state_dir/trusted-peers.toml`.
///
/// # Errors
///
/// Returns `TrustError::Io` if the file cannot be read,
/// `TrustError::Toml` if the file is not valid TOML,
/// `TrustError::PubkeyDecode` if the pubkey is not valid base64,
/// `TrustError::PubkeyLen` if the decoded pubkey is not exactly `PUBKEY_LEN` bytes, or
/// `TrustError::AddrParse` if `last_seen_addr` is not a valid socket address.
pub fn load(state_dir: &Path) -> Result<Option<TrustedPeer>, TrustError> {
    let path = state_dir.join(FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let parsed: OnDisk = toml::from_str(&raw)?;
    let pubkey_bytes = B64.decode(parsed.peer.pubkey.as_bytes())
        .map_err(|_| TrustError::PubkeyDecode)?;
    if pubkey_bytes.len() != PUBKEY_LEN {
        return Err(TrustError::PubkeyLen(pubkey_bytes.len()));
    }
    let mut pubkey = [0_u8; PUBKEY_LEN];
    pubkey.copy_from_slice(&pubkey_bytes);
    let last_seen_addr = parsed.peer.last_seen_addr
        .map(|s| s.parse::<SocketAddr>().map_err(|_| TrustError::AddrParse(s)))
        .transpose()?;
    Ok(Some(TrustedPeer {
        pubkey,
        name: parsed.peer.name,
        paired_at: parsed.peer.paired_at,
        last_seen_addr,
    }))
}

/// Persist `peer` to `state_dir/trusted-peers.toml`, creating `state_dir` if needed.
///
/// # Errors
///
/// Returns `TrustError::Io` if the directory cannot be created or the file cannot be written.
///
/// # Panics
///
/// Never panics in practice. The internal `expect` on TOML serialization is infallible because
/// `OnDisk` contains only `String` and `Option<String>` fields.
pub fn save(state_dir: &Path, peer: &TrustedPeer) -> Result<(), TrustError> {
    fs::create_dir_all(state_dir)?;
    let on_disk = OnDisk {
        peer: PeerSection {
            pubkey: B64.encode(peer.pubkey),
            name: peer.name.clone(),
            paired_at: peer.paired_at.clone(),
            last_seen_addr: peer.last_seen_addr.map(|a| a.to_string()),
        },
    };
    let s = toml::to_string_pretty(&on_disk)
        .expect("serializing TrustedPeer cannot fail");
    fs::write(state_dir.join(FILENAME), s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> TrustedPeer {
        TrustedPeer {
            pubkey: [9_u8; PUBKEY_LEN],
            name: "deck-living-room".into(),
            paired_at: "2026-05-07T19:23:11Z".into(),
            last_seen_addr: Some("192.168.1.42:49152".parse().unwrap()),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempdir().unwrap();
        let p = sample();
        save(dir.path(), &p).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, p);
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn corrupt_toml_returns_error() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FILENAME), "not = valid = toml").unwrap();
        assert!(matches!(load(dir.path()), Err(TrustError::Toml(_))));
    }

    #[test]
    fn save_overwrites() {
        let dir = tempdir().unwrap();
        let mut p = sample();
        save(dir.path(), &p).unwrap();
        p.name = "deck-bedroom".into();
        save(dir.path(), &p).unwrap();
        assert_eq!(load(dir.path()).unwrap().unwrap().name, "deck-bedroom");
    }
}
