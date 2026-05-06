//! Steam Deck controller protocol.
//!
//! Three layers, intentionally separate:
//!
//! - [`state`] — canonical controller state. The thing both sides agree on.
//! - [`hid`] — Deck HID report layout (byte-exact, lifted from
//!   `drivers/hid/hid-steam.c` in the Linux kernel and
//!   `SDL_hidapi_steamdeck.c` in SDL2). Used by the Windows driver to format
//!   reports Steam Input will accept.
//! - [`wire`] — network framing between Deck server and Windows client.
//!   Distinct from HID — we ship canonical state over the wire, not raw HID,
//!   so the Deck side stays decoupled from the exact USB report format.
//!
//! No I/O lives in this crate. Pure types + codecs.

#![no_std]

pub mod buttons;
pub mod hid;
pub mod state;
pub mod wire;

pub use buttons::Buttons;
pub use state::{ControllerState, RumbleCommand, Stick, Trackpad, Vec3i};
pub use wire::{Channel, Header, WireError};
