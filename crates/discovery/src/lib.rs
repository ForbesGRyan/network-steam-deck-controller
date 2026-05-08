//! LAN discovery + first-time pairing.
//!
//! Two binaries (`network-deck` on the Deck, `client-win` on Windows) run
//! the same beacon: sign a small UDP packet with our Ed25519 key, broadcast
//! it on the data port, and listen for the peer's matching announce. After a
//! one-shot `pair` flow with mutual button-press confirmation, both ends
//! persist the peer's pubkey to a trusted-peers file, derive a shared HMAC
//! key via HKDF, and the existing per-packet auth in `deck-protocol::auth`
//! takes over.

pub mod beacon;
pub mod crypto;
pub mod identity;
pub mod netifs;
pub mod packet;
pub mod pair;
pub mod state_dir;
pub mod trust;

pub use beacon::Beacon;
pub use identity::{load_or_generate as load_or_generate_identity, Identity};
pub use packet::MAGIC as BEACON_MAGIC;
pub use pair::{run_pair_with_candidates, PairCandidate};
pub use trust::{load as load_trust, save as save_trust, TrustedPeer};
