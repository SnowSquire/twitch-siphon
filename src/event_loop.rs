//! Work-thread run loop. The [`Worker`] multiplexes UI intents, tray
//! actions, toast requests, the hermes socket, a one-second tick, and
//! background job completions in one `select!`; jobs carry owned results
//! back instead of sharing borrows, so handlers run with plain `&mut self`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{FutureExt as _, StreamExt as _};

use crate::config::Channel;
use crate::http;
use crate::state::{AvailableUpdate, JobDone, JobFuture, UiIntent, UpdateStatus, Worker};
use crate::tray::TrayAction;
use crate::update;

impl Worker {
    pub async fn run(&mut self) {
        let ui_rx = self.ui_rx.clone();
        let tray_rx = self.tray_rx.clone();
        let mut tick = compio::time::interval(Duration::from_secs(1));
        loop {
            if self.socket.is_none()
                && !self.connecting
                && {
                    let this = &self;
                    !this.channels.is_empty() || !this.shared.read().config.channels.is_empty()
                }
                && Instant::now() >= self.reconnect_at
            {
                self.start_connect();
            }
            // The poll futures borrow `self` (socket, jobs); they are
            // scoped to this block and dropped before dispatch, so the
            // handlers below run with plain `&mut self`. Every payload
            // crossing the boundary is owned.
            enum Event {
                Intent(UiIntent),
                Tray(TrayAction),
                SocketText(compio::ws::tungstenite::Utf8Bytes),
                SocketClosed,
                Tick,
                Job(Box<JobDone>),
            }
            let event = {
                let ui_fut = ui_rx.recv().fuse();
                let tray_fut = tray_rx.recv().fuse();
                let socket_fut = async {
                    match self.socket.as_mut() {
                        Some(socket) => socket.next().await,
                        None => std::future::pending().await,
                    }
                }
                .fuse();

                let tick_fut = tick.tick().fuse();
                let job_fut = Self::next_job(&mut self.jobs).fuse();

                futures_util::pin_mut!(ui_fut, tray_fut, socket_fut, tick_fut, job_fut);
                futures_util::select! {
                    intent = ui_fut => match intent {
                        Ok(intent) => Event::Intent(intent),
                        Err(_) => break,
                    },
                    action = tray_fut => match action {
                        Ok(action) => Event::Tray(action),
                        Err(_) => break,
                    },
                    message = socket_fut => match message {
                        Some(Ok(compio::ws::tungstenite::Message::Close(_))) => {
                            log::info!(target: "hermes", "server sent close frame");
                            Event::SocketClosed
                        }
                        Some(Ok(message)) => match message.into_text() {
                            Ok(text) => Event::SocketText(text),
                            Err(_) => continue,
                        },
                        Some(Err(error)) => {
                            log::info!(target: "hermes", "read error: {error}");
                            Event::SocketClosed
                        }
                        None => {
                            log::info!(target: "hermes", "connection closed");
                            Event::SocketClosed
                        }
                    },
                    _ = tick_fut => Event::Tick,
                    done = job_fut => match done {
                        Some(done) => Event::Job(Box::new(done)),
                        None => continue,
                    },
                }
            };
            match event {
                Event::Intent(intent) => self.handle_intent(intent).await,
                Event::Tray(action) => self.forward_tray(action),
                Event::SocketText(text) => self.handle_message(&text).await,
                Event::SocketClosed => self.on_disconnect().await,
                Event::Tick => self.on_tick().await,
                Event::Job(done) => self.handle_job(*done).await,
            }
        }
    }

    async fn next_job(
        jobs: &mut futures_util::stream::FuturesUnordered<JobFuture>,
    ) -> Option<JobDone> {
        if jobs.is_empty() {
            std::future::pending::<()>().await;
            None
        } else {
            jobs.next().await
        }
    }

    pub(crate) async fn handle_job(&mut self, done: JobDone) {
        match done {
            JobDone::AddResolved(login, result) => {
                self.complete_add_login(login, result).await;
            }
            JobDone::LiveRefreshed(id, result) => {
                self.complete_live_refresh(id, result);
            }
            JobDone::SaveFinished(result) => {
                if let Err(error) = result {
                    self.set_error(error.to_string());
                }
            }
            JobDone::ToastShown => {}
            JobDone::UpdateCheckFinished(result) => {
                // A click may have moved on to `Downloading` while the
                // check was in flight; only `Checking` still wants this.
                if !matches!(self.shared.read().update, UpdateStatus::Checking) {
                    return;
                }
                match result {
                    Ok(release) => {
                        self.shared.update(|state| {
                            state.update = if update::newer_than_current(&release) {
                                log::info!(target: "update", "new release: {}", release.version);
                                UpdateStatus::Available(AvailableUpdate {
                                    version: release.version.to_string(),
                                    msi_url: release.msi_url,
                                    page_url: release.page_url,
                                })
                            } else {
                                UpdateStatus::Current
                            };
                        });
                    }
                    Err(error) => {
                        log::info!(target: "update", "check failed: {error}");
                        self.shared.update(|state| {
                            state.update = UpdateStatus::Idle;
                        });
                    }
                }
                self.push_update_wait();
            }
            JobDone::UpdateCheckDue => self.push_update_check(),
            JobDone::UpdateDownloadFinished(result) => {
                self.finish_update_download(result);
            }
            JobDone::ConnectFinished(outcome) => {
                self.complete_connect(*outcome).await;
            }
        }
    }

    async fn handle_intent(&mut self, intent: UiIntent) {
        match intent {
            UiIntent::AddLogin(login) => {
                self.request_add_login(login);
            }
            UiIntent::RemoveChannel(id) => {
                self.apply_remove_channel(id).await;
            }
            UiIntent::SetNotifyTitleChanges(value) => {
                self.apply_notify_title_changes(value);
            }
            UiIntent::SetSound(value) => {
                self.apply_sound(value);
            }
            UiIntent::AddFilteredWord(word) => {
                self.apply_add_filtered_word(word);
            }
            UiIntent::RemoveFilteredWord(index) => {
                self.apply_remove_filtered_word(index);
            }
            UiIntent::ApplyUpdate => {
                self.handle_apply_update();
            }
            UiIntent::ClearError => self.shared.update(|state| {
                state.error.clear();
            }),
        }
    }

    /// Validates an add request and queues the single resolve. Returns the
    /// normalized login when a resolve is worthwhile; duplicates, in-flight
    /// resolves, and empty input are silently ignored. Persistence happens
    /// in [`Self::complete_add_login`]; nothing here touches disk.
    pub(crate) fn request_add_login(&mut self, login: String) -> Option<String> {
        let login = login.trim().to_lowercase();
        let (known, pending) = {
            let state = self.shared.read();
            (
                state
                    .config
                    .channels
                    .iter()
                    .any(|channel| channel.login.eq_ignore_ascii_case(&login)),
                state
                    .pending
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(&login)),
            )
        };
        if login.is_empty() || known || pending {
            return None;
        }
        self.shared.update(|state| {
            state.pending.push(login.clone());
            state.error.clear();
        });
        let resolve = login.clone();
        self.jobs.push(Box::pin(async move {
            let user = http::fetch_channel(&resolve).await;
            JobDone::AddResolved(resolve, user)
        }));
        Some(login)
    }

    /// Applies a finished resolve: resolved channels persist via a save
    /// job, join the tracked users, publish a fresh row, and subscribe
    /// immediately; unknown logins and transport failures only surface
    /// an error and never touch disk.
    pub(crate) async fn complete_add_login(
        &mut self,
        login: String,
        result: anyhow::Result<Option<http::ResolvedChannel>>,
    ) {
        let stale = {
            let state = self.shared.read();
            !state
                .pending
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&login))
        };
        if stale {
            return;
        }
        match result {
            Ok(Some(user)) => {
                log::info!(
                    target: "config",
                    "resolved {} to id {} ({})",
                    login,
                    user.channel_id,
                    user.channel_display_name
                );
                let duplicate = {
                    let state = self.shared.read();
                    state
                        .config
                        .channels
                        .iter()
                        .any(|existing| existing.id == user.channel_id)
                };
                let id = user.channel_id;
                let channel = Channel {
                    login: user.channel_name.clone(),
                    id: user.channel_id,
                    display_name: Some(user.channel_display_name.clone()),
                };
                self.shared.update(|state| {
                    state
                        .pending
                        .retain(|item| !item.eq_ignore_ascii_case(&login));
                    if !duplicate {
                        state.config.channels.push(channel);
                    }
                });
                if duplicate {
                    return;
                }
                // Track the resolved user so status rows and notifications
                // see it, then publish: without this the row keeps the
                // Pending fallback until a reconnect re-baselines.
                self.track_channel(user);
                self.queue_save();
                self.ensure_subs(id);
                self.refresh_views(None);
                if self.welcomed {
                    self.subscribe_channel(id).await;
                }
            }
            Ok(None) => {
                log::info!(
                    target: "config",
                    "gql returned no channel named {login}, is it a typo?"
                );
                self.shared.update(|state| {
                    state
                        .pending
                        .retain(|item| !item.eq_ignore_ascii_case(&login));
                });
                self.set_error(format!("channel {login} not found"));
            }
            Err(error) => {
                log::info!(target: "config", "channel resolution failed: {error}");
                self.shared.update(|state| {
                    state
                        .pending
                        .retain(|item| !item.eq_ignore_ascii_case(&login));
                });
                self.set_error(format!("failed to resolve {login}: {error}"));
            }
        }
    }

    pub(crate) async fn apply_remove_channel(&mut self, id: u64) {
        self.shared.update(|state| {
            state.config.channels.retain(|channel| channel.id != id);
        });
        self.drop_channel(id);
        for (sub_id, topic) in self.take_channel_subs(id) {
            log::info!(target: "hermes", "unsubscribing from {topic}");
            let message_id = self.rng.nano_id();
            self.send_message(&serde_json::json!({
                "type": "unsubscribe",
                "id": message_id,
                "unsubscribe": { "id": sub_id },
                "timestamp": crate::logging::timestamp(),
            }))
            .await;
        }
        self.refresh_views(None);
        self.queue_save();
    }

    /// Drops ids that no longer resolve (deleted/renamed): they disappear
    /// from the persisted list with an error, instead of lingering. No
    /// disk touch when nothing changed.
    pub(crate) fn apply_prune_ids(&mut self, ids: &[u64]) {
        let pruned: Vec<String> = {
            let state = self.shared.read();
            state
                .config
                .channels
                .iter()
                .filter(|channel| ids.contains(&channel.id))
                .map(|channel| {
                    channel
                        .display_name
                        .clone()
                        .unwrap_or_else(|| channel.login.clone())
                })
                .collect()
        };
        if pruned.is_empty() {
            return;
        }
        for id in ids {
            self.drop_channel(*id);
        }
        let conn_error = self.shared.read().conn_error.clone();
        self.shared.update(|state| {
            state
                .config
                .channels
                .retain(|channel| !ids.contains(&channel.id));
        });
        self.refresh_views(conn_error);
        self.queue_save();
        let names = pruned.join(", ");
        if pruned.len() == 1 {
            self.set_error(format!("{names} no longer resolves and was removed"));
        } else {
            self.set_error(format!("{names} no longer resolve and were removed"));
        }
    }

    pub(crate) fn apply_notify_title_changes(&mut self, value: bool) {
        self.shared.update(|state| {
            state.config.notify_title_changes = value;
        });
        self.queue_save();
    }

    pub(crate) fn apply_sound(&mut self, value: bool) {
        self.shared.update(|state| {
            state.config.sound = value;
        });
        self.queue_save();
    }

    /// Trims and dedupes (case-insensitive, matching the matcher's folding).
    /// Empty input and duplicates change nothing: no publish, no save.
    pub(crate) fn apply_add_filtered_word(&mut self, word: String) {
        let word = word.trim().to_owned();
        if word.is_empty() {
            return;
        }
        let folded = word.to_lowercase();
        let duplicate = {
            let state = self.shared.read();
            state
                .config
                .filtered_words
                .iter()
                .any(|existing| existing.to_lowercase() == folded)
        };
        if duplicate {
            return;
        }
        self.shared.update(|state| {
            state.config.filtered_words.push(word);
            state.config.version = crate::config::Config::VERSION;
        });
        self.matcher =
            crate::hermes::build_matcher(&self.shared.read().config.filtered_words.clone());
        self.queue_save();
    }

    pub(crate) fn apply_remove_filtered_word(&mut self, index: usize) {
        let out_of_range = {
            let state = self.shared.read();
            index >= state.config.filtered_words.len()
        };
        if out_of_range {
            return;
        }
        self.shared.update(|state| {
            state.config.filtered_words.remove(index);
            state.config.version = crate::config::Config::VERSION;
        });
        self.matcher =
            crate::hermes::build_matcher(&self.shared.read().config.filtered_words.clone());
        self.queue_save();
    }

    pub(crate) fn push_update_check(&mut self) {
        if matches!(
            self.shared.read().update,
            UpdateStatus::Checking | UpdateStatus::Downloading(_)
        ) {
            return;
        }
        self.shared.update(|state| {
            state.update = UpdateStatus::Checking;
        });
        self.jobs.push(Box::pin(async move {
            JobDone::UpdateCheckFinished(update::fetch_latest().await)
        }));
    }

    fn push_update_wait(&mut self) {
        self.jobs.push(Box::pin(async move {
            compio::time::sleep(update::CHECK_INTERVAL).await;
            JobDone::UpdateCheckDue
        }));
    }

    fn handle_apply_update(&mut self) {
        let offer = {
            let state = self.shared.read();
            match &state.update {
                // Yields the offer only from `Available`, so double clicks
                // and stale intents are ignored.
                UpdateStatus::Available(offer) => Some(offer.clone()),
                _ => None,
            }
        };
        let Some(offer) = offer else { return };
        self.shared.update(|state| {
            state.update = UpdateStatus::Downloading(offer.clone());
        });
        // MSI installs download the installer; everything else opens the
        // release page right away, keeping the offer.
        let msi = matches!(update::install_mode(), update::InstallMode::Msi)
            .then(|| offer.msi_url.clone())
            .flatten();
        if let Some(url) = msi {
            let version = offer.version.clone();
            self.jobs.push(Box::pin(async move {
                JobDone::UpdateDownloadFinished(update::download_msi(&url, &version).await)
            }));
        } else {
            self.finish_update_page(&offer);
        }
    }

    /// Opens the release page for builds with no installer handoff
    /// (portable builds, or a release without an installer asset). The
    /// offer stays: the button remains until a newer check replaces it.
    fn finish_update_page(&mut self, offer: &AvailableUpdate) {
        self.shared.update(|state| {
            state.update = UpdateStatus::Available(offer.clone());
        });
        if let Err(error) = open::that(&offer.page_url) {
            self.set_error(format!("couldn't open {}: {error}", offer.page_url));
        }
    }

    /// Applies a finished installer download: launches the updater and
    /// quits so no files are locked; the updater reopens the app once the
    /// install finishes. Failures revert to the offer with an error, so
    /// the button retries.
    fn finish_update_download(&mut self, result: anyhow::Result<PathBuf>) {
        let offer = {
            let state = self.shared.read();
            match &state.update {
                UpdateStatus::Downloading(offer) => Some(offer.clone()),
                _ => None,
            }
        };
        let Some(offer) = offer else { return };
        match result {
            Ok(path) => match launch_installer(&path) {
                Ok(()) => {
                    log::info!(target: "update", "updater launched, quitting for upgrade");
                    self.forward_tray(TrayAction::Quit);
                }
                Err(error) => {
                    self.shared.update(|state| {
                        state.update = UpdateStatus::Available(offer.clone());
                    });
                    self.set_error(format!("couldn't launch installer: {error}"));
                }
            },
            Err(error) => {
                self.shared.update(|state| {
                    state.update = UpdateStatus::Available(offer.clone());
                });
                self.set_error(format!("update download failed: {error}"));
            }
        }
    }
}

/// Runs the downloaded package's installer, then reopens the app. Only
/// MSI installs ever download, so this always runs `msiexec` through the
/// detached waiter in `update`.
fn launch_installer(path: &std::path::Path) -> anyhow::Result<()> {
    update::install_msi_and_relaunch(path)
}
