//! Canonical controller state.
//!
//! Both the Deck server and the Windows client speak this type. It is *not*
//! the HID wire format — see [`crate::hid`] for the byte-exact codec. Field
//! semantics follow the raw Deck HID report (no Linux-input axis flipping).

use crate::buttons::Buttons;

/// Full controller state at one instant.
///
/// Range conventions, lifted from `drivers/hid/hid-steam.c`:
///
/// - Sticks / trackpads: signed 16-bit, `[-32768, 32767]`.
/// - Triggers: 16-bit, `0` released → `~32767` fully pressed (the report
///   stores them as i16; we widen to u16 for clarity since values are
///   non-negative in practice).
/// - Accelerometer: signed 16-bit, range ±2 g, 16384 LSB/g.
/// - Gyroscope: signed 16-bit, range ±2000 dps, 16 LSB/dps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControllerState {
    /// Per-frame sequence number from the controller (bytes 4..8 of the
    /// raw HID input report).
    pub sequence: u32,

    /// Digital button state. See [`Buttons`].
    pub buttons: Buttons,

    /// Left analog stick.
    pub left_stick: Stick,
    /// Right analog stick.
    pub right_stick: Stick,

    /// Left analog trigger.
    pub left_trigger: u16,
    /// Right analog trigger.
    pub right_trigger: u16,

    /// Left trackpad position. Meaningful only when
    /// [`Buttons::LPAD_TOUCH`] is set.
    pub left_pad: Trackpad,
    /// Right trackpad position. Meaningful only when
    /// [`Buttons::RPAD_TOUCH`] is set.
    pub right_pad: Trackpad,

    /// Linear acceleration.
    pub accel: Vec3i,
    /// Angular velocity.
    pub gyro: Vec3i,
}

/// Analog stick XY pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stick {
    /// `[-32768, 32767]`, positive = right.
    pub x: i16,
    /// `[-32768, 32767]`, positive = up (raw report convention).
    pub y: i16,
}

/// Trackpad XY position. Touch state lives in [`Buttons`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trackpad {
    /// `[-32768, 32767]`.
    pub x: i16,
    /// `[-32768, 32767]`.
    pub y: i16,
}

/// 3D vector of signed 16-bit values (gyro, accel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vec3i {
    /// X component.
    pub x: i16,
    /// Y component.
    pub y: i16,
    /// Z component.
    pub z: i16,
}
