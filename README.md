# Siphon

A Twitch notifier: track streamers, get a toast the moment they go live
(or retitle while offline). Pure Rust desktop app built on
[`eframe`](https://github.com/emilk/egui)/`egui`, with a system-tray icon
via the standalone [`tray-icon`](https://github.com/tauri-apps/tray-icon)
crate and single-instance enforcement via
[`app-single-instance`](https://crates.io/crates/app-single-instance).

## Dev

```powershell
cd src-tauri
cargo run
```

Logs go to stderr; set `RUST_LOG=info` (or `debug`) to see the
`config`/`gql`/`hermes`/`notifier` targets.

## Release

```powershell
cd src-tauri
cargo build --release
```

The portable binary is `src-tauri/target/release/siphon.exe`. Closing the
window hides it to the tray; Quit from the tray menu exits. A second launch
wakes the running instance instead of starting a new one.

Config lives at `%APPDATA%\com.iken.siphon\config.json` (same path as the
old Tauri builds, so existing installs carry over).

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Linux notes

Targets are Windows, macOS, and Wayland. On Linux the tray icon uses
`tray-icon`'s `ksni` backend (pure-Rust StatusNotifierItem over D-Bus), so
no system tray dev-packages are needed to build. To actually see the icon,
the desktop must speak StatusNotifierItem: KDE Plasma works out of the box,
GNOME needs its AppIndicator extension (shipped by default on Ubuntu).
