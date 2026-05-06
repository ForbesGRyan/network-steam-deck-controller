//! Network wire format between the Deck server and the Windows client.
//!
//! Two channels share a 16-byte header. Bodies are channel-specific. All
//! multi-byte integers are little-endian.
//!
//! Header layout (16 bytes):
//!
//! ```text
//!  0..4   magic            "NUSB"
//!  4      version          [`VERSION`]
//!  5      channel          [`Channel`]
//!  6..8   flags            reserved, must be zero
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
//! Output body (Windows → Deck, 6 bytes):
//!
//! ```text
//! 0..2  left_magnitude   u16
//! 2..4  right_magnitude  u16
//! 4..6  duration_ms      u16
//! ```

use crate::buttons::Buttons;
use crate::state::{ControllerState, RumbleCommand, Stick, Trackpad, Vec3i};

/// Magic bytes at the start of every packet. Catches misrouted traffic.
pub const MAGIC: [u8; 4] = *b"NUSB";

/// Wire-format version. Bump on incompatible changes.
pub const VERSION: u8 = 1;

/// Bytes in the shared packet header.
pub const HEADER_LEN: usize = 16;

/// Bytes in an [`Channel::Input`] body.
pub const INPUT_BODY_LEN: usize = 44;

/// Bytes in an [`Channel::Output`] body.
pub const OUTPUT_BODY_LEN: usize = 6;

/// Total bytes on the wire for an input packet.
pub const INPUT_PACKET_LEN: usize = HEADER_LEN + INPUT_BODY_LEN;

/// Total bytes on the wire for an output packet.
pub const OUTPUT_PACKET_LEN: usize = HEADER_LEN + OUTPUT_BODY_LEN;

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
    out[6..8].copy_from_slice(&[0, 0]); // flags reserved
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
    let sequence = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let timestamp_us = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    Ok(Header {
        channel,
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

/// Encode a [`RumbleCommand`] into the first 6 bytes of `out`.
///
/// # Errors
/// [`WireError::Short`] if `out.len() < OUTPUT_BODY_LEN`.
pub fn encode_output(cmd: &RumbleCommand, out: &mut [u8]) -> Result<(), WireError> {
    if out.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: out.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    write_le_u16(out, 0, cmd.left_magnitude);
    write_le_u16(out, 2, cmd.right_magnitude);
    write_le_u16(out, 4, cmd.duration_ms);
    Ok(())
}

/// Decode a [`RumbleCommand`] from the first 6 bytes of `buf`.
///
/// # Errors
/// [`WireError::Short`] if `buf.len() < OUTPUT_BODY_LEN`.
pub fn decode_output(buf: &[u8]) -> Result<RumbleCommand, WireError> {
    if buf.len() < OUTPUT_BODY_LEN {
        return Err(WireError::Short {
            got: buf.len(),
            want: OUTPUT_BODY_LEN,
        });
    }
    Ok(RumbleCommand {
        left_magnitude: read_le_u16(buf, 0),
        right_magnitude: read_le_u16(buf, 2),
        duration_ms: read_le_u16(buf, 4),
    })
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
        let cmd = RumbleCommand {
            left_magnitude: 32_000,
            right_magnitude: 16_000,
            duration_ms: 250,
        };
        let mut buf = [0_u8; OUTPUT_BODY_LEN];
        encode_output(&cmd, &mut buf).unwrap();
        assert_eq!(decode_output(&buf).unwrap(), cmd);
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
        assert!(matches!(decode_header(&buf), Err(WireError::Short { .. })));
        assert!(matches!(decode_input(&buf), Err(WireError::Short { .. })));
        assert!(matches!(decode_output(&buf), Err(WireError::Short { .. })));
    }
}
