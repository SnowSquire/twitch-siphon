//! GUI↔work wire protocol and work-thread state.
//!
//! The GUI thread is purely presentational: it renders the latest
//! [`Snapshot`] and sends [`UiIntent`]s. The work thread owns a [`Worker`]:
//! presentation state lives in [`SharedSnapshot`] (the one slot the GUI reads),
//! everything else (connection, subscriptions, background jobs) is private
//! to the work thread. Each [`SharedSnapshot::update`] wakes the foreground
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

use crate::balesh::{CheapRng, NanoId};
use crate::config::Config;
use crate::hermes::{Topic, WsStream};
use crate::http;
use crate::matcher::Matcher;
use crate::notifier;
use crate::tray::TrayAction;

/// One imperative UI mutation. Sent GUI→work; the work thread applies it,
/// persists it as a background job, and publishes the change.
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
    Available(crate::update::Release),
    Downloading(crate::update::Release),
}

/// Subscription state for one topic. Shared vocabulary: the work thread
/// sets it, the GUI badges it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    Pending,
    Connected,
    Failed,
}

/// One channel's live row: the last resolved detail plus both
/// subscription states. Lives only in [`Snapshot::channels`], keyed by
/// channel id; the work thread mutates rows in place and the GUI reads
/// them by id. Mute stamps and subscription protocol ids stay on the
/// [`Worker`]: they are never rendered.
#[derive(Clone)]
pub struct ChannelRow {
    pub channel: http::ChannelDetail,
    pub title: SubscriptionState,
    pub live: SubscriptionState,
}

impl ChannelRow {
    pub(crate) fn new(channel: http::ChannelDetail) -> Self {
        Self {
            channel,
            title: SubscriptionState::Pending,
            live: SubscriptionState::Pending,
        }
    }
}

/// Everything the GUI needs for one frame. Single source of truth for
/// presentation state: live rows live only here, mutated in place by
/// the work thread under short write locks (never held across an
/// `await`); the GUI clones the snapshot out when its seen version
/// lags.
#[derive(Clone)]
pub struct Snapshot {
    pub config: Config,
    pub connected: bool,
    pub conn_error: Option<String>,
    pub channels: HashMap<u64, ChannelRow>,
    pub pending: Vec<String>,
    pub error: String,
    pub update: UpdateStatus,
}

impl Default for Snapshot {
    /// Capacities are reserved once so steady-state updates reuse the
    /// allocations instead of regrowing the map per change.
    fn default() -> Self {
        Self {
            config: Config::default(),
            connected: false,
            conn_error: None,
            channels: HashMap::with_capacity(32),
            pending: Vec::new(),
            error: String::new(),
            update: UpdateStatus::Idle,
        }
    }
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
pub struct SharedSnapshot {
    inner: Arc<RwLock<Snapshot>>,
    version: Arc<AtomicU64>,
    tx: Sender<GuiEvent>,
}

impl SharedSnapshot {
    /// Creates the slot plus the pump wakeups: the receiver belongs to the
    /// foreground pump awaiting [`GuiEvent`]s.
    pub fn pair(state: Snapshot) -> (Self, kanal::AsyncReceiver<GuiEvent>) {
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
    /// current, so duplicates are harmless. Returns whatever the
    /// mutation computes, so callers can extract owned data in the same
    /// pass instead of locking twice.
    pub fn update<R>(&self, apply: impl FnOnce(&mut Snapshot) -> R) -> R {
        let result = apply(
            &mut self
                .inner
                .write()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        self.version.fetch_add(1, Ordering::Release);
        self.send(GuiEvent::Snapshot);
        result
    }

    /// Borrows the slot. Short critical sections only: never hold the
    /// guard across an `await` (clippy's `await_holding_lock` enforces
    /// this). A poisoned lock still reads; the snapshot matters more than
    /// the panic that poisoned it.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Snapshot> {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Current version, bumped by every [`SharedSnapshot::update`]. The pump
    /// skips the clone when its seen version is current; a store racing
    /// the check surfaces on the next pass via the accompanying poke.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Work→GUI events sharing the [`SharedSnapshot`] channel to the foreground
/// pump. `Snapshot` means the slot holds something newer; `Theme`
/// carries the mode the system watcher reported; `Tray` carries a tray or
/// single-instance action. Tray handling reloads the snapshot too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuiEvent {
    Snapshot,
    Theme(ThemeMode),
    Tray(TrayAction),
}

impl SharedSnapshot {
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
    AddResolved(String, anyhow::Result<Option<http::ChannelDetail>>),
    LiveRefreshed(u64, anyhow::Result<Vec<http::ChannelDetail>>),
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
pub(crate) struct ConnectOutcome {
    pub channels: Vec<http::ChannelDetail>,
    pub missing: Vec<u64>,
    pub socket: Option<WsStream>,
}

/// One in-flight job. `!Send` is fine: everything stays on the single
/// thread-per-core runtime thread.
pub(crate) type JobFuture = Pin<Box<dyn Future<Output = JobDone>>>;

/// Everything the work thread needs at startup. Moved in whole; the
/// channel ends are then owned by the run loop, the snapshot slot is
/// shared with the GUI thread.
pub struct WorkerInit {
    pub config_path: PathBuf,
    pub config: Config,
    pub ui_rx: kanal::Receiver<UiIntent>,
    pub tray_rx: kanal::Receiver<TrayAction>,
    pub shared: SharedSnapshot,
}
/// All mutable work-thread state, owned by the single work thread. The
/// `shared` slot is the presentation source of truth (the GUI holds a
/// clone of its `Arc`): live rows and subscription states live only
/// there, mutated in place. Every other field is private to this thread
/// and never rendered: mute stamps, subscription protocol ids,
/// connection, background jobs. `handle_job`/`handle_intent` mutate with
/// plain `&mut self`: jobs carry owned results back instead of sharing
/// borrows, and locks are never held across an `await`.
pub struct Worker {
    pub(crate) config_path: PathBuf,
    pub(crate) shared: SharedSnapshot,
    pub(crate) ui_rx: kanal::AsyncReceiver<UiIntent>,
    pub(crate) tray_rx: kanal::AsyncReceiver<TrayAction>,
    pub(crate) quiet_until: HashMap<u64, Instant>,
    pub(crate) sub_ids: HashMap<Topic, NanoId>,
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
    pub fn new(params: WorkerInit) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};

        let matcher = crate::hermes::build_matcher(&params.config.filtered_words);
        let worker = Self {
            config_path: params.config_path,
            shared: params.shared,
            ui_rx: params.ui_rx.to_async(),
            tray_rx: params.tray_rx.to_async(),
            quiet_until: HashMap::new(),
            sub_ids: HashMap::new(),
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

    pub(crate) fn set_error(&self, message: String) {
        self.shared.update(|state| {
            state.error = message;
        });
    }

    /// Records connection health after a lifecycle event: connected while
    /// the socket is live and welcomed, plus any error to show. Data
    /// mutations never call this: rows publish through their own updates,
    /// so unrelated edits leave health alone.
    pub(crate) fn publish_conn_health(&self, conn_error: Option<String>) {
        let connected = self.socket.is_some() && self.welcomed;
        self.shared.update(|snapshot| {
            snapshot.connected = connected;
            snapshot.conn_error = conn_error;
        });
    }

    pub(crate) fn queue_save(&mut self) {
        let path = self.config_path.clone();
        let config = self.shared.read().config.clone();
        self.jobs.push(Box::pin(async move {
            JobDone::SaveFinished(config.save(&path).await)
        }));
    }

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
    // Tray and single-instance actions arrive on the work thread but take
    // effect on the GUI thread; forwarding through the shared slot's channel
    // lets the foreground pump apply them in one place.
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
        let (shared, _gui_rx) = SharedSnapshot::pair(Snapshot::default());
        let worker = Worker::new(WorkerInit {
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
        let user = http::ChannelDetail {
            id: 7,
            login: "alice".to_owned(),
            display_name: "Alice".to_owned(),
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
                    .get(&7)
                    .expect("resolved channel should publish a row");
                assert_eq!(row.channel.stream_id, 9);
                assert_eq!(row.channel.viewers, Some(145));
                assert_eq!(row.channel.collaboration_viewers, Some(889));
            }
            assert!(worker.shared.read().channels.contains_key(&7));
            drive_one(&mut worker).await;

            let saved = saved_config(&path).await;
            assert_eq!(saved.channels.len(), 1);
            assert_eq!(saved.channels[0].login, "alice");
            assert_eq!(saved.channels[0].id, 7);
            compio::fs::remove_file(&path).await.ok();
        });
    }

    #[test]
    fn complete_add_login_failure_leaves_disk_untouched() {
        // Both failure kinds — unknown login (`Ok(None)`) and transport
        // error (`Err`) — must surface an error without touching disk or
        // queueing a save.
        for (name, login, transport_error) in
            [("typo", "typo", false), ("resolve-err", "alice", true)]
        {
            let (mut worker, path) = test_worker(Config::default(), name);

            block_on(async {
                compio::fs::remove_file(&path).await.ok();
                worker.request_add_login(login.to_owned());
                worker.jobs.clear();
                let result: anyhow::Result<Option<http::ChannelDetail>> =
                    if transport_error {
                        Err(anyhow::anyhow!("boom"))
                    } else {
                        Ok(None)
                    };
                worker
                    .complete_add_login(login.to_owned(), result)
                    .await;

                assert!(
                    compio::fs::metadata(&path).await.is_err(),
                    "{name}: failures must not create a config file"
                );
                assert!(worker.jobs.is_empty(), "{name}");
                let state = worker.shared.read();
                assert!(state.pending.is_empty(), "{name}");
                assert!(!state.error.is_empty(), "{name}");
                if transport_error {
                    assert!(
                        state.error.contains("failed to resolve alice"),
                        "{name}: was {}",
                        state.error
                    );
                }
            });
        }
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
