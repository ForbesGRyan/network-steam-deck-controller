//! Deck HID report layout.
//!
//! Lifted from `drivers/hid/hid-steam.c` (Linux kernel, GPL-2.0+):
//! `steam_do_deck_input_event` and `steam_do_deck_sensors_event` for the
//! input report; the feature-report `enum` for opcodes.
//!
//! Wire shape of every Steam Controller / Deck report (USB interrupt-in,
//! little-endian):
//!
//! ```text
//! 0    : 0x01    \  framing prefix; the kernel checks both
//! 1    : 0x00    /
//! 2    : type    -> see [`report_type`]
//! 3    : len     -> payload length (informational)
//! 4..N : payload -> meaning depends on `type`
//! ```
//!
//! For Deck input (`type == 0x09`) the 60-byte buffer holds:
//!
//! ```text
//! 4..8   u32   sequence number
//! 8..15  u8x   button bytes (sparse, see BUTTON_MAP)
//! 16..18 i16   left trackpad X
//! 18..20 i16   left trackpad Y
//! 20..22 i16   right trackpad X
//! 22..24 i16   right trackpad Y
//! 24..26 i16   accel X
//! 26..28 i16   accel Y
//! 28..30 i16   accel Z
//! 30..32 i16   gyro X
//! 32..34 i16   gyro Y
//! 34..36 i16   gyro Z
//! 44..46 i16   left analog trigger
//! 46..48 i16   right analog trigger
//! 48..50 i16   left stick X
//! 50..52 i16   left stick Y
//! 52..54 i16   right stick X
//! 54..56 i16   right stick Y
//! ```

use crate::buttons::Buttons;
use crate::state::{ControllerState, Stick, Trackpad, Vec3i};

/// Valve USB Vendor ID.
pub const VID_VALVE: u16 = 0x28de;

/// Steam Deck controller Product ID (HID interface).
pub const PID_STEAM_DECK: u16 = 0x1205;

/// Original Steam Controller (wired) PID. Kept for reference; not used by
/// this crate's encode/decode path.
pub const PID_STEAM_CONTROLLER: u16 = 0x1102;

/// Steam Controller wireless dongle PID. Reference only.
pub const PID_STEAM_CONTROLLER_WIRELESS: u16 = 0x1142;

/// Length of a Deck input report on the wire: 4-byte framing header
/// (`0x01 0x00 type len`) plus the [`DECK_PAYLOAD_LEN`]-byte payload.
pub const REPORT_LEN: usize = 4 + DECK_PAYLOAD_LEN as usize;

/// Always-on framing prefix at byte 0.
pub const FRAME_PREFIX_0: u8 = 0x01;
/// Always-on framing prefix at byte 1.
pub const FRAME_PREFIX_1: u8 = 0x00;

/// Payload length the Deck reports for its input frames (byte 3).
pub const DECK_PAYLOAD_LEN: u8 = 56;

/// Report type values (byte 2 of every report).
pub mod report_type {
    /// Steam Controller input data (60-byte payload).
    pub const CONTROLLER_STATE: u8 = 1;
    /// Debug data.
    pub const CONTROLLER_DEBUG: u8 = 2;
    /// Wireless connect / disconnect notification.
    pub const CONTROLLER_WIRELESS: u8 = 3;
    /// Battery / status update.
    pub const CONTROLLER_STATUS: u8 = 4;
    /// More debug.
    pub const CONTROLLER_DEBUG2: u8 = 5;
    /// Secondary Steam Controller state.
    pub const CONTROLLER_SECONDARY_STATE: u8 = 6;
    /// BLE state report.
    pub const CONTROLLER_BLE_STATE: u8 = 7;
    /// Steam Deck input data (56-byte payload).
    pub const CONTROLLER_DECK_STATE: u8 = 9;
}

/// Errors from parsing or encoding Deck HID input reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HidError {
    /// Buffer was the wrong length.
    BadLength {
        /// Bytes received.
        got: usize,
        /// Bytes expected.
        want: usize,
    },
    /// Framing prefix bytes did not match `0x01 0x00`.
    BadPrefix,
    /// `data[2]` (report type) did not match the expected
    /// [`report_type::CONTROLLER_DECK_STATE`].
    UnexpectedReportType(u8),
}

/// Mapping of `Buttons` flag to (byte offset, bit position) inside a Deck
/// input report. Offsets and bits taken from `steam_do_deck_input_event`.
#[allow(clippy::doc_markdown)]
pub const BUTTON_MAP: &[(Buttons, u8, u8)] = &[
    // byte 8
    (Buttons::R2, 8, 0),
    (Buttons::L2, 8, 1),
    (Buttons::R1, 8, 2),
    (Buttons::L1, 8, 3),
    (Buttons::Y, 8, 4),
    (Buttons::B, 8, 5),
    (Buttons::X, 8, 6),
    (Buttons::A, 8, 7),
    // byte 9
    (Buttons::DPAD_UP, 9, 0),
    (Buttons::DPAD_RIGHT, 9, 1),
    (Buttons::DPAD_LEFT, 9, 2),
    (Buttons::DPAD_DOWN, 9, 3),
    (Buttons::VIEW, 9, 4),
    (Buttons::STEAM, 9, 5),
    (Buttons::MENU, 9, 6),
    (Buttons::L5, 9, 7),
    // byte 10
    (Buttons::R5, 10, 0),
    (Buttons::LPAD_CLICK, 10, 1),
    (Buttons::RPAD_CLICK, 10, 2),
    (Buttons::LPAD_TOUCH, 10, 3),
    (Buttons::RPAD_TOUCH, 10, 4),
    (Buttons::L3, 10, 6),
    // byte 11
    (Buttons::R3, 11, 2),
    // byte 13
    (Buttons::L4, 13, 1),
    (Buttons::R4, 13, 2),
    // byte 14
    (Buttons::QAM, 14, 2),
];

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

/// Parse a [`REPORT_LEN`]-byte raw Deck HID input report into canonical state.
///
/// # Errors
/// - [`HidError::BadLength`] if `buf.len() != REPORT_LEN`.
/// - [`HidError::BadPrefix`] if the framing bytes are wrong.
/// - [`HidError::UnexpectedReportType`] if this isn't a Deck input report.
pub fn parse_input_report(buf: &[u8]) -> Result<ControllerState, HidError> {
    if buf.len() != REPORT_LEN {
        return Err(HidError::BadLength {
            got: buf.len(),
            want: REPORT_LEN,
        });
    }
    if buf[0] != FRAME_PREFIX_0 || buf[1] != FRAME_PREFIX_1 {
        return Err(HidError::BadPrefix);
    }
    if buf[2] != report_type::CONTROLLER_DECK_STATE {
        return Err(HidError::UnexpectedReportType(buf[2]));
    }

    let sequence = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

    let mut buttons = Buttons::empty();
    for &(flag, byte, bit) in BUTTON_MAP {
        if buf[byte as usize] & (1 << bit) != 0 {
            buttons |= flag;
        }
    }

    let left_pad = Trackpad {
        x: read_le_i16(buf, 16),
        y: read_le_i16(buf, 18),
    };
    let right_pad = Trackpad {
        x: read_le_i16(buf, 20),
        y: read_le_i16(buf, 22),
    };

    let accel = Vec3i {
        x: read_le_i16(buf, 24),
        y: read_le_i16(buf, 26),
        z: read_le_i16(buf, 28),
    };
    let gyro = Vec3i {
        x: read_le_i16(buf, 30),
        y: read_le_i16(buf, 32),
        z: read_le_i16(buf, 34),
    };

    // Triggers stored as i16 in the report; values are non-negative in
    // practice, so widen to u16 for the canonical type.
    #[allow(clippy::cast_sign_loss)]
    let left_trigger = read_le_i16(buf, 44) as u16;
    #[allow(clippy::cast_sign_loss)]
    let right_trigger = read_le_i16(buf, 46) as u16;

    let left_stick = Stick {
        x: read_le_i16(buf, 48),
        y: read_le_i16(buf, 50),
    };
    let right_stick = Stick {
        x: read_le_i16(buf, 52),
        y: read_le_i16(buf, 54),
    };

    Ok(ControllerState {
        sequence,
        buttons,
        left_stick,
        right_stick,
        left_trigger,
        right_trigger,
        left_pad,
        right_pad,
        accel,
        gyro,
    })
}

/// Encode canonical state into a [`REPORT_LEN`]-byte Deck HID input report buffer.
///
/// Bytes outside the documented field offsets are zeroed.
///
/// # Errors
/// [`HidError::BadLength`] if `out.len() != REPORT_LEN`.
pub fn encode_input_report(state: &ControllerState, out: &mut [u8]) -> Result<(), HidError> {
    if out.len() != REPORT_LEN {
        return Err(HidError::BadLength {
            got: out.len(),
            want: REPORT_LEN,
        });
    }

    for byte in out.iter_mut() {
        *byte = 0;
    }

    out[0] = FRAME_PREFIX_0;
    out[1] = FRAME_PREFIX_1;
    out[2] = report_type::CONTROLLER_DECK_STATE;
    out[3] = DECK_PAYLOAD_LEN;

    out[4..8].copy_from_slice(&state.sequence.to_le_bytes());

    for &(flag, byte, bit) in BUTTON_MAP {
        if state.buttons.contains(flag) {
            out[byte as usize] |= 1 << bit;
        }
    }

    write_le_i16(out, 16, state.left_pad.x);
    write_le_i16(out, 18, state.left_pad.y);
    write_le_i16(out, 20, state.right_pad.x);
    write_le_i16(out, 22, state.right_pad.y);

    write_le_i16(out, 24, state.accel.x);
    write_le_i16(out, 26, state.accel.y);
    write_le_i16(out, 28, state.accel.z);
    write_le_i16(out, 30, state.gyro.x);
    write_le_i16(out, 32, state.gyro.y);
    write_le_i16(out, 34, state.gyro.z);

    #[allow(clippy::cast_possible_wrap)]
    {
        write_le_i16(out, 44, state.left_trigger as i16);
        write_le_i16(out, 46, state.right_trigger as i16);
    }

    write_le_i16(out, 48, state.left_stick.x);
    write_le_i16(out, 50, state.left_stick.y);
    write_le_i16(out, 52, state.right_stick.x);
    write_le_i16(out, 54, state.right_stick.y);

    Ok(())
}

/// Feature-report opcodes the Deck firmware accepts on the control endpoint.
///
/// Lifted from the `enum` near the top of `drivers/hid/hid-steam.c`. Names
/// match the kernel's `ID_*` symbols with the prefix dropped.
#[allow(missing_docs)]
pub mod feature {
    pub const SET_DIGITAL_MAPPINGS: u8 = 0x80;
    pub const CLEAR_DIGITAL_MAPPINGS: u8 = 0x81;
    pub const GET_DIGITAL_MAPPINGS: u8 = 0x82;
    pub const GET_ATTRIBUTES_VALUES: u8 = 0x83;
    pub const GET_ATTRIBUTE_LABEL: u8 = 0x84;
    pub const SET_DEFAULT_DIGITAL_MAPPINGS: u8 = 0x85;
    pub const FACTORY_RESET: u8 = 0x86;
    pub const SET_SETTINGS_VALUES: u8 = 0x87;
    pub const CLEAR_SETTINGS_VALUES: u8 = 0x88;
    pub const GET_SETTINGS_VALUES: u8 = 0x89;
    pub const GET_SETTING_LABEL: u8 = 0x8A;
    pub const GET_SETTINGS_MAXS: u8 = 0x8B;
    pub const GET_SETTINGS_DEFAULTS: u8 = 0x8C;
    pub const SET_CONTROLLER_MODE: u8 = 0x8D;
    pub const LOAD_DEFAULT_SETTINGS: u8 = 0x8E;
    pub const TRIGGER_HAPTIC_PULSE: u8 = 0x8F;
    pub const TURN_OFF_CONTROLLER: u8 = 0x9F;
    pub const GET_DEVICE_INFO: u8 = 0xA1;
    pub const CALIBRATE_TRACKPADS: u8 = 0xA7;
    pub const SET_SERIAL_NUMBER: u8 = 0xA9;
    pub const GET_TRACKPAD_CALIBRATION: u8 = 0xAA;
    pub const GET_TRACKPAD_FACTORY_CALIBRATION: u8 = 0xAB;
    pub const GET_TRACKPAD_RAW_DATA: u8 = 0xAC;
    pub const ENABLE_PAIRING: u8 = 0xAD;
    pub const GET_STRING_ATTRIBUTE: u8 = 0xAE;
    pub const RADIO_ERASE_RECORDS: u8 = 0xAF;
    pub const RADIO_WRITE_RECORD: u8 = 0xB0;
    pub const SET_DONGLE_SETTING: u8 = 0xB1;
    pub const DONGLE_DISCONNECT_DEVICE: u8 = 0xB2;
    pub const DONGLE_COMMIT_DEVICE: u8 = 0xB3;
    pub const DONGLE_GET_WIRELESS_STATE: u8 = 0xB4;
    pub const CALIBRATE_GYRO: u8 = 0xB5;
    pub const PLAY_AUDIO: u8 = 0xB6;
    pub const AUDIO_UPDATE_START: u8 = 0xB7;
    pub const AUDIO_UPDATE_DATA: u8 = 0xB8;
    pub const AUDIO_UPDATE_COMPLETE: u8 = 0xB9;
    pub const GET_CHIPID: u8 = 0xBA;
    pub const CALIBRATE_JOYSTICK: u8 = 0xBF;
    pub const CALIBRATE_ANALOG_TRIGGERS: u8 = 0xC0;
    pub const SET_AUDIO_MAPPING: u8 = 0xC1;
    pub const CHECK_GYRO_FW_LOAD: u8 = 0xC2;
    pub const CALIBRATE_ANALOG: u8 = 0xC3;
    pub const DONGLE_GET_CONNECTED_SLOTS: u8 = 0xC4;
    pub const RESET_IMU: u8 = 0xCE;
    pub const TRIGGER_HAPTIC_CMD: u8 = 0xEA;
    pub const TRIGGER_RUMBLE_CMD: u8 = 0xEB;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> ControllerState {
        ControllerState {
            sequence: 0xDEAD_BEEF,
            buttons: Buttons::A
                | Buttons::DPAD_UP
                | Buttons::L4
                | Buttons::R5
                | Buttons::STEAM
                | Buttons::QAM
                | Buttons::LPAD_TOUCH,
            left_stick: Stick {
                x: 12_345,
                y: -6_789,
            },
            right_stick: Stick {
                x: -1,
                y: 1,
            },
            left_trigger: 0,
            right_trigger: 32_767,
            left_pad: Trackpad {
                x: 100,
                y: -200,
            },
            right_pad: Trackpad { x: 0, y: 0 },
            accel: Vec3i {
                x: 16_384,
                y: 0,
                z: -16_384,
            },
            gyro: Vec3i {
                x: 1,
                y: 2,
                z: 3,
            },
        }
    }

    #[test]
    fn roundtrip() {
        let state = sample_state();
        let mut buf = [0_u8; REPORT_LEN];
        encode_input_report(&state, &mut buf).expect("encode");

        // Frame header sanity.
        assert_eq!(buf[0], FRAME_PREFIX_0);
        assert_eq!(buf[1], FRAME_PREFIX_1);
        assert_eq!(buf[2], report_type::CONTROLLER_DECK_STATE);
        assert_eq!(buf[3], DECK_PAYLOAD_LEN);

        let parsed = parse_input_report(&buf).expect("parse");
        assert_eq!(state, parsed);
    }

    #[test]
    fn rejects_wrong_prefix() {
        let mut buf = [0_u8; REPORT_LEN];
        buf[0] = 0xFF;
        assert!(matches!(
            parse_input_report(&buf),
            Err(HidError::BadPrefix)
        ));
    }

    #[test]
    fn rejects_wrong_type() {
        let mut buf = [0_u8; REPORT_LEN];
        buf[0] = FRAME_PREFIX_0;
        buf[1] = FRAME_PREFIX_1;
        buf[2] = report_type::CONTROLLER_STATE; // not Deck
        assert!(matches!(
            parse_input_report(&buf),
            Err(HidError::UnexpectedReportType(1))
        ));
    }

    #[test]
    fn rejects_wrong_length() {
        let buf = [0_u8; 32];
        assert!(matches!(
            parse_input_report(&buf),
            Err(HidError::BadLength {
                got: 32,
                want: REPORT_LEN
            })
        ));
    }

    #[test]
    fn each_button_maps_to_distinct_bit() {
        // Catch typos in BUTTON_MAP: every (byte, bit) pair must be unique.
        let mut seen: [u8; 64] = [0; 64];
        for &(_, byte, bit) in BUTTON_MAP {
            let b = byte as usize;
            assert!(b < 64, "byte offset out of range");
            assert!(bit < 8, "bit out of range");
            assert_eq!(
                seen[b] & (1 << bit),
                0,
                "duplicate (byte {b}, bit {bit}) in BUTTON_MAP"
            );
            seen[b] |= 1 << bit;
        }
    }
}
