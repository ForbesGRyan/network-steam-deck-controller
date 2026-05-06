// Network Deck Controller — driver/user-mode contract.
//
// This header is included from BOTH the kernel driver and the user-mode
// Rust client (the latter via a thin C shim or hand-mirrored constants).
// Keep it minimal and standalone.

#pragma once

#ifdef _KERNEL_MODE
#include <ntdef.h>
#else
#include <windows.h>
#include <winioctl.h>
#endif

// ---------------------------------------------------------------------------
// Device interface GUID. User-mode finds the driver via SetupDiEnumDeviceInterfaces
// using this GUID, then CreateFile on the resulting symbolic link.
//
// Generated 2026-05-06. If you fork this driver, regenerate with `uuidgen`
// to avoid colliding with the original.
// ---------------------------------------------------------------------------
// {83A31D29-D1B2-4F5E-A04B-7C9F12345678}
DEFINE_GUID(GUID_DEVINTERFACE_DECK_VIRTUAL,
    0x83a31d29, 0xd1b2, 0x4f5e, 0xa0, 0x4b, 0x7c, 0x9f, 0x12, 0x34, 0x56, 0x78);

// ---------------------------------------------------------------------------
// Buffer sizes. INPUT mirrors deck_protocol::hid::REPORT_LEN. OUTPUT is
// generous; tighten once we know the real haptic/rumble report size.
// ---------------------------------------------------------------------------
#define DECK_INPUT_REPORT_SIZE    64
#define DECK_OUTPUT_REPORT_SIZE   64

// ---------------------------------------------------------------------------
// IOCTLs.
//
// IOCTL_DECK_PUSH_INPUT_REPORT
//   Direction: user -> driver
//   Input    : 64 bytes (one Deck HID input report).
//   Output   : none
//   Effect   : driver hands the bytes to the virtual USB interrupt-IN
//              endpoint; Steam's HID stack reads them as if they came from
//              real hardware.
//
// IOCTL_DECK_PEND_OUTPUT_REPORT
//   Direction: user <- driver (pended)
//   Input    : none
//   Output   : up to DECK_OUTPUT_REPORT_SIZE bytes (a feature/output report
//              the host wrote to the virtual device — rumble, haptic, LED).
//   Effect   : driver completes the request when an output report arrives.
//              User-mode keeps one of these IOCTLs outstanding to avoid
//              missing reports.
// ---------------------------------------------------------------------------
#define IOCTL_DECK_PUSH_INPUT_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_DATA)

#define IOCTL_DECK_PEND_OUTPUT_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_DATA)
