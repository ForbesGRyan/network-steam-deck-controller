//! Network wire format between the Deck server and the Windows client.
//!
//! Two channels share a 16-byte header followed by a channel-specific body
//! and a 16-byte truncated HMAC-SHA256 tag (see [`crate::auth`]). All
//! multi-byte integers are little-endian.
//!
//! Header layout (16 bytes):
//!
//! ```text
//!  0..4   magic            "NUSB"
//!  4      version          [`VERSION`]
//!  5      channel          [`Channel`]
//!  6..8   flags            u16 — bit 0 = AUTHENTICATED (tag is real HMAC)
//!  8..12  sequence         u32, monotonically increasing per channel
//! 12..16  timestamp_us     u32, low 32 bits of sender wall-clock µs
//! ```
//!
//! Input body (Deck → Windows, 44 bytes):
//!
//! ```text
//!  0..4    sequence         u32 from controller firmware
//!  4..12   buttons          u64 (bitflags::Buttons::bits())
//! 12..14   left_stick.x     i16
//! 14..16   left_stick.y     i16
//! 16..18   right_stick.x    i16
//! 18..20   right_stick.y    i16
//! 20..22   left_trigger     u16
//! 22..24   right_trigger    u16
//! 24..26   left_pad.x       i16
//! 26..28   left_pad.y       i16
//! 28..30   right_pad.x      i16
//! 30..32   right_pad.y      i16
//! 32..34   accel.x          i16
//! 34..36   accel.y          i16
//! 36..38   accel.z          i16
//! 38..40   gyro.x           i16
//! 40..42   gyro.y           i16
//! 42..44   gyro.z           i16
//! ```
//!
//! Output body (Windows → Deck, 64 bytes): a raw Steam Deck feature report
//! lifted verbatim from Steam's `SET_REPORT(FEATURE)` on the virtual device.
//! Byte 0 is the Steam Controller `msg_id` (0xEA `TRIGGER_HAPTIC_CMD`, 0xEB
//! `TRIGGER_RUMBLE_CMD`, 0x8F `TRIGGER_HAPTIC_PULSE`, ...). The Deck's
//! hidraw interface speaks the same dialect, so the body is written
//! through unchanged on the Deck side.

use crate::auth::{self, AuthKey, FLAG_AUTHENTICATED, TAG_LEN};
use crate::buttons::Buttons;
use crate::state::{ControllerState, Stick, Trackpad, Vec3i};

/// Magic bytes at the start of every packet. Catches misrouted traffic.
pub const MAGIC: [u8; 4] = *b"NUSB";

/// Wire-format version. Bumped to 2 when the per-packet HMAC tag was added.
pub const VERSION: u8 = 2;

/// Bytes in the shared packet header.
pub const HEADER_LEN: usize = 16;

/// Bytes in an [`Channel::Input`] body.
pub const INPUT_BODY_LEN: usize = 44;

/// Bytes in an [`Channel::Output`] body. Sized to one Steam Deck feature
/// report — the same 64 bytes Steam writes via `SET_REPORT(FEATURE)`.
pub const OUTPUT_BODY_LEN: usize = 64;

/// Total bytes on the wire for an input packet (header + body + auth tag).
pub const INPUT_PACKET_LEN: usize = HEADER_LEN + INPUT_BODY_LEN + TAG_LEN;

/// Total bytes on the wire for an output packet (header + body + auth tag).
pub const OUTPUT_PACKET_LEN: usize = HEADER_LEN + OUTPUT_BODY_LEN + TAG_LEN;

/// Logical channel a packet belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Channel {
    /// Controller state, Deck → Windows. Carried over UDP.
    Input = 1,
    /// Rumble / haptic command, Windows → Deck. Carried over a reliable
    /// transport.
    Output = 2,
}

impl Channel {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Input),
            2 => Some(Self::Output),
            _ => None,
        }
    }
}

/// Decoded packet header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    /// Which channel this packet belongs to.
    pub channel: Channel,
    /// Bitfield. Bit 0 ([`FLAG_AUTHENTICATED`]) is set by senders that
    /// computed a real HMAC tag for this packet.
    pub flags: u16,
    /// Per-channel monotonic sequence number.
    pub sequence: u32,
    /// Sender timestamp, low 32 bits of microseconds.
    pub timestamp_us: u32,
}

/// Errors produced by the wire codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireError {
    /// Buffer was too short for what we tried to read or write.
    Short {
        /// Bytes available.
        got: usize,
        /// Bytes required.
        want: usize,
    },
    /// Magic bytes did not match [`MAGIC`].
    BadMagic,
    /// Version byte did not match [`VERSION`].
    BadVersion(u8),
    /// Channel byte did not map to a known [`Channel`].
    BadChannel(u8),
    /// Receiver had a key but the packet's HMAC tag didn't validate, or
    /// the sender claimed `FLAG_AUTHENTICATED` but no key was configured.
    AuthFailed,
    /// Packet's `timestamp_us` falls outside the configured replay window.
    Replay,
}

#[inline]
fn read_le_i16(buf: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn write_le_i16(buf: &mut [u8], off: usize, val: i16) {
    let bytes = val.to_le_bytes();
    buf[off] = bytes[0];
    buf[off + 1] = bytes[1];
}

#[inline]
fn read_le_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn write_le_u16(buf: &mut [u8], off: usize, val: u16) {
    let bytes = val.to_le_bytes();
    buf[off] = bytes[0];
    buf[off + 1] = bytes[1];
}

/// Encode a packet header into the first 16 bytes of `out`.
///
/// # Errors
/// [`WireError::Short`] if `out.len() < HEADER_LEN`.
pub fn encode_header(hdr: &Header, out: &mut [u8]) -> Result<(), WireError> {
    if out.len() < HEADER_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: HEADER_LEN,
        });
    }
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    out[5] = hdr.channel as u8;
    out[6..8].copy_from_slice(&hdr.flags.to_le_bytes());
    out[8..12].copy_from_slice(&hdr.sequence.to_le_bytes());
    out[12..16].copy_from_slice(&hdr.timestamp_us.to_le_bytes());
    Ok(())
}

/// Decode a packet header from the first 16 bytes of `buf`.
///
/// # Errors
/// - [`WireError::Short`] if `buf.len() < HEADER_LEN`.
/// - [`WireError::BadMagic`] / [`WireError::BadVersion`] /
///   [`WireError::BadChannel`] on mismatch.
pub fn decode_header(buf: &[u8]) -> Result<Header, WireError> {
    if buf.len() < HEADER_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: HEADER_LEN,
        });
    }
    if buf[0..4] != MAGIC {
        return Err(WireError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(WireError::BadVersion(buf[4]));
    }
    let channel = Channel::from_u8(buf[5]).ok_or(WireError::BadChannel(buf[5]))?;
    let flags = u16::from_le_bytes([buf[6], buf[7]]);
    let sequence = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let timestamp_us = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    Ok(Header {
        channel,
        flags,
        sequence,
        timestamp_us,
    })
}

/// Encode a [`ControllerState`] into the first 44 bytes of `out`.
///
/// # Errors
/// [`WireError::Short`] if `out.len() < INPUT_BODY_LEN`.
pub fn encode_input(state: &ControllerState, out: &mut [u8]) -> Result<(), WireError> {
    if out.len() < INPUT_BODY_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: INPUT_BODY_LEN,
        });
    }
    out[0..4].copy_from_slice(&state.sequence.to_le_bytes());
    out[4..12].copy_from_slice(&state.buttons.bits().to_le_bytes());
    write_le_i16(out, 12, state.left_stick.x);
    write_le_i16(out, 14, state.left_stick.y);
    write_le_i16(out, 16, state.right_stick.x);
    write_le_i16(out, 18, state.right_stick.y);
    write_le_u16(out, 20, state.left_trigger);
    write_le_u16(out, 22, state.right_trigger);
    write_le_i16(out, 24, state.left_pad.x);
    write_le_i16(out, 26, state.left_pad.y);
    write_le_i16(out, 28, state.right_pad.x);
    write_le_i16(out, 30, state.right_pad.y);
    write_le_i16(out, 32, state.accel.x);
    write_le_i16(out, 34, state.accel.y);
    write_le_i16(out, 36, state.accel.z);
    write_le_i16(out, 38, state.gyro.x);
    write_le_i16(out, 40, state.gyro.y);
    write_le_i16(out, 42, state.gyro.z);
    Ok(())
}

/// Decode a [`ControllerState`] from the first 44 bytes of `buf`.
///
/// Unknown bits in the buttons field are silently dropped via
/// [`Buttons::from_bits_truncate`] — forward-compatible with senders that
/// add new flags.
///
/// # Errors
/// [`WireError::Short`] if `buf.len() < INPUT_BODY_LEN`.
pub fn decode_input(buf: &[u8]) -> Result<ControllerState, WireError> {
    if buf.len() < INPUT_BODY_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: INPUT_BODY_LEN,
        });
    }
    let sequence = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let buttons_raw = u64::from_le_bytes([
        buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
    ]);
    Ok(ControllerState {
        sequence,
        buttons: Buttons::from_bits_truncate(buttons_raw),
        left_stick: Stick {
            x: read_le_i16(buf, 12),
            y: read_le_i16(buf, 14),
        },
        right_stick: Stick {
            x: read_le_i16(buf, 16),
            y: read_le_i16(buf, 18),
        },
        left_trigger: read_le_u16(buf, 20),
        right_trigger: read_le_u16(buf, 22),
        left_pad: Trackpad {
            x: read_le_i16(buf, 24),
            y: read_le_i16(buf, 26),
        },
        right_pad: Trackpad {
            x: read_le_i16(buf, 28),
            y: read_le_i16(buf, 30),
        },
        accel: Vec3i {
            x: read_le_i16(buf, 32),
            y: read_le_i16(buf, 34),
            z: read_le_i16(buf, 36),
        },
        gyro: Vec3i {
            x: read_le_i16(buf, 38),
            y: read_le_i16(buf, 40),
            z: read_le_i16(buf, 42),
        },
    })
}

/// Encode a raw 64-byte Deck feature report into the first
/// [`OUTPUT_BODY_LEN`] bytes of `out`.
///
/// # Errors
/// [`WireError::Short`] if either side is shorter than [`OUTPUT_BODY_LEN`].
pub fn encode_output(report: &[u8], out: &mut [u8]) -> Result<(), WireError> {
    if report.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: report.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    if out.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    out[..OUTPUT_BODY_LEN].copy_from_slice(&report[..OUTPUT_BODY_LEN]);
    Ok(())
}

/// Decode a raw 64-byte Deck feature report into `out`.
///
/// # Errors
/// [`WireError::Short`] if either side is shorter than [`OUTPUT_BODY_LEN`].
pub fn decode_output(buf: &[u8], out: &mut [u8]) -> Result<(), WireError> {
    if buf.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    if out.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    out[..OUTPUT_BODY_LEN].copy_from_slice(&buf[..OUTPUT_BODY_LEN]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Packet-level helpers: header + body + tag in one shot.
//
// The primitive `encode_header` / `encode_input` / `encode_output` calls are
// still available for callers that want to assemble packets piece-meal, but
// the helpers here are the recommended path. They handle the tag and the
// FLAG_AUTHENTICATED bit so the auth contract stays a property of the wire
// layer instead of leaking into every send / recv site.
// ---------------------------------------------------------------------------

/// Encode an INPUT packet into `out`.
///
/// `out` must be at least [`INPUT_PACKET_LEN`] bytes. If `key` is `Some`,
/// the trailing 16 bytes get the HMAC tag and `FLAG_AUTHENTICATED` is set
/// in the header. If `None`, the trailing 16 bytes are zeroed and the flag
/// is left clear — that's the dev-mode (no key) plaintext format.
///
/// # Errors
/// [`WireError::Short`] if `out.len() < INPUT_PACKET_LEN`.
pub fn encode_input_packet(
    hdr: &Header,
    state: &ControllerState,
    key: Option<&AuthKey>,
    out: &mut [u8],
) -> Result<(), WireError> {
    if out.len() < INPUT_PACKET_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: INPUT_PACKET_LEN,
        });
    }
    let mut hdr = *hdr;
    hdr.flags = if key.is_some() {
        hdr.flags | FLAG_AUTHENTICATED
    } else {
        hdr.flags & !FLAG_AUTHENTICATED
    };
    encode_header(&hdr, &mut out[..HEADER_LEN])?;
    encode_input(state, &mut out[HEADER_LEN..HEADER_LEN + INPUT_BODY_LEN])?;
    apply_tag(key, &mut out[..INPUT_PACKET_LEN]);
    Ok(())
}

/// Decode and authenticate an INPUT packet.
///
/// Returns the parsed header and body. If `key` is `Some`, the packet's tag
/// is verified and the timestamp is checked against the replay window. If
/// `key` is `None`, packets advertising `FLAG_AUTHENTICATED` are still
/// rejected (they were meant for a configured peer); plaintext packets pass.
///
/// # Errors
/// All [`WireError`] variants. Common ones:
/// - [`WireError::AuthFailed`] if the tag is wrong, missing, or the flag bit
///   contradicts the configuration.
/// - [`WireError::Replay`] if `now_us` and the packet's timestamp diverge by
///   more than `replay_window_us`.
pub fn decode_input_packet(
    buf: &[u8],
    key: Option<&AuthKey>,
    now_us: u32,
    replay_window_us: u32,
) -> Result<(Header, ControllerState), WireError> {
    if buf.len() < INPUT_PACKET_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: INPUT_PACKET_LEN,
        });
    }
    let hdr = decode_header(&buf[..HEADER_LEN])?;
    verify_packet(&hdr, key, &buf[..INPUT_PACKET_LEN], now_us, replay_window_us)?;
    let state = decode_input(&buf[HEADER_LEN..HEADER_LEN + INPUT_BODY_LEN])?;
    Ok((hdr, state))
}

/// Encode an OUTPUT packet (header + 64-byte feature report + tag) into `out`.
///
/// # Errors
/// [`WireError::Short`] if either buffer is too small.
pub fn encode_output_packet(
    hdr: &Header,
    report: &[u8],
    key: Option<&AuthKey>,
    out: &mut [u8],
) -> Result<(), WireError> {
    if out.len() < OUTPUT_PACKET_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: OUTPUT_PACKET_LEN,
        });
    }
    let mut hdr = *hdr;
    hdr.flags = if key.is_some() {
        hdr.flags | FLAG_AUTHENTICATED
    } else {
        hdr.flags & !FLAG_AUTHENTICATED
    };
    encode_header(&hdr, &mut out[..HEADER_LEN])?;
    encode_output(report, &mut out[HEADER_LEN..HEADER_LEN + OUTPUT_BODY_LEN])?;
    apply_tag(key, &mut out[..OUTPUT_PACKET_LEN]);
    Ok(())
}

/// Decode and authenticate an OUTPUT packet, copying the 64-byte feature
/// report payload into `report_out`.
///
/// # Errors
/// See [`decode_input_packet`] — same set.
pub fn decode_output_packet(
    buf: &[u8],
    key: Option<&AuthKey>,
    now_us: u32,
    replay_window_us: u32,
    report_out: &mut [u8],
) -> Result<Header, WireError> {
    if buf.len() < OUTPUT_PACKET_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: OUTPUT_PACKET_LEN,
        });
    }
    let hdr = decode_header(&buf[..HEADER_LEN])?;
    verify_packet(&hdr, key, &buf[..OUTPUT_PACKET_LEN], now_us, replay_window_us)?;
    decode_output(
        &buf[HEADER_LEN..HEADER_LEN + OUTPUT_BODY_LEN],
        report_out,
    )?;
    Ok(hdr)
}

/// Compute and write the trailing HMAC tag for `packet`. With `key = None`,
/// zeros the tag bytes — receivers will reject this packet if they were
/// configured with a key (because `FLAG_AUTHENTICATED` is also clear, set
/// by the encode helpers above).
fn apply_tag(key: Option<&AuthKey>, packet: &mut [u8]) {
    let len = packet.len();
    let body_end = len - TAG_LEN;
    match key {
        Some(k) => {
            let tag = auth::compute_tag(k, &packet[..body_end]);
            packet[body_end..].copy_from_slice(&tag);
        }
        None => {
            for byte in &mut packet[body_end..] {
                *byte = 0;
            }
        }
    }
}

/// Validate the auth + replay properties of a fully-buffered packet.
///
/// Plaintext (no key on either side) is allowed and skipped. Mixed
/// configurations (one side has a key, the other doesn't) are rejected so
/// that operators can't accidentally run with auth on one end only.
fn verify_packet(
    hdr: &Header,
    key: Option<&AuthKey>,
    packet: &[u8],
    now_us: u32,
    replay_window_us: u32,
) -> Result<(), WireError> {
    let len = packet.len();
    let body_end = len - TAG_LEN;
    let claims_auth = (hdr.flags & FLAG_AUTHENTICATED) != 0;

    match (key, claims_auth) {
        (Some(k), true) => {
            if !auth::verify_tag(k, &packet[..body_end], &packet[body_end..]) {
                return Err(WireError::AuthFailed);
            }
            if !auth::is_within_replay_window(hdr.timestamp_us, now_us, replay_window_us) {
                return Err(WireError::Replay);
            }
            Ok(())
        }
        (None, false) => Ok(()), // dev-mode plaintext on both sides
        // Mismatched: receiver expects auth and sender didn't sign, or
        // receiver doesn't have a key and sender claims auth.
        _ => Err(WireError::AuthFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> ControllerState {
        ControllerState {
            sequence: 0x1234_5678,
            buttons: Buttons::A | Buttons::DPAD_LEFT | Buttons::QAM | Buttons::RPAD_TOUCH,
            left_stick: Stick { x: 1000, y: -1000 },
            right_stick: Stick {
                x: -32_768,
                y: 32_767,
            },
            left_trigger: 0,
            right_trigger: 65_535,
            left_pad: Trackpad { x: 1, y: -1 },
            right_pad: Trackpad { x: 100, y: 200 },
            accel: Vec3i {
                x: -1,
                y: -2,
                z: -3,
            },
            gyro: Vec3i { x: 4, y: 5, z: 6 },
        }
    }

    #[test]
    fn header_roundtrip() {
        let hdr = Header {
            channel: Channel::Input,
            flags: FLAG_AUTHENTICATED,
            sequence: 42,
            timestamp_us: 0xDEAD_BEEF,
        };
        let mut buf = [0_u8; HEADER_LEN];
        encode_header(&hdr, &mut buf).unwrap();
        assert_eq!(&buf[0..4], &MAGIC);
        assert_eq!(buf[4], VERSION);
        assert_eq!(buf[5], 1);
        assert_eq!(decode_header(&buf).unwrap(), hdr);
    }

    #[test]
    fn input_body_roundtrip() {
        let state = sample_state();
        let mut buf = [0_u8; INPUT_BODY_LEN];
        encode_input(&state, &mut buf).unwrap();
        assert_eq!(decode_input(&buf).unwrap(), state);
    }

    #[test]
    fn output_body_roundtrip() {
        let mut report = [0_u8; OUTPUT_BODY_LEN];
        report[0] = 0xEB; // TRIGGER_RUMBLE_CMD
        report[1] = 4;
        report[2..6].copy_from_slice(&[0x00, 0x7D, 0x00, 0x3E]);
        let mut buf = [0_u8; OUTPUT_BODY_LEN];
        encode_output(&report, &mut buf).unwrap();
        let mut decoded = [0_u8; OUTPUT_BODY_LEN];
        decode_output(&buf, &mut decoded).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0_u8; HEADER_LEN];
        buf[0] = b'X';
        assert!(matches!(decode_header(&buf), Err(WireError::BadMagic)));
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = [0_u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = 99;
        buf[5] = 1;
        assert!(matches!(
            decode_header(&buf),
            Err(WireError::BadVersion(99))
        ));
    }

    #[test]
    fn rejects_bad_channel() {
        let mut buf = [0_u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = VERSION;
        buf[5] = 99;
        assert!(matches!(
            decode_header(&buf),
            Err(WireError::BadChannel(99))
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        let buf = [0_u8; 4];
        let mut out = [0_u8; OUTPUT_BODY_LEN];
        assert!(matches!(decode_header(&buf), Err(WireError::Short { .. })));
        assert!(matches!(decode_input(&buf), Err(WireError::Short { .. })));
        assert!(matches!(
            decode_output(&buf, &mut out),
            Err(WireError::Short { .. })
        ));
    }

    fn test_key() -> AuthKey {
        AuthKey::from_hex(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        )
        .unwrap()
    }

    fn header(channel: Channel) -> Header {
        Header {
            channel,
            flags: 0,
            sequence: 7,
            timestamp_us: 1_234_567,
        }
    }

    #[test]
    fn input_packet_roundtrip_with_auth() {
        let key = test_key();
        let hdr = header(Channel::Input);
        let state = sample_state();
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, Some(&key), &mut packet).unwrap();
        let (decoded_hdr, decoded_state) =
            decode_input_packet(&packet, Some(&key), hdr.timestamp_us, 30_000_000).unwrap();
        assert_eq!(decoded_hdr.flags & FLAG_AUTHENTICATED, FLAG_AUTHENTICATED);
        assert_eq!(decoded_hdr.sequence, hdr.sequence);
        assert_eq!(decoded_state, state);
    }

    #[test]
    fn input_packet_roundtrip_plaintext() {
        let hdr = header(Channel::Input);
        let state = sample_state();
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, None, &mut packet).unwrap();
        // Plaintext — flag clear, tag bytes zero.
        assert_eq!(packet[6], 0);
        for &b in &packet[INPUT_PACKET_LEN - 16..] {
            assert_eq!(b, 0);
        }
        let (_, decoded_state) =
            decode_input_packet(&packet, None, hdr.timestamp_us, 30_000_000).unwrap();
        assert_eq!(decoded_state, state);
    }

    #[test]
    fn input_packet_rejects_bad_tag() {
        let key = test_key();
        let hdr = header(Channel::Input);
        let state = sample_state();
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, Some(&key), &mut packet).unwrap();
        packet[INPUT_PACKET_LEN - 1] ^= 0xFF;
        assert!(matches!(
            decode_input_packet(&packet, Some(&key), hdr.timestamp_us, 30_000_000),
            Err(WireError::AuthFailed)
        ));
    }

    #[test]
    fn input_packet_rejects_mixed_config() {
        let key = test_key();
        let hdr = header(Channel::Input);
        let state = sample_state();
        // Sender has key, receiver doesn't.
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, Some(&key), &mut packet).unwrap();
        assert!(matches!(
            decode_input_packet(&packet, None, hdr.timestamp_us, 30_000_000),
            Err(WireError::AuthFailed)
        ));
        // Sender no key, receiver has key.
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, None, &mut packet).unwrap();
        assert!(matches!(
            decode_input_packet(&packet, Some(&key), hdr.timestamp_us, 30_000_000),
            Err(WireError::AuthFailed)
        ));
    }

    #[test]
    fn input_packet_rejects_replay() {
        let key = test_key();
        let hdr = header(Channel::Input);
        let state = sample_state();
        let mut packet = [0_u8; INPUT_PACKET_LEN];
        encode_input_packet(&hdr, &state, Some(&key), &mut packet).unwrap();
        // Receiver clock 2 minutes ahead of packet.
        let future = hdr.timestamp_us.wrapping_add(120_000_000);
        assert!(matches!(
            decode_input_packet(&packet, Some(&key), future, 30_000_000),
            Err(WireError::Replay)
        ));
    }

    #[test]
    fn output_packet_roundtrip() {
        let key = test_key();
        let hdr = header(Channel::Output);
        let mut report = [0_u8; OUTPUT_BODY_LEN];
        report[0] = 0xEB;
        report[1] = 4;
        let mut packet = [0_u8; OUTPUT_PACKET_LEN];
        encode_output_packet(&hdr, &report, Some(&key), &mut packet).unwrap();
        let mut decoded = [0_u8; OUTPUT_BODY_LEN];
        let decoded_hdr = decode_output_packet(
            &packet,
            Some(&key),
            hdr.timestamp_us,
            30_000_000,
            &mut decoded,
        )
        .unwrap();
        assert_eq!(decoded_hdr.channel, Channel::Output);
        assert_eq!(decoded, report);
    }
}
