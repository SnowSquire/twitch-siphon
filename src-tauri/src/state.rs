//! GUI↔work wire protocol and work-thread state.
//!
//! The GUI thread is purely presentational: it renders the latest
//! [`FrameState`] and sends [`UiIntent`]s. The work thread owns everything
//! else (config, persistence, the hermes session) inside [`WorkState`], which
//! lives on that single thread — no `Arc`, no `Mutex` for app state. The only
//! cross-thread primitives are the [`crossbeam_channel`] queues below plus
//! [`GuiWaker`], which is a doorbell, not state.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crossbeam_channel::{Receiver, Sender};

use crate::config::{Channel, Config};
use crate::gql;
use crate::hermes::{self, ChannelEntry};
use crate::tray::TrayAction;

/// One imperative UI mutation. Sent GUI→work instead of diffing whole
/// settings snapshots; the work thread persists, forwards to hermes, and
/// pushes a fresh [`FrameState`] back.
pub enum UiIntent {
    AddLogin(String),
    RemoveLogin(String),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
}

/// Everything the GUI needs for one frame. Pushed work→GUI on every change;
/// the GUI keeps the latest and renders it. All fields are `Clone` so the
/// snapshot crosses the channel by value.
#[derive(Clone, Default)]
pub struct FrameState {
    pub config: Config,
    pub status: Option<hermes::StatusEvent>,
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
    // Cheap handle construction (`Arc` + `OnceLock`); cannot be `const`.
    #[allow(clippy::missing_const_for_fn)]
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
    pub waker: GuiWaker,
}

/// All mutable app state, owned by the work thread alone. Every method is
/// plain single-threaded code; tasks on the runtime share it through
/// `Rc<RefCell<WorkState>>` with short scoped borrows, never held across
/// an `await`.
pub struct WorkState {
    config_path: PathBuf,
    config: Config,
    error: String,
    status: Option<hermes::StatusEvent>,
    session_tx: futures_channel::mpsc::UnboundedSender<hermes::Command>,
    ui_rx: Receiver<UiIntent>,
    frame_tx: Sender<FrameState>,
    tray_tx: Sender<TrayAction>,
    waker: GuiWaker,
}

impl WorkState {
    pub fn new(
        ctx: WorkContext,
        session_tx: futures_channel::mpsc::UnboundedSender<hermes::Command>,
    ) -> Self {
        Self {
            config_path: ctx.config_path,
            config: ctx.config,
            error: String::new(),
            status: None,
            session_tx,
            ui_rx: ctx.ui_rx,
            frame_tx: ctx.frame_tx,
            tray_tx: ctx.tray_tx,
            waker: ctx.waker,
        }
    }

    /// Non-blocking drain of pending UI intents, oldest first.
    pub fn drain_intents(&self) -> Vec<UiIntent> {
        self.ui_rx.try_iter().collect()
    }

    /// Sends the startup commands hermes needs: one add per configured
    /// channel plus the saved preferences, then the first frame.
    pub fn seed(&self) {
        for channel in &self.config.channels {
            self.forward(hermes::Command::AddChannel(ChannelEntry {
                login: channel.login.clone(),
                id: Some(channel.id),
                display_name: channel.display_name.clone(),
            }));
        }
        self.forward(hermes::Command::SetNotifyTitleChanges(
            self.config.notify_title_changes,
        ));
        self.forward(hermes::Command::SetSound(self.config.sound));
        self.push_frame();
    }

    pub fn set_status(&mut self, status: hermes::StatusEvent) {
        self.status = Some(status);
        self.push_frame();
    }

    pub fn forward_tray(&self, action: TrayAction) {
        let _ = self.tray_tx.send(action);
        self.wake();
    }

    /// Validates an add request. Returns the normalized login when a resolve
    /// is worthwhile; duplicates and empty input are silently ignored.
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
        self.error.clear();
        self.push_frame();
        Some(login)
    }

    /// Applies a finished resolve: resolved channels persist to disk, unknown
    /// logins are forwarded session-only so the ui shows them as not found
    /// without persisting the typo.
    pub fn finish_add_login(
        &mut self,
        login: String,
        result: Result<Vec<gql::User>, gql::Error>,
    ) {
        let resolved = match result {
            Ok(users) => users
                .into_iter()
                .find(|user| user.channel_name.eq_ignore_ascii_case(&login))
                .map(|user| {
                    log::info!(
                        target: "config",
                        "resolved {} to id {} ({})",
                        login,
                        user.channel_id,
                        user.channel_display_name
                    );
                    Channel {
                        login: user.channel_name,
                        id: user.channel_id,
                        display_name: Some(user.channel_display_name),
                    }
                }),
            Err(error) => {
                log::info!(target: "config", "channel resolution failed: {error}");
                None
            }
        };

        if let Some(channel) = resolved {
            if self
                .config
                .channels
                .iter()
                .any(|existing| existing.id == channel.id)
            {
                return;
            }
            let entry = ChannelEntry {
                login: channel.login.clone(),
                id: Some(channel.id),
                display_name: channel.display_name.clone(),
            };
            self.config.channels.push(channel);
            if let Err(error) = self.save() {
                self.set_error(error);
                return;
            }
            self.forward(hermes::Command::AddChannel(entry));
        } else {
            log::info!(
                target: "config",
                "gql returned no channel named {login}, is it a typo?"
            );
            self.forward(hermes::Command::AddChannel(ChannelEntry {
                login,
                id: None,
                display_name: None,
            }));
        }
        self.push_frame();
    }

    pub fn apply_remove_login(&mut self, login: &str) {
        self.config
            .channels
            .retain(|channel| !channel.login.eq_ignore_ascii_case(login));
        if let Err(error) = self.save() {
            self.set_error(error);
            return;
        }
        self.forward(hermes::Command::RemoveChannel(login.to_owned()));
        self.push_frame();
    }

    pub fn apply_notify_title_changes(&mut self, value: bool) {
        self.config.notify_title_changes = value;
        if let Err(error) = self.save() {
            self.set_error(error);
            return;
        }
        self.forward(hermes::Command::SetNotifyTitleChanges(value));
        self.push_frame();
    }

    pub fn apply_sound(&mut self, value: bool) {
        self.config.sound = value;
        if let Err(error) = self.save() {
            self.set_error(error);
            return;
        }
        self.forward(hermes::Command::SetSound(value));
        self.push_frame();
    }

    fn forward(&self, command: hermes::Command) {
        if let Err(error) = self.session_tx.unbounded_send(command) {
            // The session owns the receiver for the runtime lifetime, so a
            // send failure is unexpected; surface it rather than dropping it.
            log::info!(target: "config", "hermes command send failed: {error}");
        }
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.error = message.into();
        self.push_frame();
    }

    fn save(&mut self) -> Result<(), String> {
        self.config.version = Config::VERSION;
        self.config
            .save(&self.config_path)
            .map_err(|error| error.to_string())
    }

    fn push_frame(&self) {
        let frame = FrameState {
            config: self.config.clone(),
            status: self.status.clone(),
            error: self.error.clone(),
        };
        let _ = self.frame_tx.send(frame);
        self.wake();
    }

    fn wake(&self) {
        self.waker.repaint();
    }
}
