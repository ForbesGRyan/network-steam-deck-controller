//! Reusable egui-based dialogs for the Windows tray app. Same framework
//! the Deck kiosk uses, so dialogs match visually.
//!
//! Each entry point spins up a top-level eframe window on the calling
//! thread. `with_progress` additionally runs a worker thread for the
//! actual work and ticks the dialog until the worker is done.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::egui;

const CONFIRM_SIZE: [f32; 2] = [480.0, 240.0];
const PROGRESS_SIZE: [f32; 2] = [480.0, 200.0];

/// Yes/No dialog with custom button labels. Closing the window via the X
/// button counts as `decline`. Blocks until the user dismisses it.
pub fn confirm(title: &str, body: &str, accept: &str, decline: &str) -> bool {
    let result = Arc::new(Mutex::new(false));
    let result2 = Arc::clone(&result);
    let body = body.to_owned();
    let accept = accept.to_owned();
    let decline = decline.to_owned();

    let opts = window_options(CONFIRM_SIZE);
    eframe::run_native(
        title,
        opts,
        Box::new(move |_cc| {
            Ok(Box::new(ConfirmApp { body, accept, decline, result: result2 })
                as Box<dyn eframe::App>)
        }),
    )
    .expect("eframe::run_native(confirm)");

    let r = *result.lock().expect("confirm result mutex");
    r
}

/// Single-button informational dialog.
pub fn info(title: &str, body: &str) {
    show_oneshot(title, body, false);
}

/// Single-button error dialog (red body text).
pub fn error(title: &str, body: &str) {
    show_oneshot(title, body, true);
}

fn show_oneshot(title: &str, body: &str, is_error: bool) {
    let body = body.to_owned();
    let opts = window_options(CONFIRM_SIZE);
    eframe::run_native(
        title,
        opts,
        Box::new(move |_cc| {
            Ok(Box::new(OneShotApp { body, is_error }) as Box<dyn eframe::App>)
        }),
    )
    .expect("eframe::run_native(oneshot)");
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
    let opts = window_options(PROGRESS_SIZE);
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

fn window_options(size: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_resizable(false),
        ..Default::default()
    }
}

struct ConfirmApp {
    body: String,
    accept: String,
    decline: String,
    result: Arc<Mutex<bool>>,
}

impl eframe::App for ConfirmApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(20.0);
            ui.label(egui::RichText::new(&self.body).size(16.0));
            ui.add_space(20.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let accept = egui::Button::new(egui::RichText::new(&self.accept).size(16.0))
                    .min_size(egui::vec2(120.0, 36.0));
                if ui.add(accept).clicked() {
                    *self.result.lock().expect("confirm result mutex") = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.add_space(8.0);
                let decline = egui::Button::new(egui::RichText::new(&self.decline).size(16.0))
                    .min_size(egui::vec2(120.0, 36.0));
                if ui.add(decline).clicked() {
                    *self.result.lock().expect("confirm result mutex") = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

struct OneShotApp {
    body: String,
    is_error: bool,
}

impl eframe::App for OneShotApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(20.0);
            if self.is_error {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    egui::RichText::new(&self.body).size(16.0),
                );
            } else {
                ui.label(egui::RichText::new(&self.body).size(16.0));
            }
            ui.add_space(20.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = egui::Button::new(egui::RichText::new("OK").size(16.0))
                    .min_size(egui::vec2(120.0, 36.0));
                if ui.add(ok).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
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
