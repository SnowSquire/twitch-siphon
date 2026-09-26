//! GUI↔work wire protocol and work-thread state.
//!
//! The GUI thread is purely presentational: it renders the latest
//! [`AppState`] and sends [`UiIntent`]s. The work thread owns a [`Worker`]:
//! presentation state lives in [`SharedFrame`] (the one slot the GUI reads),
//! everything else (connection, subscriptions, background jobs) is private
//! to the work thread. Each [`SharedFrame::update`] wakes the foreground
//! pump through the same channel, so state and wakeup never drift apart.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use futures_util::stream::FuturesUnordered;
use kanal::Sender;
use wgpui_kit::component::theme::ThemeMode;

use crate::balesh::{CheapRng, Topic};
use crate::config::Config;
use crate::hermes::{Sub, WsStream};
use crate::http;
use crate::matcher::Matcher;
use crate::notifier;
use crate::tray::TrayAction;

/// One imperative UI mutation. Sent GUI→work; the work thread applies it,
/// persists it as a background job, and publishes a fresh [`AppState`].
/// Adds carry only the login (the only thing the UI knows); removes carry
/// the resolved id.
pub enum UiIntent {
    AddLogin(String),
    RemoveChannel(u64),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
    AddFilteredWord(String),
    RemoveFilteredWord(usize),
    ApplyUpdate,
    ClearError,
}

/// Update offer shown in the top bar. The GUI renders it and sends
/// [`UiIntent::ApplyUpdate`]; the work thread downloads and hands off to
/// the installer. A click never fires from a toast.
#[derive(Clone, Debug, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    Current,
    Available(AvailableUpdate),
    Downloading(AvailableUpdate),
}

/// A newer release: display version plus where to get it. `msi_url` is
/// `None` on portable builds or when the release carries no installer,
/// in which case the button opens `page_url` instead.
#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub version: String,
    pub msi_url: Option<String>,
    pub page_url: String,
}

/// One channel's subscription state for the GUI row, plus the resolved
/// detail the expanded row shows.
#[derive(Clone)]
pub struct ChannelStatus {
    pub channel_id: u64,
    pub login: String,
    pub display_name: String,
    pub title_status: SubStatus,
    pub live_status: SubStatus,
    pub stream_id: u64,
    pub stream_title: Option<String>,
    /// Milliseconds since the unix epoch; `None` when offline.
    pub stream_start: Option<i64>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    pub game: Option<String>,
    pub game_id: Option<u64>,
}

#[derive(Clone, Copy)]
pub enum SubStatus {
    Pending,
    Connected,
    Failed,
}

/// Everything the GUI needs for one frame. This is the single source of
/// truth for presentation state: the work thread mutates it under short
/// write locks (never held across an `await`) and the GUI clones it out
/// when its seen version lags.
#[derive(Clone, Default)]
pub struct AppState {
    pub config: Config,
    pub connected: bool,
    pub conn_error: Option<String>,
    pub channels: Vec<ChannelStatus>,
    pub pending: Vec<String>,
    pub error: String,
    pub update: UpdateStatus,
}

/// Latest-only work→GUI snapshot slot. The work thread mutates it in place
/// on every change; each mutation wakes the foreground pump through the
/// bundled channel, so state and wakeup never drift apart. The GUI thread
/// clones the snapshot out when its seen version lags. Intermediate states
/// vanish instead of queueing, so a stalled GUI never builds backlog.
/// The channel is unbounded because tray actions are lossless; frame
/// traffic is user actions and connection events, never a hot loop, so no
/// backlog builds in practice.
#[derive(Clone)]
pub struct SharedFrame {
    inner: Arc<RwLock<AppState>>,
    version: Arc<AtomicU64>,
    tx: Sender<GuiEvent>,
}

impl SharedFrame {
    /// Creates the slot plus the pump wakeups: the receiver belongs to the
    /// foreground pump awaiting [`GuiEvent`]s.
    pub fn pair(state: AppState) -> (Self, kanal::AsyncReceiver<GuiEvent>) {
        let (tx, rx) = kanal::unbounded();
        (
            Self {
                inner: Arc::new(RwLock::new(state)),
                version: Arc::new(AtomicU64::new(0)),
                tx,
            },
            rx.to_async(),
        )
    }

    /// Mutates the slot in place, marks it newer, and wakes the pump.
    /// Short critical sections only: never hold the guard across an
    /// `await`. A poisoned lock still applies the write; the snapshot
    /// matters more than the panic that poisoned it. Wakeups are
    /// idempotent: the pump skips the clone when its seen version is
    /// current, so duplicates are harmless.
    pub fn update(&self, apply: impl FnOnce(&mut AppState)) {
        apply(
            &mut self
                .inner
                .write()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        self.version.fetch_add(1, Ordering::Release);
        self.send(GuiEvent::Frame);
    }

    /// Borrows the slot. Short critical sections only: never hold the
    /// guard across an `await` (clippy's `await_holding_lock` enforces
    /// this). A poisoned lock still reads; the snapshot matters more than
    /// the panic that poisoned it.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, AppState> {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Current version, bumped by every [`SharedFrame::update`]. The pump
    /// skips the clone when its seen version is current; a store racing
    /// the check surfaces on the next pass via the accompanying poke.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Work→GUI events sharing the [`SharedFrame`] channel to the foreground
/// pump. `Frame` means the snapshot slot holds something newer; `Theme`
/// carries the mode the system watcher reported; `Tray` carries a tray or
/// single-instance action. Tray handling reloads the frame too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuiEvent {
    Frame,
    Theme(ThemeMode),
    Tray(TrayAction),
}

impl SharedFrame {
    fn send(&self, event: GuiEvent) {
        if self.tx.send(event).is_err() {
            log::info!(target: "app", "gui channel closed");
        }
    }

    /// Forwards a tray or single-instance action to the foreground pump.
    pub fn notify_tray(&self, action: TrayAction) {
        self.send(GuiEvent::Tray(action));
    }

    /// Forwards a system theme report to the foreground pump.
    pub fn notify_theme(&self, mode: ThemeMode) {
        self.send(GuiEvent::Theme(mode));
    }
}

/// One finished background job: a channel resolve, a persisted save, a
/// queued toast, a shown toast, an update step, or a connection attempt.
/// All kinds share the queue; completions dispatch in `handle_job`, which
/// applies them to the [`Worker`] with plain `&mut` access.
pub(crate) enum JobDone {
    AddResolved(String, anyhow::Result<Option<http::ResolvedChannel>>),
    LiveRefreshed(u64, anyhow::Result<Vec<http::ResolvedChannel>>),
    SaveFinished(anyhow::Result<()>),
    ToastShown,
    UpdateCheckFinished(anyhow::Result<crate::update::Release>),
    UpdateCheckDue,
    UpdateDownloadFinished(anyhow::Result<PathBuf>),
    ConnectFinished(Box<ConnectOutcome>),
}

/// Outcome of one connection attempt: the fresh baseline (with the ids
/// that no longer resolve) plus the live socket, if the handshake
/// succeeded. Failures reconnect with whatever the sync established.
pub(crate) enum ConnectOutcome {
    Connected {
        channels: Vec<http::ResolvedChannel>,
        missing: Vec<u64>,
        socket: Option<WsStream>,
    },
}

/// One in-flight job. `!Send` is fine: everything stays on the single
/// thread-per-core runtime thread.
pub(crate) type JobFuture = Pin<Box<dyn Future<Output = JobDone>>>;

/// Everything the work thread needs at startup. Moved in whole; the
/// channel ends are then owned by the run loop, the snapshot slot is
/// shared with the GUI thread.
pub struct WorkerParams {
    pub config_path: PathBuf,
    pub config: Config,
    pub ui_rx: kanal::Receiver<UiIntent>,
    pub tray_rx: kanal::Receiver<TrayAction>,
    pub shared: SharedFrame,
}
/// All mutable work-thread state, owned by the single work thread. The
/// `shared` slot is the presentation source of truth (the GUI holds a
/// clone of its `Arc`); every other field is private to this thread.
/// `handle_job`/`handle_intent` mutate with plain `&mut self`: jobs carry
/// owned results back instead of sharing borrows, and locks are never
/// held across an `await`.
pub struct Worker {
    pub(crate) config_path: PathBuf,
    pub(crate) shared: SharedFrame,
    pub(crate) ui_rx: kanal::AsyncReceiver<UiIntent>,
    pub(crate) tray_rx: kanal::AsyncReceiver<TrayAction>,
    pub(crate) channels: HashMap<u64, http::ResolvedChannel>,
    pub(crate) subs: HashMap<Topic, Sub>,
    pub(crate) matcher: Matcher,
    pub(crate) socket: Option<WsStream>,
    pub(crate) welcomed: bool,
    pub(crate) connecting: bool,
    pub(crate) keepalive_secs: u64,
    pub(crate) last_message: Instant,
    pub(crate) reconnect_at: Instant,
    pub(crate) reconnect_attempt: u32,
    pub(crate) rng: CheapRng,
    pub(crate) jobs: FuturesUnordered<JobFuture>,
}

impl Worker {
    pub fn new(params: WorkerParams) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};

        let matcher = crate::hermes::build_matcher(&params.config.filtered_words);
        let worker = Self {
            config_path: params.config_path,
            shared: params.shared,
            ui_rx: params.ui_rx.to_async(),
            tray_rx: params.tray_rx.to_async(),
            channels: HashMap::new(),
            subs: HashMap::new(),
            matcher,
            socket: None,
            welcomed: false,
            connecting: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            rng: CheapRng::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0x853c_49e6_748f_ea9b, |elapsed| elapsed.as_nanos() as u64),
            ),
            jobs: FuturesUnordered::new(),
        };
        worker.shared.update(|state| {
            state.config = params.config;
        });
        worker
    }

    /// Publishes the slot without changing it: for startup, so the GUI
    /// (which loaded version 0 before this thread started) reloads.
    pub fn publish(&self) {
        self.shared.update(|_| {});
    }

    pub(crate) fn set_error(&self, message: String) {
        self.shared.update(|state| {
            state.error = message;
        });
    }

    pub(crate) fn clear_error(&self) {
        self.shared.update(|state| {
            state.error.clear();
        });
    }

    /// Rebuilds the channel rows from the tracked users and subscription
    /// states, records connection health, and publishes. Called after
    /// every change on either side; cheap enough to run unconditionally.
    pub(crate) fn refresh_views(&self, conn_error: Option<String>) {
        let mut channels: Vec<ChannelStatus> = self
            .channels
            .iter()
            .map(|(id, user)| ChannelStatus {
                channel_id: *id,
                login: user.channel_name.clone(),
                display_name: user.channel_display_name.clone(),
                title_status: self.status_for(crate::balesh::Topic::BroadcastSettingsUpdate(*id)),
                live_status: self.status_for(crate::balesh::Topic::VideoPlaybackById(*id)),
                stream_id: user.stream_id,
                stream_title: user.stream_title.clone(),
                stream_start: user.stream_start,
                viewers: user.viewers,
                collaboration_viewers: user.collaboration_viewers,
                game: user
                    .game
                    .as_ref()
                    .map(|game| game.display_name.clone())
                    .filter(|name| !name.is_empty())
                    .or_else(|| user.game.as_ref().map(|game| game.name.clone())),
                game_id: user.game.as_ref().map(|game| game.id),
            })
            .collect();
        channels.sort_by(|left, right| left.login.cmp(&right.login));
        let connected = self.socket.is_some() && self.welcomed;
        self.shared.update(|state| {
            state.channels = channels;
            state.connected = connected;
            state.conn_error = conn_error;
        });
    }

    /// Persists the latest config without stalling the loop: the config
    /// is cloned out from under a short read (the only thing serialization
    /// needs) and the write runs as a job. Failures surface as an error;
    /// successes change nothing — the mutation already published.
    pub(crate) fn queue_save(&mut self) {
        let path = self.config_path.clone();
        let config = self.shared.read().config.clone();
        self.jobs.push(Box::pin(async move {
            JobDone::SaveFinished(config.save(&path).await)
        }));
    }

    /// Shows a toast without stalling the loop: resolving the avatar and
    /// handing off parks only this job, so intents and socket traffic keep
    /// flowing while the image downloads.
    pub(crate) fn queue_toast(
        &mut self,
        summary: String,
        body: String,
        sound: bool,
        image: Option<String>,
        login: Option<String>,
    ) {
        self.jobs.push(Box::pin(async move {
            notifier::show(summary, body, sound, image, login).await;
            JobDone::ToastShown
        }));
    }

    pub(crate) fn forward_tray(&self, action: TrayAction) {
        self.shared.notify_tray(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Channel;
    use futures_util::StreamExt as _;

    fn test_worker(config: Config, name: &str) -> (Worker, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("siphon-worker-{name}-{}.json", std::process::id()));
        let (_ui_tx, ui_rx) = kanal::unbounded();
        let (_tray_tx, tray_rx) = kanal::unbounded();
        let (shared, _gui_rx) = SharedFrame::pair(AppState::default());
        let worker = Worker::new(WorkerParams {
            config_path: path.clone(),
            config,
            ui_rx,
            tray_rx,
            shared,
        });
        (worker, path)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn channel(login: &str, id: u64) -> Channel {
        Channel {
            login: login.to_owned(),
            id,
            display_name: None,
        }
    }

    async fn saved_config(path: &std::path::Path) -> Config {
        let bytes = compio::fs::read(path)
            .await
            .expect("saved config should be readable");
        serde_json::from_slice(&bytes).expect("saved config should parse")
    }

    /// Drives one queued job to completion through the worker's own
    /// completion handler.
    async fn drive_one(worker: &mut Worker) {
        let done = worker.jobs.next().await.expect("a job should be queued");
        worker.handle_job(done).await;
    }

    #[test]
    fn remove_channel_publishes_and_saves() {
        let config = Config {
            channels: vec![channel("alice", 1), channel("bob", 2)],
            ..Config::default()
        };
        let (mut worker, path) = test_worker(config, "remove");

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            worker.apply_remove_channel(1).await;

            {
                let state = worker.shared.read();
                assert_eq!(
                    state
                        .config
                        .channels
                        .iter()
                        .map(|c| c.login.as_str())
                        .collect::<Vec<_>>(),
                    ["bob"]
                );
            }
            drive_one(&mut worker).await;

            let saved = saved_config(&path).await;
            assert_eq!(
                saved
                    .channels
                    .iter()
                    .map(|c| c.login.as_str())
                    .collect::<Vec<_>>(),
                ["bob"]
            );
            compio::fs::remove_file(&path).await.ok();
        });
    }

    #[test]
    fn add_login_validates_and_dedupes() {
        let (mut worker, _path) = test_worker(Config::default(), "validate");

        assert_eq!(
            worker.request_add_login("  ALICE ".to_owned()),
            Some("alice".to_owned())
        );
        {
            let state = worker.shared.read();
            assert_eq!(state.pending, ["alice"]);
            assert!(state.error.is_empty());
        }

        assert_eq!(worker.request_add_login("alice".to_owned()), None);
        assert_eq!(worker.request_add_login("ALICE".to_owned()), None);
        assert_eq!(worker.request_add_login("   ".to_owned()), None);
        let state = worker.shared.read();
        assert_eq!(state.pending, ["alice"]);
    }

    #[test]
    fn complete_add_login_persists_resolved_channel() {
        let (mut worker, path) = test_worker(Config::default(), "add");
        let user = http::ResolvedChannel {
            channel_id: 7,
            channel_name: "alice".to_owned(),
            channel_display_name: "Alice".to_owned(),
            profile_image_url: String::new(),
            stream_id: 9,
            stream_title: None,
            stream_start: None,
            stream_created_at: None,
            viewers: Some(145),
            collaboration_viewers: Some(889),
            game: None,
            live: false,
        };

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            worker.request_add_login("alice".to_owned());
            // Drop the resolve job the request queued: this test drives
            // the completion directly with a canned user.
            worker.jobs.clear();
            worker
                .complete_add_login("alice".to_owned(), Ok(Some(user)))
                .await;

            {
                let state = worker.shared.read();
                assert!(state.pending.is_empty());
                assert_eq!(state.config.channels.len(), 1);
                // The row must publish from the tracked user, or the GUI
                // keeps the Pending fallback despite an accepted subscribe.
                let row = state
                    .channels
                    .iter()
                    .find(|c| c.channel_id == 7)
                    .expect("resolved channel should publish a row");
                assert_eq!(row.stream_id, 9);
                assert_eq!(row.viewers, Some(145));
                assert_eq!(row.collaboration_viewers, Some(889));
            }
            assert!(worker.channels.contains_key(&7));
            drive_one(&mut worker).await;

            let saved = saved_config(&path).await;
            assert_eq!(saved.channels.len(), 1);
            assert_eq!(saved.channels[0].login, "alice");
            assert_eq!(saved.channels[0].id, 7);
            compio::fs::remove_file(&path).await.ok();
        });
    }

    #[test]
    fn complete_add_login_surfaces_typo_without_touching_disk() {
        let (mut worker, path) = test_worker(Config::default(), "typo");

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            worker.request_add_login("typo".to_owned());
            worker.jobs.clear();
            worker.complete_add_login("typo".to_owned(), Ok(None)).await;

            assert!(
                compio::fs::metadata(&path).await.is_err(),
                "typos must not create a config file"
            );
            assert!(worker.jobs.is_empty());
            let state = worker.shared.read();
            assert!(state.pending.is_empty());
            assert!(!state.error.is_empty());
        });
    }

    #[test]
    fn complete_add_login_surfaces_transport_error_without_touching_disk() {
        let (mut worker, path) = test_worker(Config::default(), "resolve-err");

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            worker.request_add_login("alice".to_owned());
            worker.jobs.clear();
            worker
                .complete_add_login("alice".to_owned(), Err(anyhow::anyhow!("boom")))
                .await;

            assert!(
                compio::fs::metadata(&path).await.is_err(),
                "failures must not create a config file"
            );
            assert!(worker.jobs.is_empty());
            let state = worker.shared.read();
            assert!(state.error.contains("failed to resolve alice"));
        });
    }

    #[test]
    fn prune_ids_removes_missing_and_surfaces_error() {
        let config = Config {
            channels: vec![channel("alice", 1), channel("bob", 2)],
            ..Config::default()
        };
        let (mut worker, path) = test_worker(config, "prune");

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            let config = worker.shared.read().config.clone();
            config.save(&path).await.unwrap();
            worker.apply_prune_ids(&[1]);

            drive_one(&mut worker).await;
            let saved = saved_config(&path).await;
            assert_eq!(
                saved
                    .channels
                    .iter()
                    .map(|c| c.login.as_str())
                    .collect::<Vec<_>>(),
                ["bob"]
            );
            {
                let state = worker.shared.read();
                assert_eq!(state.config.channels.len(), 1);
                assert!(state.error.contains("alice"), "error was: {}", state.error);
            }
            compio::fs::remove_file(&path).await.ok();
        });
    }
}
