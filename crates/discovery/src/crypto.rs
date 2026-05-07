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
        // Locks the INFO string + sort order. A future refactor that
        // changes either is caught by an explicit byte mismatch here,
        // not by silent breakage in the field.
        let a = [0x00_u8; 32];
        let b = [0xff_u8; 32];
        let expected: [u8; 32] = [
            0x49, 0x40, 0x4a, 0x8a, 0xa4, 0x9c, 0x44, 0x92,
            0x00, 0x46, 0x49, 0x14, 0xf3, 0x9d, 0x3a, 0xf0,
            0x2e, 0x22, 0xfb, 0x6c, 0x74, 0x93, 0x39, 0x5b,
            0xa5, 0x0e, 0x19, 0xce, 0x24, 0x50, 0x1c, 0xec,
        ];
        assert_eq!(derive_session_key(&a, &b), expected);
        assert_eq!(derive_session_key(&b, &a), expected);
    }
}
