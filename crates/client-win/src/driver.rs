//! Driver IPC: find the virtual Deck device, open a handle, push input
//! reports / pend output reports.
//!
//! All constants here mirror `driver/inc/public.h`. Keep them in sync.
//! Mismatches manifest as `ERROR_INVALID_FUNCTION` when the IOCTL is
//! issued — annoying to debug, easy to prevent.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation
)]

use std::io;
use std::mem;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Bytes in a Deck HID input report. Mirrors `DECK_INPUT_REPORT_SIZE`.
pub const INPUT_REPORT_SIZE: usize = 64;
/// Buffer size for output reports. Mirrors `DECK_OUTPUT_REPORT_SIZE`.
pub const OUTPUT_REPORT_SIZE: usize = 64;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// `IOCTL_DECK_PUSH_INPUT_REPORT` from `public.h`.
pub const IOCTL_PUSH_INPUT_REPORT: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_DATA);

/// `IOCTL_DECK_PEND_OUTPUT_REPORT` from `public.h`.
pub const IOCTL_PEND_OUTPUT_REPORT: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_DATA);

/// `GUID_DEVINTERFACE_DECK_VIRTUAL` from `public.h`.
/// `{83A31D29-D1B2-4F5E-A04B-7C9F12345678}`
///
/// `static` rather than `const` so we can take a stable address of it for
/// the `SetupDi` FFI calls.
static GUID_DEVINTERFACE_DECK_VIRTUAL: GUID = GUID {
    data1: 0x83a3_1d29,
    data2: 0xd1b2,
    data3: 0x4f5e,
    data4: [0xa0, 0x4b, 0x7c, 0x9f, 0x12, 0x34, 0x56, 0x78],
};

/// Open handle to the virtual Deck driver.
pub struct DeckDriver {
    handle: HANDLE,
}

// SAFETY: Windows HANDLEs returned by CreateFile are usable from any
// thread. DeviceIoControl is documented as concurrent-safe across threads
// for separate I/O operations, which is how we use it (one thread pushes,
// one thread pends).
unsafe impl Send for DeckDriver {}
unsafe impl Sync for DeckDriver {}

impl DeckDriver {
    /// Find the virtual Deck device by interface GUID and open a handle.
    pub fn open() -> io::Result<Self> {
        // SAFETY: all SetupDi/CreateFile calls are FFI; we manage the
        // resulting HDEVINFO with SetupDiDestroyDeviceInfoList on every
        // exit path before propagating errors.
        unsafe {
            let dev_info = SetupDiGetClassDevsW(
                &raw const GUID_DEVINTERFACE_DECK_VIRTUAL,
                ptr::null(),
                ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            // In windows-sys 0.59, HDEVINFO is `isize` while
            // INVALID_HANDLE_VALUE is `*mut c_void`. Cast through.
            if dev_info == INVALID_HANDLE_VALUE as isize {
                return Err(io::Error::last_os_error());
            }

            let result = (|| -> io::Result<HANDLE> {
                let mut iface: SP_DEVICE_INTERFACE_DATA = mem::zeroed();
                iface.cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

                if SetupDiEnumDeviceInterfaces(
                    dev_info,
                    ptr::null_mut(),
                    &raw const GUID_DEVINTERFACE_DECK_VIRTUAL,
                    0,
                    &raw mut iface,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }

                // First call: probe required size. It returns FALSE with
                // ERROR_INSUFFICIENT_BUFFER but populates `required`.
                let mut required: u32 = 0;
                SetupDiGetDeviceInterfaceDetailW(
                    dev_info,
                    &raw const iface,
                    ptr::null_mut(),
                    0,
                    &raw mut required,
                    ptr::null_mut(),
                );

                // Use a u32-aligned backing buffer so the cast to
                // SP_DEVICE_INTERFACE_DETAIL_DATA_W (u32-aligned, due to
                // its leading DWORD cbSize) is well-defined.
                let words = (required as usize).div_ceil(mem::size_of::<u32>());
                let mut buffer = vec![0u32; words];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

                if SetupDiGetDeviceInterfaceDetailW(
                    dev_info,
                    &raw const iface,
                    detail,
                    required,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }

                // DevicePath is a null-terminated wchar at the end of the
                // detail struct; CreateFileW takes the same pointer shape.
                let path_ptr = (*detail).DevicePath.as_ptr();
                let handle = CreateFileW(
                    path_ptr,
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    0,
                    ptr::null_mut(),
                );

                if handle == INVALID_HANDLE_VALUE {
                    return Err(io::Error::last_os_error());
                }
                Ok(handle)
            })();

            SetupDiDestroyDeviceInfoList(dev_info);
            result.map(|handle| Self { handle })
        }
    }

    /// Push one Deck HID input report (must be exactly [`INPUT_REPORT_SIZE`]
    /// bytes) into the driver's IOCTL queue. Synchronous; completes after
    /// the driver has handed the bytes to the virtual interrupt-IN endpoint.
    pub fn push_input(&self, report: &[u8]) -> io::Result<()> {
        if report.len() != INPUT_REPORT_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("input report must be {INPUT_REPORT_SIZE} bytes, got {}", report.len()),
            ));
        }
        let mut returned: u32 = 0;
        // SAFETY: handle valid for self's lifetime; report is a borrowed
        // slice we read but don't keep a reference past the call.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_PUSH_INPUT_REPORT,
                report.as_ptr().cast(),
                report.len() as u32,
                ptr::null_mut(),
                0,
                &raw mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Block until the driver has an output report ready (rumble, haptic,
    /// LED). Returns the number of bytes written into `buf`. Use a thread
    /// dedicated to this — it can park indefinitely.
    pub fn pend_output(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut returned: u32 = 0;
        // SAFETY: handle valid for self's lifetime; buf is a unique mutable
        // borrow with the asserted length.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_PEND_OUTPUT_REPORT,
                ptr::null(),
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &raw mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(returned as usize)
    }
}

impl Drop for DeckDriver {
    fn drop(&mut self) {
        // SAFETY: handle was obtained from CreateFileW and not closed
        // elsewhere.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Resilient wrapper around [`DeckDriver`] for callers that want a long-lived
/// handle that survives driver reloads, hibernate transitions, and Steam
/// restarts. Each call clones the inner `Arc` out from under the lock,
/// invokes the I/O, and on `Err` invalidates the holder so the next call
/// transparently re-opens. Open attempts back off to 5 s so a fully absent
/// driver doesn't burn CPU on `SetupDiGetClassDevsW`.
///
/// `try_init` does not error on a missing driver — the holder starts empty
/// and the first successful `current()` call lazily opens. Call sites can
/// therefore always be running, even before `pnputil` has installed the .sys.
pub struct DriverHolder {
    inner: Mutex<Option<Arc<DeckDriver>>>,
    last_open_attempt: Mutex<Option<Instant>>,
}

impl DriverHolder {
    /// Construct a holder, attempting an initial open. The result of that
    /// attempt is captured but never returned — the caller proceeds either
    /// way. Logs the outcome so a misconfigured environment is visible.
    #[must_use]
    pub fn try_init() -> Self {
        let inner = match DeckDriver::open() {
            Ok(d) => {
                eprintln!("driver: opened");
                Some(Arc::new(d))
            }
            Err(e) => {
                eprintln!("driver: not available ({e}); will retry on demand");
                None
            }
        };
        Self {
            inner: Mutex::new(inner),
            last_open_attempt: Mutex::new(Some(Instant::now())),
        }
    }

    /// Open-on-demand backoff. Public so test code can plumb its own value.
    const REOPEN_INTERVAL: Duration = Duration::from_secs(5);

    /// Get the current `Arc<DeckDriver>`, attempting a re-open if absent
    /// and the backoff window has elapsed.
    fn current(&self) -> Option<Arc<DeckDriver>> {
        // Fast path: handle is live.
        if let Some(d) = self.inner.lock().ok().and_then(|g| g.clone()) {
            return Some(d);
        }
        // Slow path: throttle re-open attempts.
        let mut last = self.last_open_attempt.lock().ok()?;
        if let Some(t) = *last {
            if t.elapsed() < Self::REOPEN_INTERVAL {
                return None;
            }
        }
        *last = Some(Instant::now());
        drop(last);

        match DeckDriver::open() {
            Ok(d) => {
                let arc = Arc::new(d);
                if let Ok(mut g) = self.inner.lock() {
                    *g = Some(arc.clone());
                }
                eprintln!("\ndriver: reopened");
                Some(arc)
            }
            Err(_) => None,
        }
    }

    fn invalidate(&self) {
        if let Ok(mut g) = self.inner.lock() {
            *g = None;
        }
    }

    /// Whether a handle is currently held. Diagnostic only — the answer can
    /// race with another thread invalidating, so don't gate I/O on it.
    pub fn is_open(&self) -> bool {
        self.inner.lock().ok().is_some_and(|g| g.is_some())
    }

    pub fn push_input(&self, report: &[u8]) -> io::Result<()> {
        let Some(d) = self.current() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "driver not open",
            ));
        };
        match d.push_input(report) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.invalidate();
                Err(e)
            }
        }
    }

    pub fn pend_output(&self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(d) = self.current() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "driver not open",
            ));
        };
        match d.pend_output(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                self.invalidate();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_public_h() {
        // CTL_CODE(0x22, 0x800, 0, 0x002) = 0x22A000
        assert_eq!(IOCTL_PUSH_INPUT_REPORT, 0x0022_A000);
        // CTL_CODE(0x22, 0x801, 0, 0x001) = 0x226004
        assert_eq!(IOCTL_PEND_OUTPUT_REPORT, 0x0022_6004);
    }
}
