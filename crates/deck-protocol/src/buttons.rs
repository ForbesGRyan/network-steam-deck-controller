//! Deck button bitflags.
//!
//! Logical bit positions chosen for readability — they do **not** mirror
//! the HID wire layout. The byte/bit mapping for the actual report lives
//! in [`crate::hid::BUTTON_MAP`], lifted from `drivers/hid/hid-steam.c`
//! (`steam_do_deck_input_event`).

use bitflags::bitflags;

bitflags! {
    /// Digital buttons on a Steam Deck.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
    pub struct Buttons: u64 {
        // Face
        const A = 1 << 0;
        const B = 1 << 1;
        const X = 1 << 2;
        const Y = 1 << 3;

        // Shoulders + trigger digital clicks
        const L1 = 1 << 4;
        const R1 = 1 << 5;
        const L2 = 1 << 6;
        const R2 = 1 << 7;

        // Stick clicks
        const L3 = 1 << 8;
        const R3 = 1 << 9;

        // System
        const VIEW  = 1 << 10; // "..." three dots, left of left trackpad
        const MENU  = 1 << 11; // hamburger, right of right trackpad
        const STEAM = 1 << 12;
        const QAM   = 1 << 13; // Quick Access Menu (three small dots)

        // Back paddles
        const L4 = 1 << 14;
        const L5 = 1 << 15;
        const R4 = 1 << 16;
        const R5 = 1 << 17;

        // D-pad
        const DPAD_UP    = 1 << 18;
        const DPAD_DOWN  = 1 << 19;
        const DPAD_LEFT  = 1 << 20;
        const DPAD_RIGHT = 1 << 21;

        // Trackpad clicks + capacitive touch
        const LPAD_CLICK = 1 << 22;
        const RPAD_CLICK = 1 << 23;
        const LPAD_TOUCH = 1 << 24;
        const RPAD_TOUCH = 1 << 25;
    }
}
