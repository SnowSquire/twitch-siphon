//! Work-thread run loop. The [`Worker`] multiplexes UI intents, tray
//! actions, toast requests, the hermes socket, a one-second tick, and
//! background job completions in one `select!`; jobs carry owned results
//! back instead of sharing borrows, so handlers run with plain `&mut self`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{FutureExt as _, StreamExt as _};

use crate::config::Channel;
use crate::http;
use crate::state::{JobDone, JobFuture, SubState, UiIntent, Worker};
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
                    let snapshot = self.shared.read();
                    !snapshot.channels.is_empty() || !snapshot.config.channels.is_empty()
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
                self.update_busy = false;
                match result {
                    Ok(release) => {
                        if update::newer_than_current(&release) {
                            log::info!(target: "update", "new release: {}", release.version);
                            self.shared.update(|state| {
                                state.update = Some(release);
                            });
                        } else {
                            self.shared.update(|state| {
                                state.update = None;
                            });
                        }
                    }
                    Err(error) => {
                        log::info!(target: "update", "check failed: {error}");
                    }
                }
                self.push_update_wait();
            }
            JobDone::UpdateCheckDue => self.push_update_check(),
            JobDone::UpdateDownloadFinished(result) => {
                self.finish_update_download(result);
            }
            JobDone::SubscribeCheckDue(topic, sub_id, attempt) => {
                let unconfirmed = matches!(
                    self.sub_ids.get(&topic),
                    Some(entry)
                        if entry.sub_id == sub_id
                            && entry.attempt == attempt
                            && entry.state == SubState::Pending
                );
                if unconfirmed {
                    // A live socket with a dead topic never heals on its
                    // own: run it through the socket reconnect so backoff
                    // and the welcome replay retry the topic. Without a
                    // socket a reconnect is already underway, which replays
                    // everything.
                    log::info!(target: "hermes", "subscription to {topic} not confirmed, reconnecting");
                    if self.socket.is_some() {
                        self.on_disconnect().await;
                    }
                }
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
        result: anyhow::Result<Option<http::ChannelDetail>>,
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
        self.shared.update(|state| {
            state
                .pending
                .retain(|item| !item.eq_ignore_ascii_case(&login));
        });
        match result {
            Ok(Some(user)) => {
                log::info!(
                    target: "config",
                    "resolved {} to id {} ({})",
                    login,
                    user.id,
                    user.display_name
                );
                let duplicate = {
                    let state = self.shared.read();
                    state
                        .config
                        .channels
                        .iter()
                        .any(|existing| existing.id == user.id)
                };
                let id = user.id;
                let channel = Channel {
                    login: user.login.clone(),
                    id: user.id,
                    display_name: Some(user.display_name.clone()),
                };
                if !duplicate {
                    self.shared.update(|state| {
                        state.config.channels.push(channel);
                    });
                }
                if duplicate {
                    return;
                }
                // Track the resolved user so rows and notifications see
                // it: without this the row is missing until a reconnect
                // re-baselines.
                self.track_channel(user);
                self.queue_save();
                self.ensure_subs(id);
                if self.welcomed {
                    self.subscribe_channel(id).await;
                }
            }
            Ok(None) => {
                log::info!(
                    target: "config",
                    "gql returned no channel named {login}, is it a typo?"
                );
                self.set_error(format!("channel {login} not found"));
            }
            Err(error) => {
                log::info!(target: "config", "channel resolution failed: {error}");
                self.set_error(format!("failed to resolve {login}: {error}"));
            }
        }
    }

    pub(crate) async fn apply_remove_channel(&mut self, id: u64) {
        self.shared.update(|state| {
            state.config.channels.retain(|channel| channel.id != id);
        });
        for (sub_id, topic) in self.drop_channel(id) {
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
        self.shared.update(|snapshot| {
            snapshot
                .config
                .channels
                .retain(|channel| !ids.contains(&channel.id));
        });
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
        if self.update_busy {
            // A check or download is in flight; keep the periodic cycle
            // alive so checks do not stall behind it.
            self.push_update_wait();
            return;
        }
        self.update_busy = true;
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
        let offer = self.shared.read().update.clone();
        let Some(offer) = offer else { return };
        if self.update_busy {
            return;
        }
        self.update_busy = true;
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
    fn finish_update_page(&mut self, offer: &crate::update::Release) {
        self.update_busy = false;
        if let Err(error) = open::that(&offer.page_url) {
            self.set_error(format!("couldn't open {}: {error}", offer.page_url));
        }
    }

    /// Applies a finished installer download: launches the updater and
    /// quits so no files are locked; the updater reopens the app once the
    /// install finishes. Failures keep the offer with an error, so the
    /// button retries.
    fn finish_update_download(&mut self, result: anyhow::Result<PathBuf>) {
        match result {
            Ok(path) => match update::install_msi_and_relaunch(&path) {
                Ok(()) => {
                    log::info!(target: "update", "updater launched, quitting for upgrade");
                    self.forward_tray(TrayAction::Quit);
                }
                Err(error) => {
                    self.update_busy = false;
                    self.set_error(format!("couldn't launch installer: {error}"));
                }
            },
            Err(error) => {
                self.update_busy = false;
                self.set_error(format!("update download failed: {error}"));
            }
        }
    }
}
