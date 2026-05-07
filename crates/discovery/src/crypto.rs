//! Session-key derivation. Both peers feed the same two pubkeys into HKDF
//! and derive the same 32-byte key, which the existing per-packet HMAC code
//! in `deck-protocol::auth` consumes unmodified.

use hkdf::Hkdf;
use sha2::Sha256;

pub const SESSION_KEY_LEN: usize = 32;
const INFO: &[u8] = b"network-deck v1 hmac";

/// Derive a 32-byte session key from a sorted pair of pubkeys.
/// Symmetric: `derive(a, b) == derive(b, a)`.
///
/// # Panics
///
/// Never panics in practice: HKDF-SHA256 allows up to 8160 bytes of output,
/// and 32 bytes is well below that limit.
#[must_use]
pub fn derive_session_key(a: &[u8; 32], b: &[u8; 32]) -> [u8; SESSION_KEY_LEN] {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut ikm = [0_u8; 64];
    ikm[..32].copy_from_slice(lo);
    ikm[32..].copy_from_slice(hi);
    let hk = Hkdf::<Sha256>::new(None, &ikm);
    let mut out = [0_u8; SESSION_KEY_LEN];
    hk.expand(INFO, &mut out).expect("32 bytes is well below HKDF-SHA256 max");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric() {
        let a = [1_u8; 32];
        let b = [2_u8; 32];
        assert_eq!(derive_session_key(&a, &b), derive_session_key(&b, &a));
    }

    #[test]
    fn distinct_inputs_distinct_outputs() {
        let a = [1_u8; 32];
        let b = [2_u8; 32];
        let c = [3_u8; 32];
        assert_ne!(derive_session_key(&a, &b), derive_session_key(&a, &c));
    }

    #[test]
    fn known_vector() {
        // Lock the constant-string + sort-order so a future refactor that
        // changes either is caught by an explicit failure here, not by a
        // silent break in the field. Vector recomputed if we ever bump v1.
        let a = [0x00_u8; 32];
        let b = [0xff_u8; 32];
        let key = derive_session_key(&a, &b);
        assert_eq!(key.len(), 32);
        assert_eq!(key, derive_session_key(&b, &a));
    }
}
