//! Tray icon thread.
//!
//! `tray-icon` requires a Win32 message pump. We park that pump on a
//! dedicated thread, expose a channel of `TrayEvent`s for the main loop to
//! consume, and a `TrayHandle` for the main loop to push status updates
//! back into the tray's tooltip.
//!
//! # API deviation from spec
//!
//! The spec wraps `tray_icon::TrayIcon` in `Arc<Mutex<Option<...>>>` for the
//! `TrayHandle`. That cannot compile because `TrayIcon` contains `Rc<RefCell<
//! ...>>` internally, making it `!Send`. Instead we send tooltip strings to
//! the tray thread via a second crossbeam channel, keeping the `TrayIcon`
//! entirely on its own thread.

use crossbeam_channel::{unbounded, Receiver, Sender};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Connect,
    Disconnect,
    Pair,
    Quit,
}

/// A handle the main loop can use to push tooltip text updates into the tray.
///
/// Internally this sends a string over a channel to the tray thread, which
/// calls `TrayIcon::set_tooltip` on the same thread that owns the icon.
#[derive(Clone)]
pub struct TrayHandle {
    tooltip_tx: Sender<String>,
}

impl TrayHandle {
    pub fn set_tooltip(&self, tooltip: &str) {
        let _ = self.tooltip_tx.send(tooltip.to_owned());
    }
}

/// Spawn the tray on its own thread. Returns the receiver of menu events
/// and a handle for tooltip updates.
///
/// # Panics
/// Never in practice; if icon construction fails we eprintln + return a
/// noop handle so the rest of the app keeps running.
#[must_use]
pub fn spawn() -> (Receiver<TrayEvent>, TrayHandle) {
    let (tx, rx) = unbounded::<TrayEvent>();
    let (tooltip_tx, tooltip_rx) = unbounded::<String>();

    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            run_tray_thread(&tx, &tooltip_rx);
        })
        .ok();

    (rx, TrayHandle { tooltip_tx })
}

fn run_tray_thread(tx: &Sender<TrayEvent>, tooltip_rx: &Receiver<String>) {
    let menu = Menu::new();
    let connect = MenuItem::new("Connect", true, None);
    let disconnect = MenuItem::new("Disconnect", true, None);
    let pair = MenuItem::new("Pair new Deck...", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[&connect, &disconnect, &pair, &quit]);

    // id() returns &MenuId; clone to take ownership for the comparison loop.
    let connect_id = connect.id().clone();
    let disconnect_id = disconnect.id().clone();
    let pair_id = pair.id().clone();
    let quit_id = quit.id().clone();

    let icon = match make_icon() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("tray icon: {e}");
            return;
        }
    };

    let tray_icon = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Network Deck — searching")
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tray build: {e}");
            return;
        }
    };

    let menu_rx = MenuEvent::receiver();
    pump_messages(
        menu_rx,
        tx,
        tooltip_rx,
        &tray_icon,
        &connect_id,
        &disconnect_id,
        &pair_id,
        &quit_id,
    );
}

#[allow(clippy::too_many_arguments)]
fn pump_messages(
    menu_rx: &crossbeam_channel::Receiver<MenuEvent>,
    tx: &Sender<TrayEvent>,
    tooltip_rx: &Receiver<String>,
    tray_icon: &tray_icon::TrayIcon,
    connect_id: &tray_icon::menu::MenuId,
    disconnect_id: &tray_icon::menu::MenuId,
    pair_id: &tray_icon::menu::MenuId,
    quit_id: &tray_icon::menu::MenuId,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let mut msg = MSG {
        hwnd: std::ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
    };

    loop {
        // Drain tooltip updates from the main loop.
        while let Ok(tip) = tooltip_rx.try_recv() {
            let _ = tray_icon.set_tooltip(Some(tip.as_str()));
        }

        // Drain menu events.
        while let Ok(event) = menu_rx.try_recv() {
            let mapped = if event.id == *connect_id {
                Some(TrayEvent::Connect)
            } else if event.id == *disconnect_id {
                Some(TrayEvent::Disconnect)
            } else if event.id == *pair_id {
                Some(TrayEvent::Pair)
            } else if event.id == *quit_id {
                Some(TrayEvent::Quit)
            } else {
                None
            };
            if let Some(e) = mapped {
                let _ = tx.send(e);
                if e == TrayEvent::Quit {
                    return;
                }
            }
        }

        // Pump one Win32 message (non-blocking) so the tray icon stays
        // responsive. A short sleep keeps the loop from spinning at 100%.
        unsafe {
            if PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Generate a 32x32 RGBA solid-color icon at runtime so we don't ship an
/// asset file. Color is a muted teal — visible on both light and dark
/// taskbars.
fn make_icon() -> Result<Icon, String> {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x2e, 0x86, 0x8a, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).map_err(|e| e.to_string())
}
