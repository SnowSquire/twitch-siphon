//! No tray icon on this platform for now. `build` always reports the
//! absence so callers fall back to quit-on-close; the proxy receiver pends
//! forever so the work task awaiting it never spins nor exits.

use super::TrayAction;

/// Placeholder tray handle; never constructed since `build` always errs.
pub struct Tray {
    _private: (),
}

/// Always errs: there is no tray icon to build here.
pub fn build() -> Result<Tray, String> {
    Err("system tray is not supported on this platform".to_owned())
}

/// Returns a receiver that never delivers. The sender is leaked so the
/// channel stays open and the work task awaiting it pends; a closed channel
/// would end the event loop.
pub fn spawn_proxy() -> kanal::Receiver<TrayAction> {
    let (tx, rx) = kanal::unbounded::<TrayAction>();
    std::mem::forget(tx);
    rx
}
