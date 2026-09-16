//! System tray via the standalone `tray-icon` crate (the same code Tauri
//! wraps — no Tauri needed). Menu holds Open/Quit; left-click shows the
//! window without opening the menu.
//!
//! Linux uses `tray-icon`'s `ksni` backend (pure-Rust `StatusNotifierItem` over
//! D-Bus): no GTK, no system tray dev-packages, no event-loop thread to host.
//! The `Tray` value must stay alive for the icon to remain: dropping it
//! removes the icon, and `Tray` always owns one — absence of a tray is
//! `Option<Tray>::None` at the call site, never a flag inside `Tray`.
//! Events arrive on `tray-icon`'s global channels; the work thread's poll
//! task drains them via [`drain_pending`], since `eframe` never polls those
//! channels on its own.

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const MENU_OPEN_ID: &str = "open";
const MENU_QUIT_ID: &str = "quit";

/// Raw bytes of the bundled icon, included exactly once. The tray and
/// window icons decode these; the toast registration writes them to disk.
pub static ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// Owns the tray icon. Held alive for the app lifetime; dropping removes
/// the icon from the tray.
pub struct Tray {
    _icon: TrayIcon,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayAction {
    Show,
    Quit,
}

/// Decodes the bundled png into raw rgba for both the tray and the window.
pub fn load_icon_rgba() -> Result<(Vec<u8>, u32, u32), String> {
    let image = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

fn load_icon() -> Result<Icon, String> {
    let (rgba, width, height) = load_icon_rgba()?;
    Icon::from_rgba(rgba, width, height).map_err(|error| error.to_string())
}

fn build_menu() -> Result<Menu, String> {
    let open = MenuItem::with_id(MENU_OPEN_ID, "Open", true, None);
    let quit = MenuItem::with_id(MENU_QUIT_ID, "Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &quit])
        .map_err(|error| error.to_string())?;
    Ok(menu)
}

/// Builds the tray icon. On Windows/macOS this must run on the thread that
/// owns an event loop (call from the `run_native` creator closure); the
/// Linux `ksni` backend pumps D-Bus itself and has no thread affinity, but
/// building there too keeps every platform uniform.
pub fn build() -> Result<Tray, String> {
    let icon = TrayIconBuilder::new()
        .with_id("main")
        .with_tooltip("Siphon")
        .with_icon(load_icon()?)
        .with_menu(Box::new(build_menu()?))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(Tray { _icon: icon })
}

/// Non-blocking drain of pending tray/menu events. The global receivers
/// never disconnect, so an empty vec means idle.
pub fn drain_pending() -> Vec<TrayAction> {
    let mut actions = Vec::new();
    for event in TrayIconEvent::receiver().try_iter() {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up | MouseButtonState::Down,
                ..
            }
        ) {
            actions.push(TrayAction::Show);
        }
    }
    for event in MenuEvent::receiver().try_iter() {
        if event.id.as_ref() == MENU_QUIT_ID {
            actions.push(TrayAction::Quit);
        } else if event.id.as_ref() == MENU_OPEN_ID {
            actions.push(TrayAction::Show);
        }
    }
    actions
}
