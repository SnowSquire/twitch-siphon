# Agent guidance

## Commands (repo root; there is no `src-tauri/` despite what README says)

- `cargo run` / `cargo test` / `cargo build --release` — single binary crate `siphon`; binary lands at `target/release/siphon.exe`.
- `cargo clippy --all-targets --locked` — must be clean: workspace lints deny warnings plus clippy `all`/`pedantic`/`nursery`. Plain clippy is enough; CI adds only a check-only Windows pass: `cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc` (never links, no MSVC needed).
- Linux build deps: `gcc pkg-config libgl-dev libegl-dev libwayland-dev libxkbcommon-dev`. Tray needs nothing extra (pure-Rust `ksni` backend, no GTK/appindicator).
- Tests are offline-safe (`cargo test` needs no network/services): GQL decode fixtures, loopback HTTP server, temp files under `%TEMP%/siphon-*-<pid>.*`. Releases (`release.yml`): `v*` tag pushes reuse the tag; manual `workflow_dispatch` mints `<UTC-date>@<short-sha>` via the `setup` job (never a branch name). Both uploaders share that tag and stay `draft: true` (required for immutable releases); publishing is manual. Artifacts: portable exe + MSI (self-hosted cross-build, WiX on `windows-latest`) + AppImage; not a local concern. The MSI is per-user (no elevation, `%LocalAppData%`) via a frozen WiX template at `packaging/wix/main.wxs` — cargo-packager 0.11.8 has no scope option, so re-diff against upstream when bumping it.

## Architecture: GUI thread vs work thread

- Entry: `src/main.rs`. GUI thread (`src/app.rs` `SiphonApp`, eframe/egui) is purely presentational: renders the latest `FrameState`, sends `UiIntent`s. All app state lives on one work thread as `Rc<RefCell<WorkState>>` (`src/state.rs`) running a compio thread-per-core runtime with `EventLoop::run()` + hermes `Session::run()` joined — no task is ever spawned (`src/event_loop.rs` polls GQL resolves via `FuturesUnordered`).
- Cross-thread wire is only `kanal` channels plus `GuiWaker` (a repaint doorbell, not state): `UiIntent` GUI→work, `FrameState` snapshots work→GUI, `TrayAction` via `tray::spawn_proxy` thread. Channels alone can't wake winit — every `push_frame` must also poke the waker.
- compio is thread-per-core, so futures are intentionally `!Send` (`future_not_send` allow in `Cargo.toml`). Never hold a `RefCell` borrow across an `.await`: follow the `stage_*` (sync, under one short borrow) + `commit_save` (async, no borrow held) split in `state.rs`.
- `Config::load` is sync and only for `main` before any runtime exists; on the work thread only use async `Config::save` (compio fs) so the runtime never blocks.
- Tray (`src/tray.rs`): build inside the `run_native` creator closure (required thread affinity on Windows/macOS); the returned `Tray` value must stay alive (dropping removes the icon); `None` means close-to-quit instead of close-to-tray.

## Gotchas

- eframe uses `wgpu` (Vulkan only) with `default-features = false`. Backend selection lives on the direct `wgpu` dep (`vulkan` + `vulkan-portability`); eframe's own `wgpu` feature pulls every backend (see `Cargo.toml` comment).
- Config at `dirs::config_dir()/com.iken.siphon/config.json`, `VERSION = 1`: a file newer than `VERSION` is discarded (fresh default), unversioned files load as 0 and are stamped on next save. Only resolved channels persist — unknown logins surface an error and must not touch disk or reach the session.
- Logging: `RUST_LOG=info|debug`, targets `app single config gql hermes notifier tray`. Release goes to stderr plus a capped rotating file (10 MB total, 5 files) under local app-data `com.iken.siphon/logs`; debug goes to stdout only and never touches disk.
- Single instance key `com.iken.siphon` (`com.iken.siphon.debug` in debug builds, which get a separate config dir too): a second launch wakes the primary via callback and exits.
- Clippy allows that look like mistakes are deliberate: `cast_*` for tray math, `missing_panics_doc`/`missing_errors_doc`/`too_many_lines`/`missing_const_for_fn`.

## Comments

Comments describe the code as it exists now. Never describe the change itself.

- Explain *why* something non-obvious is the way it is: invariants,
  ownership, threading, ordering constraints, failure modes.
- Do not write what changed, what it used to do, what was removed, or why
  the change was made. That lives in git history and review discussion.
- Banned framing in comments: "now", "no longer", "instead of",
  "previously", "used to", "was removed", "startup init" as a synonym for
  "this used to be seeded elsewhere", or any reference to a prior shape.
- If a comment only makes sense as a diff ("so no per-channel commands are
  sent here"), delete it or rewrite it as a timeless invariant.
- Keep comments short. Prefer making the code obvious over explaining it.
