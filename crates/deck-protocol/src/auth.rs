//! Per-packet authentication: a 16-byte truncated HMAC-SHA256 tag covering
//! the wire header + body. Keeps casual-LAN attackers off the input stream
//! and out of the rumble channel without needing a TLS-ish handshake.
//!
//! Threat model:
//! - Trusted LAN was the v0 assumption; this lifts it just enough that an
//!   on-link attacker can't synthesize button presses or steal rumble.
//! - Replay protection is layered: per-channel sequence numbers reject
//!   stale packets within a session, and [`is_within_replay_window`] rejects
//!   packets timestamped outside a configurable wall-clock window so a
//!   capture from yesterday can't be re-sent at the next session.
//! - We do **not** defend against an attacker who has the shared key.
//!   That's a key-distribution problem and out of scope.
//!
//! Use:
//! - [`AuthKey::from_hex`] to load a 32-byte key from configuration.
//! - [`compute_tag`] to attach a tag to outgoing packets (caller fills the
//!   trailing 16 bytes of the buffer).
//! - [`verify_tag`] to check incoming packets.
//! - [`is_within_replay_window`] for time-based replay rejection.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Bytes of the auth tag appended to every wire packet.
pub const TAG_LEN: usize = 16;

/// Bytes of an [`AuthKey`].
pub const KEY_LEN: usize = 32;

/// Header-flags bit set by senders that computed a real HMAC tag. Receivers
/// in secure mode (key present) MUST drop packets with this bit clear; in
/// dev mode (no key) the bit is informational. Lives in the packet header's
/// `flags` u16, bit 0.
pub const FLAG_AUTHENTICATED: u16 = 0x0001;

/// Default ±wall-clock skew tolerated by [`is_within_replay_window`].
/// 30 seconds is short enough to defang a delayed replay, long enough to
/// absorb NTP wobble between two LAN hosts that haven't slewed in a while.
pub const REPLAY_WINDOW_US: u32 = 30_000_000;

/// 32-byte symmetric key. Both ends load the same value (e.g. from an env
/// variable) at startup. Cloning is cheap and intentional — the key is
/// shared across the input thread and the output thread.
#[derive(Clone)]
pub struct AuthKey([u8; KEY_LEN]);

impl AuthKey {
    /// Construct from raw bytes. Available outside the crate so callers
    /// can derive the key from an OS-keyring source.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Decode 64 hex chars into a 32-byte key. Permissive about leading /
    /// trailing whitespace (an env-var reader's friend) but strict about
    /// the character set.
    ///
    /// # Errors
    /// [`KeyError::Length`] if the hex string isn't 64 nibble characters
    /// (after trim) and [`KeyError::HexChar`] for any non-hex character.
    pub fn from_hex(hex: &str) -> Result<Self, KeyError> {
        let bytes = hex.as_bytes();
        // Trim ASCII whitespace at both ends in place — saves an allocation
        // for the env-var read path. We only need the trimmed slice.
        let mut start = 0;
        let mut end = bytes.len();
        while start < end && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        while end > start && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let trimmed = &bytes[start..end];
        if trimmed.len() != KEY_LEN * 2 {
            return Err(KeyError::Length(trimmed.len()));
        }
        let mut out = [0_u8; KEY_LEN];
        for (i, chunk) in trimmed.chunks_exact(2).enumerate() {
            let hi = nibble(chunk[0])?;
            let lo = nibble(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    /// Raw key bytes. Exposed for diagnostic logging only — do **not**
    /// print these to a shared console.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

#[inline]
fn nibble(b: u8) -> Result<u8, KeyError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(KeyError::HexChar(b)),
    }
}

/// Errors produced by [`AuthKey::from_hex`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyError {
    /// Input wasn't 64 hex characters (after trimming whitespace).
    Length(usize),
    /// Input contained a non-hex byte.
    HexChar(u8),
}

/// Compute an HMAC-SHA256 over `data` using `key`, returning the leading
/// [`TAG_LEN`] bytes. The caller writes the result into the last 16 bytes
/// of the wire packet.
///
/// # Panics
/// Never. The `expect` is on a constructor that fails only for empty keys,
/// and our key is always [`KEY_LEN`] bytes by construction.
#[must_use]
pub fn compute_tag(key: &AuthKey, data: &[u8]) -> [u8; TAG_LEN] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key.0)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(data);
    let full = mac.finalize().into_bytes();
    let mut out = [0_u8; TAG_LEN];
    out.copy_from_slice(&full[..TAG_LEN]);
    out
}

/// Constant-time check that `tag` is the truncated HMAC of `data` under
/// `key`. Returns `false` for any mismatch (length included).
///
/// # Panics
/// Never. Same reasoning as [`compute_tag`].
#[must_use]
pub fn verify_tag(key: &AuthKey, data: &[u8], tag: &[u8]) -> bool {
    if tag.len() != TAG_LEN {
        return false;
    }
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key.0)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(data);
    // The crate's verify variants want a full 32-byte tag; we want a 16-byte
    // truncated check. Recompute and constant-time compare ourselves.
    let full = mac.finalize().into_bytes();
    constant_time_eq(&full[..TAG_LEN], tag)
}

#[inline]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// True if `packet_us` is within `window_us` of `now_us` (wrap-aware).
///
/// Both timestamps are the low 32 bits of microseconds since some epoch;
/// signed wrap-aware arithmetic handles the case where the field wraps
/// (every ~71 minutes). Tolerates clock skew up to `window_us` in either
/// direction.
#[must_use]
#[allow(clippy::cast_possible_wrap)]
pub fn is_within_replay_window(packet_us: u32, now_us: u32, window_us: u32) -> bool {
    let dt = (now_us as i32).wrapping_sub(packet_us as i32);
    let abs_dt = dt.unsigned_abs();
    abs_dt <= window_us
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AuthKey {
        AuthKey::from_hex(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        )
        .unwrap()
    }

    #[test]
    fn tag_roundtrip() {
        let k = key();
        let data = b"network-deck-test-vector";
        let tag = compute_tag(&k, data);
        assert!(verify_tag(&k, data, &tag));
    }

    #[test]
    fn rejects_modified_data() {
        let k = key();
        let data = b"network-deck-test-vector";
        let tag = compute_tag(&k, data);
        let mut tampered = *data;
        tampered[0] ^= 1;
        assert!(!verify_tag(&k, &tampered, &tag));
    }

    #[test]
    fn rejects_modified_tag() {
        let k = key();
        let data = b"network-deck-test-vector";
        let mut tag = compute_tag(&k, data);
        tag[5] ^= 0xFF;
        assert!(!verify_tag(&k, data, &tag));
    }

    #[test]
    fn rejects_short_tag() {
        let k = key();
        let data = b"network-deck-test-vector";
        let tag = compute_tag(&k, data);
        assert!(!verify_tag(&k, data, &tag[..15]));
    }

    #[test]
    fn key_hex_parse() {
        let k = AuthKey::from_hex(
            "  00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff  ",
        )
        .unwrap();
        assert_eq!(k.as_bytes()[0], 0x00);
        assert_eq!(k.as_bytes()[31], 0xff);
    }

    #[test]
    fn key_hex_uppercase() {
        let k = AuthKey::from_hex(
            "FFEEDDCCBBAA99887766554433221100FFEEDDCCBBAA99887766554433221100",
        )
        .unwrap();
        assert_eq!(k.as_bytes()[0], 0xff);
    }

    #[test]
    fn key_hex_rejects_short() {
        assert!(matches!(
            AuthKey::from_hex("00112233"),
            Err(KeyError::Length(_))
        ));
    }

    #[test]
    fn key_hex_rejects_nonhex() {
        // 64-char string with non-hex chars in the middle. Length check
        // must pass for the digit check to fire.
        assert!(matches!(
            AuthKey::from_hex(
                "001122334455667788ZZ2233445566778899aabbccddeeff00112233aabbccdd"
            ),
            Err(KeyError::HexChar(b'Z'))
        ));
    }

    #[test]
    fn replay_window_in_window() {
        assert!(is_within_replay_window(
            1_000_000,
            1_000_000 + 5_000_000,
            REPLAY_WINDOW_US
        ));
        assert!(is_within_replay_window(
            1_000_000 + 5_000_000,
            1_000_000,
            REPLAY_WINDOW_US
        ));
    }

    #[test]
    fn replay_window_out_of_window() {
        assert!(!is_within_replay_window(
            1_000_000,
            1_000_000 + REPLAY_WINDOW_US + 1,
            REPLAY_WINDOW_US
        ));
    }

    #[test]
    fn replay_window_wrap() {
        // packet sent just before u32 wrap; receiver clock just after.
        let packet_us = u32::MAX - 1_000_000;
        let now_us = 2_000_000;
        // Net delta ~ 3 s — within window.
        assert!(is_within_replay_window(packet_us, now_us, REPLAY_WINDOW_US));
    }
}
