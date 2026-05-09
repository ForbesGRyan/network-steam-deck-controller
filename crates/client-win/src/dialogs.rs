//! Dialogs for the Windows tray app.
//!
//! `info`, `error`, and `confirm` use native Win32 `MessageBoxW`. They're
//! used in sequence during the install + first-run-pair flow, and
//! `eframe::run_native` (winit) panics if a second event loop is created in
//! the same process — so anything called more than once must be native.
//!
//! `with_progress` is the only egui dialog. It's only ever invoked once per
//! process (during the usbip-win2 install), so a single `run_native` call
//! is safe. It runs a worker thread and ticks a spinner until the work
//! finishes; we keep egui here because `MessageBoxW` can't show a spinner.

use std::ptr;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::egui;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO,
};

use crate::util::wide;

const PROGRESS_SIZE: [f32; 2] = [480.0, 200.0];

/// Yes/No dialog. The `accept`/`decline` strings are appended to the body
/// (Win32 `MessageBoxW` doesn't support custom button labels, so the text
/// has to spell out which button to click). Closing the window via the X
/// button counts as `decline`. Blocks until the user dismisses it.
pub fn confirm(title: &str, body: &str, accept: &str, decline: &str) -> bool {
    let body = format!("{body}\n\nYes = {accept}    No = {decline}");
    message_box(title, &body, MB_YESNO | MB_ICONQUESTION) == IDYES
}

/// Single-button error dialog.
pub fn error(title: &str, body: &str) {
    message_box(title, body, MB_OK | MB_ICONERROR);
}

fn message_box(title: &str, body: &str, style: u32) -> i32 {
    let title_w = wide(title);
    let body_w = wide(body);
    // SAFETY: both buffers are NUL-terminated UTF-16 from `wide`. Passing
    // null hWnd shows an unowned top-level dialog. MB_TOPMOST | MB_SETFOREGROUND
    // brings it above whatever has focus (the tray app has no visible window
    // for it to anchor to).
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            style | MB_TOPMOST | MB_SETFOREGROUND,
        )
    }
}

/// Run `work` on a worker thread while showing a spinner + status line.
/// `work` can call `handle.set_status` from any thread to update the UI;
/// the dialog closes automatically when `work` returns. The worker's
/// return value is propagated back to the caller.
pub fn with_progress<T, F>(title: &str, initial_status: &str, work: F) -> T
where
    F: FnOnce(ProgressHandle) -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel::<ProgressMsg>();
    let handle = ProgressHandle { tx: tx.clone() };

    let worker_tx = tx;
    let worker: JoinHandle<T> = thread::Builder::new()
        .name("install-worker".into())
        .spawn(move || {
            let result = work(handle);
            let _ = worker_tx.send(ProgressMsg::Close);
            result
        })
        .expect("spawn install-worker");

    let initial = initial_status.to_owned();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(PROGRESS_SIZE)
            .with_min_inner_size(PROGRESS_SIZE)
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        title,
        opts,
        Box::new(move |_cc| {
            Ok(Box::new(ProgressApp { status: initial, rx }) as Box<dyn eframe::App>)
        }),
    )
    .expect("eframe::run_native(progress)");

    worker.join().expect("install-worker panicked")
}

/// Thread-safe handle into a running progress dialog.
pub struct ProgressHandle {
    tx: mpsc::Sender<ProgressMsg>,
}

impl ProgressHandle {
    pub fn set_status(&self, text: &str) {
        let _ = self.tx.send(ProgressMsg::SetStatus(text.to_owned()));
    }
}

enum ProgressMsg {
    SetStatus(String),
    Close,
}

struct ProgressApp {
    status: String,
    rx: mpsc::Receiver<ProgressMsg>,
}

impl eframe::App for ProgressApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        loop {
            match self.rx.try_recv() {
                Ok(ProgressMsg::SetStatus(s)) => self.status = s,
                Ok(ProgressMsg::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        // The worker controls when we exit — block X-button closes.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(40.0);
                ui.heading(egui::RichText::new(&self.status).size(22.0));
                ui.add_space(24.0);
                ui.spinner();
            });
        });
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}
