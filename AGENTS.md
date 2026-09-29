# Agent guidance

## Commands

- `cargo run` / `cargo test` / `cargo build --release` — single binary crate `siphon`; binary lands at `target/release/siphon.exe`.
- `cargo clippy --all-targets --locked` — must be clean: workspace lints deny warnings plus clippy `all`/`pedantic`/`nursery`. Plain clippy is enough locally; CI runs only the check-only Windows pass: `cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc` (never links, no MSVC needed). No Linux clippy: the crate is Windows-only and cannot compile for Linux.
- Windows-only: no Linux/macOS targets. Builds need just the Rust toolchain
- plus MSVC for linking.
- Tests are offline-safe (`cargo test` needs no network/services): GQL decode fixtures, loopback HTTP server, temp files under `%TEMP%/siphon-*-<pid>.*`. Releases (`release.yml`): `v*` tag pushes reuse the tag; manual `workflow_dispatch` mints `<UTC-date>@<short-sha>` via the `setup` job (never a branch name). The upload stays `draft: true` (required for immutable releases); publishing is manual. Artifacts: portable exe + MSI (self-hosted cross-build, WiX on `windows-latest`); not a local concern. The MSI is per-user (no elevation, `%LocalAppData%`) via a frozen WiX template at `packaging/wix/main.wxs` — cargo-packager 0.11.8 has no scope option, so re-diff against upstream when bumping it.

## Architecture: GUI thread vs work thread

- Entry: `src/main.rs`. GUI thread (`src/app.rs` `SiphonView`, wgpui) is purely presentational: renders the latest `AppState`, sends `UiIntent`s. All work-thread state lives in one `Worker` (`src/state.rs`, behavior split across `src/event_loop.rs` for intents/jobs and `src/hermes.rs` for the connection) running on a compio thread-per-core runtime with a single `Worker::run()` loop; background work waits in one `FuturesUnordered` polled by that loop.
- Cross-thread wire is only `kanal` channels plus one shared slot: `UiIntent` GUI→work, `AppState` work→GUI through the `SharedFrame` slot (an `Arc<RwLock<…>>` holding only what the GUI renders; each `update` wakes the pump through the bundled channel, carrying `GuiEvent` `Frame`/`Theme`/`Tray`), `TrayAction` tray→work via a `kanal` channel whose sender the `Tray` owns (`Tray::build(tx)`).
- compio is thread-per-core, so futures are intentionally `!Send` (`future_not_send` allow in `Cargo.toml`; `compio::runtime::spawn` takes `Future + 'static` with no `Send` bound, so spawning is allowed but the single loop keeps all completions in one place). Never hold a `RwLock` guard across an `.await`: mutate the slot under short `SharedFrame::update` sections, and persist via save jobs that own a cloned `Config`.
- `Config::load` is sync and only for `main` before any runtime exists; on the work thread only use async `Config::save` (compio fs) so the runtime never blocks.
- Tray (`src/tray.rs`): built on the GUI thread inside `Application::run` (required thread affinity); the returned `Tray` value must stay alive (dropping removes the icon); `None` means close-to-quit instead of close-to-tray.

## Structure: flat over clever

- One owner, one loop, one place per behavior. Never split a flow across
  begin_/finish_, stage_/commit_, or request/response halves unless separate
  threads or await points force it.
- Prefer a longer function over a new abstraction. Don't extract helpers to
  shorten a function; extract only reused logic or a genuinely distinct
  operation with its own invariants.
- No trivial wrappers: no newtype around a single call, no one-line
  forwarding functions, no pass-through layers or channels between two
  halves of the same thread. Call the real thing directly.
- No speculative generality: no traits, generics, callbacks, or builders
  for one call site. Expose the direct value (a guard, a struct, a channel
  end) instead of inventing an API around it.
- A few duplicated lines beat a premature shared helper; deduplicate only
  once the shared logic is real, stable, and named by what it does.

## Gotchas

- eframe uses `wgpu` (Vulkan only) with `default-features = false`. Backend selection lives on the direct `wgpu` dep (`vulkan` + `vulkan-portability`); eframe's own `wgpu` feature pulls every backend (see `Cargo.toml` comment).
- Config at `dirs::config_dir()/com.iken.siphon/config.json`, `VERSION = 2`: a file newer than `VERSION` is discarded (fresh default), unversioned files load as 0 and are stamped on next save. Only resolved channels persist — unknown logins surface an error and must not touch disk or subscribe.
- Logging: `RUST_LOG=info|debug`, targets `app single config gql hermes notifier tray update`. Release goes to stderr plus a capped rotating file (10 MB total, 5 files) under local app-data `com.iken.siphon/logs`; debug goes to stdout only and never touches disk.
- Single instance key `com.iken.siphon` (`com.iken.siphon.debug` in debug builds, which get a separate config dir too): a second launch wakes the primary via callback and exits.
- Clippy allows that look like mistakes are deliberate: `cast_*` for tray math, `missing_panics_doc`/`missing_errors_doc`/`too_many_lines`/`missing_const_for_fn`.

## Comments

Comments describe the code as it exists now. Never describe the change itself.

- Explain _why_ something non-obvious is the way it is: invariants,
  ownership, threading, ordering constraints, failure modes.
- Do not simply restate the code in English, the comments are not written to replace reading the code, they are there as a reminder.
- Do not write what changed, what it used to do, what was removed, or why
  the change was made. That lives in git history and review discussion.
- Banned framing in comments: "now", "no longer", "instead of",
  "previously", "used to", "was removed", "startup init" as a synonym for
  "this used to be seeded elsewhere", or any reference to a prior shape.
- If a comment only makes sense as a diff ("so no per-channel commands are
  sent here"), delete it or rewrite it as a timeless invariant.
- Keep comments short. Prefer making the code obvious over explaining it.
