//! System tray icon with an Open/Quit menu; left-click shows the window
//! without opening the menu.
//!
//! Windows owns a raw `Shell_NotifyIconW` icon on a hidden message window,
//! so tray clicks arrive as window messages on the GUI thread and forward
//! into a [`kanal`] channel with sends that never block; the work task and
//! the GUI pump await tray events instead of polling. Other platforms build
//! no tray for now: closing the window quits there instead of hiding.
//! The `Tray` value must stay alive for the icon to remain: dropping it
//! removes the icon, and `Tray` always owns one — absence of a tray is
//! `Option<Tray>::None` at the call site, never a flag inside `Tray`.

#[cfg(windows)]
mod windows;
#[cfg(not(windows))]
mod other;

#[cfg(windows)]
pub use windows::{Tray, build, spawn_proxy};
#[cfg(not(windows))]
pub use other::{Tray, build, spawn_proxy};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayAction {
    Show,
    Quit,
}

/// Raw bytes of the bundled icon, included exactly once. The toast
/// registration writes these to disk.
pub static ICON_PNG: &[u8] = include_bytes!("../../icons/icon.png");
