//! GUI↔work wire protocol and work-thread state.
//!
//! The GUI thread is purely presentational: it renders the latest
//! [`FrameState`] and sends [`UiIntent`]s. The work thread owns everything
//! else (config, persistence, the hermes session) inside [`WorkState`], which
//! lives on that single thread — no `Arc`, no `Mutex` for app state. The only
//! cross-thread primitives are the [`kanal`] queues below plus
//! [`GuiWaker`], which is a doorbell, not state.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock, RwLock};

use kanal::{Receiver, Sender};

use crate::config::{Channel, Config};
use crate::gql;
use crate::hermes;
use crate::tray::TrayAction;

/// One imperative UI mutation. Sent GUI→work instead of diffing whole
/// settings snapshots; the work thread persists, forwards to hermes, and
/// pushes a fresh [`FrameState`] back. Adds carry only the login (the only
/// thing the UI knows); removes carry the resolved id.
pub enum UiIntent {
    AddLogin(String),
    RemoveChannel(u64),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
    ClearError,
}

/// Everything the GUI needs for one frame. Pushed work→GUI on every change;
/// the GUI keeps the latest and renders it. All fields are `Clone` so the
/// snapshot crosses the channel by value.
#[derive(Clone, Default)]
pub struct FrameState {
    pub config: Config,
    pub status: Option<hermes::StatusSnapshot>,
    pub error: String,
}

/// The single remaining shared primitive: a wake handle, not app state.
/// Set once by the GUI thread; poked by the work thread after every push so
/// a hidden or idle event loop notices. Channels alone cannot wake winit.
#[derive(Clone)]
pub struct GuiWaker {
    ctx: Arc<OnceLock<egui::Context>>,
}

impl GuiWaker {
    pub fn new() -> Self {
        Self {
            ctx: Arc::new(OnceLock::new()),
        }
    }

    pub fn set(&self, ctx: egui::Context) {
        let _ = self.ctx.set(ctx);
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
}

/// Everything the work thread needs at startup. Moved in whole; the
/// receivers/ends are then owned by the runtime tasks.
pub struct WorkContext {
    pub config_path: PathBuf,
    pub config: Config,
    pub ui_rx: Receiver<UiIntent>,
    pub frame_tx: Sender<FrameState>,
    pub tray_tx: Sender<TrayAction>,
    pub tray_events: Receiver<TrayAction>,
    pub waker: GuiWaker,
}

/// All mutable app state, owned by the work thread alone. Every method is
/// plain single-threaded code; tasks on the runtime share it through
/// `Rc<RefCell<WorkState>>` with short scoped borrows, never held across
/// an `await`.
pub struct WorkState {
    config_path: Rc<PathBuf>,
    config: Rc<Config>,
    error: String,
    status: Option<hermes::StatusSnapshot>,
    session_tx: Sender<hermes::Command>,
    pub(crate) ui_rx: kanal::AsyncReceiver<UiIntent>,
    frame_tx: Sender<FrameState>,
    tray_tx: Sender<TrayAction>,
    pub(crate) tray_events: kanal::AsyncReceiver<TrayAction>,
    waker: GuiWaker,
    pub(crate) pending_adds: Arc<RwLock<Vec<String>>>,
}

struct PendingSave {
    path: Rc<PathBuf>,
    config: Rc<Config>,
    forward: hermes::Command,
}

impl WorkState {
    pub fn new(ctx: WorkContext, session_tx: Sender<hermes::Command>) -> Self {
        Self {
            config_path: Rc::new(ctx.config_path),
            config: Rc::new(ctx.config),
            error: String::new(),
            status: None,
            session_tx,
            ui_rx: ctx.ui_rx.to_async(),
            frame_tx: ctx.frame_tx,
            tray_tx: ctx.tray_tx,
            tray_events: ctx.tray_events.to_async(),
            pending_adds: Arc::new(RwLock::new(Vec::new())),
            waker: ctx.waker,
        }
    }

    pub fn clear_error(&mut self) {
        self.error.clear();
        self.push_frame();
    }

    pub fn config_snapshot(&self) -> Rc<Config> {
        Rc::clone(&self.config)
    }

    pub fn set_status(&mut self, status: hermes::StatusSnapshot) {
        self.status = Some(status);
        self.push_frame();
    }

    pub fn forward_tray(&self, action: TrayAction) {
        let _ = self.tray_tx.send(action);
        self.wake();
    }

    /// Validates an add request. Returns the normalized login when a resolve
    /// is worthwhile; duplicates, in-flight resolves, and empty input are
    /// silently ignored.
    pub fn begin_add_login(&mut self, login: &str) -> Option<String> {
        let login = login.trim().to_lowercase();
        if login.is_empty()
            || self
                .config
                .channels
                .iter()
                .any(|channel| channel.login.eq_ignore_ascii_case(&login))
        {
            return None;
        }
        if let Ok(pending) = self.pending_adds.read()
            && pending.iter().any(|item| item.eq_ignore_ascii_case(&login))
        {
            return None;
        }
        if let Ok(mut v) = self.pending_adds.write() {
            v.push(login.clone());
        }
        self.error.clear();
        self.push_frame();
        Some(login)
    }

    /// Applies a finished resolve: resolved channels persist to disk, unknown
    /// logins never enter the list and surface an error instead. Takes
    /// `work` instead of `&mut self` so the `RefCell` borrow is never held
    /// across the disk write's await.
    pub async fn finish_add_login(
        work: &Rc<RefCell<Self>>,
        login: String,
        result: Result<Vec<gql::User>, gql::Error>,
    ) {
        let pending = work.borrow_mut().stage_finish_add_login(&login, result);
        let finished_without_save = pending.is_none();
        Self::commit_save(work, pending).await;
        if finished_without_save {
            work.borrow().push_frame();
        }
    }

    pub async fn apply_remove_channel(work: &Rc<RefCell<Self>>, id: u64) {
        let pending = work.borrow_mut().stage_remove_channel(id);
        Self::commit_save(work, Some(pending)).await;
    }

    /// Drops ids the session found unresolvable (deleted/renamed): they
    /// disappear from the persisted list with an error, instead of lingering.
    pub async fn apply_prune_ids(work: &Rc<RefCell<Self>>, ids: &[u64]) {
        let staged = work.borrow_mut().stage_prune_ids(ids);
        let Some((path, config)) = staged else { return };
        match config.save(&path).await {
            Ok(()) => {
                work.borrow_mut()
                    .set_error("a channel no longer resolves and was removed");
            }
            Err(error) => work.borrow_mut().set_error(error.to_string()),
        }
    }

    /// Drops one login the session found unresolvable: it disappears from
    /// the persisted list (if present) and surfaces an error.
    pub async fn apply_prune_login(work: &Rc<RefCell<Self>>, login: &str) {
        let staged = work.borrow_mut().stage_prune_login(login);
        let Some((path, config)) = staged else {
            work.borrow_mut()
                .set_error(format!("channel {login} not found"));
            return;
        };
        match config.save(&path).await {
            Ok(()) => {
                work.borrow_mut()
                    .set_error(format!("channel {login} not found"));
            }
            Err(error) => work.borrow_mut().set_error(error.to_string()),
        }
    }

    pub async fn apply_notify_title_changes(work: &Rc<RefCell<Self>>, value: bool) {
        let pending = work.borrow_mut().stage_notify_title_changes(value);
        Self::commit_save(work, Some(pending)).await;
    }

    pub async fn apply_sound(work: &Rc<RefCell<Self>>, value: bool) {
        let pending = work.borrow_mut().stage_sound(value);
        Self::commit_save(work, Some(pending)).await;
    }

    fn stage_save(&mut self, forward: hermes::Command) -> PendingSave {
        Rc::make_mut(&mut self.config).version = Config::VERSION;
        PendingSave {
            path: Rc::clone(&self.config_path),
            config: Rc::clone(&self.config),
            forward,
        }
    }

    /// Shared tail of every persisting intent: writes the snapshot with no
    /// borrow held, then forwards + frames on success or surfaces the error.
    async fn commit_save(work: &Rc<RefCell<Self>>, pending: Option<PendingSave>) {
        let Some(save) = pending else { return };
        match save.config.save(&save.path).await {
            Ok(()) => {
                let work = work.borrow();
                work.forward(save.forward);
                work.push_frame();
            }
            Err(error) => work.borrow_mut().set_error(error.to_string()),
        }
    }

    /// Synchronous in-memory half of [`Self::finish_add_login`]: mutates and
    /// snapshots under one short borrow, leaving the disk write to
    /// [`Self::commit_save`]. Paths that persist nothing (duplicates finish
    /// silently, unknown logins surface an error) yield `None`.
    fn stage_finish_add_login(
        &mut self,
        login: &str,
        result: Result<Vec<gql::User>, gql::Error>,
    ) -> Option<PendingSave> {
        match result {
            Ok(users) => {
                if let Ok(mut v) = self.pending_adds.write() {
                    v.retain(|item| !item.eq_ignore_ascii_case(login));
                }
                let user = users
                    .into_iter()
                    .find(|user| user.channel_name.eq_ignore_ascii_case(login));
                let Some(user) = user else {
                    log::info!(
                        target: "config",
                        "gql returned no channel named {login}, is it a typo?"
                    );
                    self.set_error(format!("channel {login} not found"));
                    return None;
                };
                log::info!(
                    target: "config",
                    "resolved {} to id {} ({})",
                    login,
                    user.channel_id,
                    user.channel_display_name
                );
                if self
                    .config
                    .channels
                    .iter()
                    .any(|existing| existing.id == user.channel_id)
                {
                    return None;
                }
                let channel = Channel {
                    login: user.channel_name.clone(),
                    id: user.channel_id,
                    display_name: Some(user.channel_display_name),
                };
                let forward_login = channel.login.clone();
                Rc::make_mut(&mut self.config).channels.push(channel);
                Some(self.stage_save(hermes::Command::AddChannel(forward_login)))
            }
            Err(error) => {
                if let Ok(mut v) = self.pending_adds.write() {
                    v.retain(|item| !item.eq_ignore_ascii_case(login));
                }
                log::info!(target: "config", "channel resolution failed: {error}");
                self.set_error(format!("failed to resolve {login}: {error}"));
                None
            }
        }
    }

    fn stage_remove_channel(&mut self, id: u64) -> PendingSave {
        Rc::make_mut(&mut self.config)
            .channels
            .retain(|channel| channel.id != id);
        self.stage_save(hermes::Command::RemoveChannel(id))
    }

    /// Synchronous in-memory half of [`Self::apply_prune_ids`]: removes any
    /// persisted channels whose ids are gone, snapshots for the write. Yields
    /// `None` when nothing changed, so no disk touch happens.
    fn stage_prune_ids(&mut self, ids: &[u64]) -> Option<(Rc<PathBuf>, Rc<Config>)> {
        let before = self.config.channels.len();
        Rc::make_mut(&mut self.config)
            .channels
            .retain(|channel| !ids.contains(&channel.id));
        if self.config.channels.len() == before {
            return None;
        }
        Rc::make_mut(&mut self.config).version = Config::VERSION;
        Some((Rc::clone(&self.config_path), Rc::clone(&self.config)))
    }

    /// Synchronous in-memory half of [`Self::apply_prune_login`]: removes
    /// any persisted channel with this login. Yields `None` when nothing
    /// changed (the login was never persisted, e.g. a typo add).
    fn stage_prune_login(&mut self, login: &str) -> Option<(Rc<PathBuf>, Rc<Config>)> {
        let before = self.config.channels.len();
        Rc::make_mut(&mut self.config)
            .channels
            .retain(|channel| !channel.login.eq_ignore_ascii_case(login));
        if self.config.channels.len() == before {
            return None;
        }
        Rc::make_mut(&mut self.config).version = Config::VERSION;
        Some((Rc::clone(&self.config_path), Rc::clone(&self.config)))
    }

    fn stage_notify_title_changes(&mut self, value: bool) -> PendingSave {
        Rc::make_mut(&mut self.config).notify_title_changes = value;
        self.stage_save(hermes::Command::SetNotifyTitleChanges(value))
    }

    fn stage_sound(&mut self, value: bool) -> PendingSave {
        Rc::make_mut(&mut self.config).sound = value;
        self.stage_save(hermes::Command::SetSound(value))
    }

    fn forward(&self, command: hermes::Command) {
        if let Err(error) = self.session_tx.send(command) {
            // The session owns the receiver for the runtime lifetime, so a
            // send failure is unexpected; surface it rather than dropping it.
            log::info!(target: "config", "hermes command send failed: {error}");
        }
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.error = message.into();
        self.push_frame();
    }

    pub fn push_frame(&self) {
        let _ = self.frame_tx.send(FrameState {
            config: self.config.as_ref().clone(),
            status: self.status.clone(),
            error: self.error.clone(),
        });
        self.wake();
    }

    fn wake(&self) {
        self.waker.repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_work(
        config: Config,
        path: PathBuf,
    ) -> (Rc<RefCell<WorkState>>, Receiver<hermes::Command>) {
        let (_ui_tx, ui_rx) = kanal::unbounded();
        let (frame_tx, _frame_rx) = kanal::unbounded();
        let (tray_tx, _tray_rx) = kanal::unbounded();
        let (_tray_event_tx, tray_events) = kanal::unbounded();
        let (session_tx, session_rx) = kanal::unbounded();
        let work = Rc::new(RefCell::new(WorkState::new(
            WorkContext {
                config_path: path,
                config,
                ui_rx,
                frame_tx,
                tray_tx,
                tray_events,
                waker: GuiWaker::new(),
            },
            session_tx,
        )));
        (work, session_rx)
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

    /// Async read of the persisted config, so tests never block the
    /// thread-per-core runtime on synchronous file IO.
    async fn saved_config(path: &Path) -> Config {
        let bytes = compio::fs::read(path)
            .await
            .expect("saved config should be readable");
        serde_json::from_slice(&bytes).expect("saved config should parse")
    }

    async fn file_exists(path: &Path) -> bool {
        compio::fs::metadata(path).await.is_ok()
    }

    #[test]
    fn remove_channel_persists_and_forwards() {
        let path =
            std::env::temp_dir().join(format!("siphon-state-remove-{}.json", std::process::id()));
        let config = Config {
            channels: vec![channel("alice", 1), channel("bob", 2)],
            ..Config::default()
        };
        let (work, session_rx) = test_work(config, path.clone());

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            WorkState::apply_remove_channel(&work, 1).await;

            let saved = saved_config(&path).await;
            assert_eq!(
                saved
                    .channels
                    .iter()
                    .map(|c| c.login.as_str())
                    .collect::<Vec<_>>(),
                ["bob"]
            );
            let command = session_rx
                .try_recv()
                .expect("channel open")
                .expect("command forwarded");
            assert!(matches!(command, hermes::Command::RemoveChannel(id) if id == 1));
            compio::fs::remove_file(&path).await.ok();
        });
    }

    #[test]
    fn finish_add_login_persists_resolved_channel() {
        let path =
            std::env::temp_dir().join(format!("siphon-state-add-{}.json", std::process::id()));
        let (work, session_rx) = test_work(Config::default(), path.clone());
        let user = gql::User {
            channel_id: 7,
            channel_name: "alice".to_owned(),
            channel_display_name: "Alice".to_owned(),
            profile_image_url: String::new(),
            stream_id: 9,
            stream_title: None,
            stream_start: None,
            game: None,
            live: false,
        };

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            WorkState::finish_add_login(&work, "alice".to_owned(), Ok(vec![user])).await;

            let saved = saved_config(&path).await;
            assert_eq!(saved.channels.len(), 1);
            assert_eq!(saved.channels[0].login, "alice");
            assert_eq!(saved.channels[0].id, 7);
            let command = session_rx
                .try_recv()
                .expect("channel open")
                .expect("command forwarded");
            assert!(matches!(command, hermes::Command::AddChannel(login) if login == "alice"));
            compio::fs::remove_file(&path).await.ok();
        });
    }

    #[test]
    fn finish_add_login_surfaces_error_without_touching_disk() {
        let path =
            std::env::temp_dir().join(format!("siphon-state-typo-{}.json", std::process::id()));
        let (frame_tx, frame_rx) = kanal::unbounded();
        let (tray_tx, _tray_rx) = kanal::unbounded();
        let (_ui_tx, ui_rx) = kanal::unbounded();
        let (_tray_event_tx, tray_events) = kanal::unbounded();
        let (session_tx, session_rx) = kanal::unbounded();
        let work = Rc::new(RefCell::new(WorkState::new(
            WorkContext {
                config_path: path.clone(),
                config: Config::default(),
                ui_rx,
                frame_tx,
                tray_tx,
                tray_events,
                waker: GuiWaker::new(),
            },
            session_tx,
        )));

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            WorkState::finish_add_login(&work, "typo".to_owned(), Ok(Vec::new())).await;

            assert!(
                !file_exists(&path).await,
                "typos must not create a config file"
            );
            assert!(
                session_rx.is_empty(),
                "unresolvable logins must not reach the session"
            );
            let frame = frame_rx
                .try_recv()
                .expect("channel open")
                .expect("error frame pushed");
            assert!(
                !frame.error.is_empty(),
                "unresolvable logins must surface an error"
            );
        });
    }

    #[test]
    fn prune_ids_removes_missing_and_surfaces_error() {
        let path =
            std::env::temp_dir().join(format!("siphon-state-prune-{}.json", std::process::id()));
        let config = Config {
            channels: vec![channel("alice", 1), channel("bob", 2)],
            ..Config::default()
        };
        let (frame_tx, frame_rx) = kanal::unbounded();
        let (tray_tx, _tray_rx) = kanal::unbounded();
        let (_ui_tx, ui_rx) = kanal::unbounded();
        let (_tray_event_tx, tray_events) = kanal::unbounded();
        let (session_tx, _session_rx) = kanal::unbounded::<hermes::Command>();
        let work = Rc::new(RefCell::new(WorkState::new(
            WorkContext {
                config_path: path.clone(),
                config,
                ui_rx,
                frame_tx,
                tray_tx,
                tray_events,
                waker: GuiWaker::new(),
            },
            session_tx,
        )));

        block_on(async {
            compio::fs::remove_file(&path).await.ok();
            let config = work.borrow().config.clone();
            config.save(&path).await.unwrap();
            WorkState::apply_prune_ids(&work, &[1]).await;

            let saved = saved_config(&path).await;
            assert_eq!(
                saved
                    .channels
                    .iter()
                    .map(|c| c.login.as_str())
                    .collect::<Vec<_>>(),
                ["bob"]
            );
            let mut last = None;
            while let Ok(Some(frame)) = frame_rx.try_recv() {
                last = Some(frame);
            }
            let frame = last.expect("frame pushed");
            assert_eq!(frame.config.channels.len(), 1);
            assert!(
                !frame.error.is_empty(),
                "pruned channels must surface an error"
            );
            compio::fs::remove_file(&path).await.ok();
        });
    }
}
