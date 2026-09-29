# Agent guidance

## Commands

- `cargo run` / `cargo test` / `cargo build --release` — single binary crate `siphon`; binary lands at `target/release/siphon.exe`.
- `cargo clippy --all-targets --locked` — must be clean: workspace lints deny warnings plus clippy `all`/`pedantic`/`nursery`. Plain local clippy is enough; CI runs the same command with `--target x86_64-pc-windows-msvc` on `ubuntu-latest` (type-checks the Windows target without linking, so no MSVC needed). No Linux clippy: the crate is Windows-only and cannot compile for Linux.
- Windows-only: no Linux/macOS targets. Builds need just the Rust toolchain plus MSVC for linking.
- Releases (`release.yml`): `v*` tag pushes reuse the tag; manual `workflow_dispatch` mints `<UTC-date>@<short-sha>` via the `setup` job (never a branch name). The upload stays `draft: true` (required for immutable releases); publishing is manual. Artifacts: portable exe + MSI (self-hosted Fedora cross-build via cargo-xwin, WiX packaging on `windows-latest`); not a local concern. The MSI is per-user (no elevation, `%LocalAppData%`) via a frozen WiX template at `packaging/wix/main.wxs` — cargo-packager 0.11.8 has no scope option, so re-diff against upstream when bumping it.

## Architecture: GUI thread vs work thread

- Entry: `src/main.rs`. GUI thread (`src/app.rs` `SiphonView`, wgpui) is purely presentational: renders its own `Snapshot` copy, sends `UiIntent`s. All work-thread behavior lives in one `Worker` (`src/state.rs`, split across `src/event_loop.rs` for intents/jobs and `src/hermes.rs` for the connection) running on a compio thread-per-core runtime with a single `Worker::run()` loop; background work waits in one `FuturesUnordered` polled by that loop.
- Cross-thread wire is only `kanal` channels plus one shared slot: `UiIntent` GUI→work, `Snapshot` work→GUI through the `SharedSnapshot` slot (an `Arc<RwLock<…>>` holding only what the GUI renders; each `update` wakes the pump through the bundled channel, carrying `GuiEvent` `Snapshot`/`Theme`/`Tray`), `TrayAction` tray→work via a `kanal` channel whose sender the `Tray` owns (`Tray::build(tx)`).
- One source of truth for presentation state: live channel rows live only
  in the shared `Snapshot`, mutated in place per event, never rebuilt. The
  worker holds no duplicate view-model — only what the GUI never renders
  (mute stamps, subscription protocol ids, socket, jobs). The GUI owns its
  snapshot copy and renders lock-free.
- Subscription acceptance is worker-private, not snapshot state: `sub_ids`
  maps each `Topic` to a `SubEntry` (`sub_id`, send-attempt counter,
  `Pending`/`Accepted`/`Rejected`). Every send arms one jobs-timeout
  carrying (topic, sub-id, attempt); the timeout errors only on an exact
  match still `Pending`, so stale deadlines can't fail newer attempts.
  Rejections error immediately and never double-report. Entries are never
  dropped — retries ride the normal reconnect replay. Notifications
  require `Accepted`.
- compio is thread-per-core, so futures are intentionally `!Send` (`future_not_send` allow in `Cargo.toml`; `compio::runtime::spawn` takes `Future + 'static` with no `Send` bound, so spawning is allowed but the single loop keeps all completions in one place). Never hold a `RwLock` guard across an `.await`: mutate the slot under short `SharedSnapshot::update` sections, and persist via save jobs that own a cloned `Config`.
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
- Prefer slices over `Vec` (and `&str` over `String`) for read-only
  parameters; unconditionally shared work goes straight-line before the
  branch, not into a helper. Duplication that guards different moments in
  time (early-out vs mid-flight race check) stays duplicated — a helper
  would hide which moment it serves.
- A few duplicated lines beat a premature shared helper; deduplicate only
  once the shared logic is real, stable, and named by what it does.

## Tests: rare and silent failures only

`cargo test` must stay offline-safe: inline decode fixtures, a one-shot
loopback server on `127.0.0.1:0`, temp files under
`%TEMP%/siphon-*-<pid>.*` (test-only pattern; production paths like
`siphon-update-{version}.msi` and `twitch-siphon-icon.png` differ). No
network, no services.

- The razor: a test earns its place when its failure would be silent,
  slow to notice, or on a path that rarely executes. Silent means wrong
  data shown as right (dates, viewer counts, filter decisions), swallowed
  work (notifications, updates), quiet data loss (config, saves), or
  unbounded growth (logs, cache). Rare means migration, rotation,
  truncation, update install, prune, odd wire shapes.
- Worth testing: config migration per version + newer-discarded +
  cleared-list-stays-cleared; batch fault isolation, nullable-layer
  collapse, string-or-number ids; `Ok(None)` vs `Err` resolve contract;
  save wiring and no-disk-touch on unknown login; prune path; rotation vs
  startup-cap vs live-truncate; truncated-body cleanup; relaunch ordering
  (`msiexec` before exe) and path quoting; matcher case-fold +
  empty-matches-nothing; `should_log_stream` change-only rule; date-math
  edges; no-msi + non-semver release shapes; fetch plumbing compared
  against `parse_release` output, never re-asserted field-by-field;
  subscription-confirmation timeout (pending-errors, accepted-silences,
  stale-attempt-silences, rejection-singles).
- Do NOT add: tests whose failure breaks the app on sight on every run —
  bundled asset round-trips, mainline envelope happy-paths, strict-greater
  version checks on a six-line function (known cuts: `tray.rs` ico/png
  positives, `http.rs` single-`user` happy path, `update.rs`
  version-greater). If such code has no other coverage, fold its unique
  asserts into an edge-case test over the same code instead of writing a
  second test. Same for a second test for the same branch with only the
  error string changed — parameterize instead (e.g. `state.rs` failure
  cases); error paths of _different_ functions each earn their own.
- One behavior, one test. Name tests `verb_condition` (`*_without_touching_disk`,
  `*_drops_oldest_first`). If a new version/config field is added, extend the
  `migrate_step_covers_every_version_below_current`-style guard, don't just
  add another happy-path test.

## Comments: why, not what or what-changed

Comments describe the code as it exists now. Never describe the change itself.

- Write a comment only for non-obvious _why_: threading/ownership
  (`!Send` client per runtime thread, no guard across `.await`, tray thread
  affinity), ordering constraints (drain headers before close, msiexec
  before reopen), failure modes (one bad batch entry skips instead of
  failing all; rename of an open file fails on Windows), wire invariants
  (GQL ids arrive string-or-number; nullable layers collapse to `None`),
  and why an override exists (WiX template, single TLS stack, clippy `allow`
  with the invariant it encodes). `SAFETY:` lines must state the
  precondition, not repeat the call.
- Do NOT write: restatements of the code (`// Missing broadcast settings
  cannot build a User.` above an early return), section headers that repeat
  a name (`// Tray-specific overrides`), layout narration, or reviewer
  questions left in code (`// OZEN: why does this exist…` — ask, resolve,
  delete).
- No prior-shape references. Banned framing: "now", "no longer", "instead
  of" / "previously" / "used to" / "predates" / "was removed" / "same
  approach as the <old stack>" when used to mean "this used to be
  elsewhere", and "startup init" as a synonym for seeding that moved.
  ("collapses to `None` instead of failing the whole batch" is fine — it
  states a failure mode, not history. "...shares v1's shape: stamping is
  the whole migration" rewritten timelessly is fine; "v0 predates
  versioning" is not.) If a comment only makes sense as a diff ("so no
  per-channel commands are sent here"), delete it or rewrite it as a
  timeless invariant.
- Keep comments short. Prefer making the code obvious over explaining it.
  Name the current stack (`wgpui`), never the old one (`eframe`).

## Gotchas

- UI is `wgpui` (retained wgpu + winit) with `wgpui-kit` components. Backend selection follows wgpui's `wgpu` (Vulkan); there is no backend pinning in `Cargo.toml`.
- Config at `dirs::config_dir()/com.iken.siphon/config.json`, `VERSION = 2`: a file newer than `VERSION` is discarded (fresh default), unversioned files load as 0 and are stamped on next save. Only resolved channels persist — unknown logins surface an error and must not touch disk or subscribe.
- Logging: `RUST_LOG=info|debug`, targets `app single config gql hermes notifier tray update`. Release goes to stderr plus a capped rotating file (10 MB total, 5 files) under local app-data `com.iken.siphon/logs`; debug goes to stdout only and never touches disk.
- Single instance key `com.iken.siphon` (`com.iken.siphon.debug` in debug builds, which get a separate config dir too): a second launch wakes the primary via callback and exits.
- Clippy allows that look like mistakes are deliberate: `cast_*` for tray math, `missing_panics_doc`/`missing_errors_doc`/`too_many_lines`/`missing_const_for_fn`.
- Hiding to tray trims the working set (measured <10 MB resident in tray); open-memory is driver-dominated (~150 MB), don't chase it from the worker.
