//! LAN discovery + first-time pairing.
//!
//! Two binaries (`server-deck` on the Deck, `client-win` on Windows) run
//! the same beacon: sign a small UDP packet with our Ed25519 key, broadcast
//! it on the data port, and listen for the peer's matching announce. After a
//! one-shot `pair` flow with mutual button-press confirmation, both ends
//! persist the peer's pubkey to a trusted-peers file, derive a shared HMAC
//! key via HKDF, and the existing per-packet auth in `deck-protocol::auth`
//! takes over.

pub mod beacon;
pub mod crypto;
pub mod identity;
pub mod packet;
pub mod pair;
pub mod state_dir;
pub mod trust;
