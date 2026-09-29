# Siphon

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev/)

Never miss a stream. Siphon watches your favorite Twitch streamers and pops up
a notification the moment one goes live. Click it (or the **Watch** button) to
open the stream in your browser. It can also notify you when a channel changes
its title.

## Download (Windows)

**Recommended: the installer (`.msi`).**

1. Go to the [Releases page](https://github.com/SnowSquire/twitch-siphon/releases)
   and open the latest release.
2. Download the `.msi` file.
3. Double-click it to install. It installs just for you — no admin password
   needed.

Don't want to install anything? Grab the `Siphon_*_x64-portable.exe` from the
same release instead: just download it and double-click to run.

## How to use

- **Add a streamer:** open Siphon, type their Twitch username, and add them to
  the Channels list.
- **Get notified:** when someone goes live, a notification appears and stays in
  the notification center until you dismiss it.
- **Title changes:** Siphon also notices when a channel changes its title. On
  the Filtered words tab you can list words (like `rerun`) whose title changes
  should stay silent.
- **The app lives in the tray:** closing the window keeps Siphon running in the
  system tray so it can keep watching. To fully exit, right-click the tray icon
  and choose Quit. If you launch Siphon while it's already running, it just
  opens the existing window.

## For developers

```powershell
cargo run
```

Release build:

```powershell
cargo build --release
```

The binary is `target/release/siphon.exe`. `cargo test` is offline-safe
(no network or services needed), and
`cargo clippy --all-targets --locked` must be clean.

Config lives at `%APPDATA%\com.iken.siphon\config.json`. Logs go to stderr; set
`RUST_LOG=info` (or `debug`) to see the
`app`/`single`/`config`/`gql`/`hermes`/`notifier`/`tray`/`update` targets.

Recommended IDE setup: [VS Code](https://code.visualstudio.com/) +
[rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer).
