//! Hermes pubsub connection. Network behavior for the [`Worker`]: the
//! batched baseline sync, the websocket lifecycle (welcome, keepalives,
//! reconnect backoff), subscription replay, and live/title notifications.
//! UI mutations never fetch here; adds resolve once through the add job
//! and land through the completion path, so this side only subscribes.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use compio::net::TcpStream;
use compio::ws::tungstenite::Message;
use compio::ws::{WebSocketStream, connect_async};
use serde_json::{Value, json};

use crate::balesh::{NanoId, Topic};
use crate::http::{self, Game};
use crate::matcher::Matcher;
use crate::state::{ConnectOutcome, JobDone, SubStatus, Worker};

const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSED_LIMIT: u64 = 2;
/// How long after a `stream-down` a channel's toasts stay muted. Restreams
/// within this window update state silently instead of notifying again.
const OFFLINE_QUIET: Duration = Duration::from_secs(10 * 60);

pub(crate) type WsStream = WebSocketStream<TcpStream>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubState {
    Pending,
    Subscribed,
    Failed,
}

pub(crate) struct Sub {
    pub(crate) id: NanoId,
    state: SubState,
}

/// Builds the matcher over `words`. Called once at startup and again on
/// every filter change, so the live matcher always reflects the persisted
/// list. Case is folded inside the matcher, so words pass through as-is.
pub(crate) fn build_matcher(words: &[String]) -> Matcher {
    Matcher::new(
        &words
            .iter()
            // An empty pattern matches everything; it can only come from
            // a hand-edited config, so drop it rather than muting all.
            .filter(|word| !word.is_empty())
            .cloned()
            .collect::<Vec<_>>(),
    )
}

impl Worker {
    /// Whether a channel's toasts are muted by a recent `stream-down`.
    /// Expired stamps are cleared on read. Unknown channels are never quiet.
    fn quiet(&mut self, channel_id: u64) -> bool {
        match self.channels.get_mut(&channel_id) {
            Some((_, Some(until))) if Instant::now() < *until => true,
            Some((_, quiet)) => {
                *quiet = None;
                false
            }
            None => false,
        }
    }

    pub(crate) fn start_connect(&mut self) {
        let mut ids: Vec<u64> = self
            .shared
            .read()
            .config
            .channels
            .iter()
            .map(|c| c.id)
            .collect();

        for id in self.channels.keys() {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return;
        }
        self.connecting = true;
        self.jobs.push(Box::pin(async move {
            let fetched = http::fetch_channels(&ids).await;
            let ok = fetched.is_ok();
            let (channels, missing) = match fetched {
                Ok(fetched) => {
                    let seen: HashSet<u64> = fetched.iter().map(|user| user.channel_id).collect();
                    let missing: Vec<u64> = ids
                        .iter()
                        .copied()
                        .filter(|id| !seen.contains(id))
                        .collect();
                    (fetched, missing)
                }
                Err(error) => {
                    log::info!(target: "hermes", "channel refresh failed: {error}");
                    (Vec::new(), Vec::new())
                }
            };
            // Nothing resolved and nothing stale could either: connecting
            // would just time out waiting for a welcome with no topics.
            if ok && channels.is_empty() {
                return JobDone::ConnectFinished(Box::new(ConnectOutcome::Connected {
                    channels: Vec::new(),
                    missing,
                    socket: None,
                }));
            }

            log::info!(target: "hermes", "connecting to {HERMES_URL}");
            let socket = match connect_async(HERMES_URL).await {
                Ok((socket, _)) => {
                    log::info!(target: "hermes", "connected, waiting for welcome");
                    Some(socket)
                }
                Err(error) => {
                    log::info!(target: "hermes", "connect failed: {error}");
                    None
                }
            };

            JobDone::ConnectFinished(Box::new(ConnectOutcome::Connected {
                channels,
                missing,
                socket,
            }))
        }));
    }

    /// Applies a finished connection attempt. Users for channels removed
    /// mid-flight are dropped; ids that resolve to nothing are pruned
    /// from the persisted list. An empty baseline or a failed handshake
    /// backs off; only a live socket with something to watch is kept.
    pub(crate) async fn complete_connect(&mut self, outcome: ConnectOutcome) {
        self.connecting = false;
        let ConnectOutcome::Connected {
            channels,
            missing,
            socket,
        } = outcome;
        let wanted: HashSet<u64> = self
            .shared
            .read()
            .config
            .channels
            .iter()
            .map(|channel| channel.id)
            .collect();
        for channel in channels {
            if !wanted.contains(&channel.channel_id) {
                continue;
            }
            let id = channel.channel_id;
            self.track_channel(channel);
            self.ensure_subs(id);
        }
        let missing: Vec<u64> = missing
            .into_iter()
            .filter(|id| wanted.contains(id))
            .collect();
        if !missing.is_empty() {
            self.apply_prune_ids(&missing);
        }
        let mut socket = socket;
        if self.channels.is_empty() {
            if let Some(mut stray) = socket.take() {
                let _ = stray.close(None).await;
            }
            log::info!(
                target: "hermes",
                "no channels resolved, retrying (typo in a login?)"
            );
            self.schedule_reconnect();
            self.refresh_views(Some(
                "no channels found for the configured logins".to_owned(),
            ));
            return;
        }
        let Some(live) = socket else {
            self.schedule_reconnect();
            self.refresh_views(None);
            return;
        };
        self.socket = Some(live);
        self.welcomed = false;
        self.last_message = Instant::now();
    }

    /// Tracks a fresh resolve, logging stream_id/createdAt at info so the
    /// release file carries it: the baseline logs on first sight when the
    /// request carried stream data, later resolves log only when the
    /// stream identity moved. Steady refreshes stay silent.
    pub(crate) fn track_channel(&mut self, user: http::ResolvedChannel) {
        let id = user.channel_id;
        if should_log_stream(self.channels.get(&id).map(|(user, _)| user), &user) {
            log::info!(
                target: "hermes",
                "stream {}({}): stream_id={} createdAt={}",
                user.channel_name,
                user.channel_id,
                user.stream_id,
                user.stream_created_at.as_deref().unwrap_or("none"),
            );
        }
        // A fresh resolve replaces the baseline but keeps the mute stamp:
        // reconnects must not re-arm notifications inside the window.
        let quiet = self.channels.get(&id).and_then(|(_, quiet)| *quiet);
        self.channels.insert(id, (user, quiet));
    }

    /// Registers a channel's two topics if not registered already. Entries
    /// persist across reconnects; every welcome replays the whole map.
    pub(crate) fn ensure_subs(&mut self, id: u64) {
        for topic in Topic::for_channel(id) {
            self.subs.entry(topic).or_insert_with(|| Sub {
                id: self.rng.nano_id(),
                state: SubState::Pending,
            });
        }
    }

    /// Takes one channel's topics out of the map, returning their
    /// subscription ids for unsubscribing.
    pub(crate) fn take_channel_subs(&mut self, id: u64) -> Vec<(NanoId, Topic)> {
        Topic::for_channel(id)
            .into_iter()
            .filter_map(|topic| self.subs.remove(&topic).map(|sub| (sub.id, topic)))
            .collect()
    }

    /// Drops one id's topics without touching the socket: the channel is
    /// gone (deleted/renamed), so there is nothing to unsubscribe from that
    /// the server would still recognize. Welcome replays never see it again.
    pub(crate) fn drop_channel(&mut self, id: u64) {
        self.channels.remove(&id);
        for topic in Topic::for_channel(id) {
            self.subs.remove(&topic);
        }
    }

    pub(crate) fn status_for(&self, topic: Topic) -> SubStatus {
        match self.subs.get(&topic).map(|sub| sub.state) {
            None | Some(SubState::Pending) => SubStatus::Pending,
            Some(SubState::Subscribed) => SubStatus::Connected,
            Some(SubState::Failed) => SubStatus::Failed,
        }
    }

    /// Drops the socket and backs off. Reconnects always start fresh and
    /// replay all subscriptions; recovering server-side sessions is not
    /// worth the state machine for a handful of topics.
    pub(crate) async fn on_disconnect(&mut self) {
        self.close_socket().await;
        self.welcomed = false;
        self.refresh_views(None);
        self.schedule_reconnect();
    }

    fn schedule_reconnect(&mut self) {
        self.reconnect_attempt += 1;
        let delay = Duration::from_millis(500)
            .saturating_mul(1 << (self.reconnect_attempt.min(10) - 1))
            .min(MAX_RECONNECT_DELAY);
        log::info!(target: "hermes", "disconnected, reconnecting in {delay:?}");
        self.reconnect_at = Instant::now() + delay;
    }

    async fn close_socket(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close(None).await;
        }
    }

    /// Fires once a second: enforces the welcome timeout and drops the
    /// connection after missed keepalives.
    pub(crate) async fn on_tick(&mut self) {
        if self.socket.is_none() {
            return;
        }
        if !self.welcomed {
            // The server never sent a welcome; treat the connection as dead.
            // Any message at all counts as signs of life and pushes this out.
            if self.last_message.elapsed() > WELCOME_TIMEOUT {
                log::info!(target: "hermes", "no welcome within 10s, dropping connection");
                self.on_disconnect().await;
            }
        } else if self.last_message.elapsed()
            > Duration::from_secs(self.keepalive_secs * KEEPALIVE_MISSED_LIMIT)
        {
            log::info!(
                target: "hermes",
                "no keepalive for {}s, dropping connection",
                self.keepalive_secs * KEEPALIVE_MISSED_LIMIT
            );
            self.on_disconnect().await;
        }
    }

    pub(crate) async fn handle_message(&mut self, text: &str) {
        // any frame at all is proof the connection is alive, even a blank
        // one that carries no payload
        self.last_message = Instant::now();
        if text.trim().is_empty() {
            return;
        }
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            log::info!(target: "hermes", "unparseable message: {text}");
            return;
        };
        match message["type"].as_str() {
            Some("welcome") => {
                let summary = if self.reconnect_attempt == 0 {
                    "Hermes connected"
                } else {
                    "Hermes reconnected"
                };
                self.welcomed = true;
                self.reconnect_attempt = 0;
                self.keepalive_secs = message["welcome"]["keepaliveSec"].as_u64().unwrap_or(15);
                log::info!(target: "hermes", "welcome: keepalive={}s", self.keepalive_secs);
                let (sound, count) = {
                    let state = self.shared.read();
                    (state.config.sound, self.channels.len())
                };
                self.queue_toast(
                    summary.to_owned(),
                    format!("watching {count} channel(s)"),
                    sound,
                    None,
                    None,
                );
                self.refresh_views(None);
                self.resubscribe_all().await;
            }
            Some("keepalive") => {}
            Some("subscribeResponse") => {
                let Some(target) = message["subscribeResponse"]["subscription"]["id"]
                    .as_str()
                    .and_then(NanoId::parse)
                else {
                    return;
                };
                let ok = message["subscribeResponse"]["result"].as_str() == Some("ok");
                let topic = match self.subs.iter_mut().find(|(_, sub)| sub.id == target) {
                    Some((topic, sub)) => {
                        sub.state = if ok {
                            SubState::Subscribed
                        } else {
                            SubState::Failed
                        };
                        *topic
                    }
                    None => return,
                };
                log::info!(
                    target: "hermes",
                    "subscription to {topic} {}",
                    if ok { "accepted" } else { "rejected" }
                );
                self.refresh_views(None);
            }
            Some("unsubscribeResponse") => {
                log::info!(target: "hermes", "unsubscribe response: {text}");
            }
            Some("notification") => self.handle_notification(&message),
            _ => log::info!(target: "hermes", "unknown message: {text}"),
        }
    }

    async fn resubscribe_all(&mut self) {
        log::info!(target: "hermes", "replaying {} topic(s)", self.subs.len());
        let pending: Vec<(NanoId, Topic)> = self
            .subs
            .iter_mut()
            .map(|(topic, sub)| {
                sub.state = SubState::Pending;
                (sub.id, *topic)
            })
            .collect();
        for (sub_id, topic) in pending {
            self.send_subscribe(sub_id, topic).await;
        }
    }

    /// Subscribes a single channel's topics; used when a channel is added
    /// to an already-live session (welcomes replay everything instead).
    pub(crate) async fn subscribe_channel(&mut self, id: u64) {
        let pending: Vec<(NanoId, Topic)> = Topic::for_channel(id)
            .into_iter()
            .filter_map(|topic| self.subs.get(&topic).map(|sub| (sub.id, topic)))
            .collect();
        if !pending.is_empty() {
            log::info!(
                target: "hermes",
                "subscribing {} topic(s) for {id}",
                pending.len()
            );
        }
        for (sub_id, topic) in pending {
            self.send_subscribe(sub_id, topic).await;
        }
    }

    async fn send_subscribe(&mut self, subscription_id: NanoId, topic: Topic) {
        log::info!(target: "hermes", "subscribing to {topic}");
        let id = self.rng.nano_id();
        self.send_message(&json!({
            "type": "subscribe",
            "id": id,
            "subscribe": {
                "id": subscription_id,
                "type": "pubsub",
                "pubsub": { "topic": topic.to_string() },
            },
            "timestamp": crate::logging::timestamp(),
        }))
        .await;
    }

    pub(crate) async fn send_message(&mut self, message: &Value) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        if let Err(error) = socket.send(Message::text(message.to_string())).await {
            log::info!(target: "hermes", "send failed: {error}");
            self.on_disconnect().await;
        }
    }

    fn handle_notification(&mut self, message: &Value) {
        let Some(target) = message["notification"]["subscription"]["id"]
            .as_str()
            .and_then(NanoId::parse)
        else {
            return;
        };
        let Some((topic, state)) = self
            .subs
            .iter()
            .find(|(_, sub)| sub.id == target)
            .map(|(topic, sub)| (*topic, sub.state))
        else {
            return;
        };
        if state != SubState::Subscribed {
            return;
        }
        let Ok(pubsub) =
            serde_json::from_str::<Value>(message["notification"]["pubsub"].as_str().unwrap_or(""))
        else {
            return;
        };
        log::info!(
            target: "hermes",
            "notification on {topic}: {}",
            serde_json::to_string(&pubsub).unwrap_or_default()
        );
        match topic {
            Topic::BroadcastSettingsUpdate(channel_id) => {
                self.on_broadcast_settings_update(channel_id, &pubsub);
            }
            Topic::VideoPlaybackById(channel_id) => {
                self.on_video_playback(channel_id, &pubsub);
            }
        }
    }

    fn on_broadcast_settings_update(&mut self, channel_id: u64, pubsub: &Value) {
        let status = pubsub["status"].as_str().unwrap_or_default().to_owned();
        let old_status = pubsub["old_status"].as_str().unwrap_or_default();
        let game = pubsub["game"]
            .as_str()
            .filter(|game| !game.is_empty())
            .map(String::from);
        let old_game = pubsub["old_game"].as_str().unwrap_or_default();
        // Whether the new title trips the word filter; only title changes
        // are gated on it below.
        let title_filtered = {
            let this = &self;
            let title: &str = &status;
            this.matcher.is_match(title)
        };
        let (notify_titles, sound) = {
            let state = self.shared.read();
            (state.config.notify_title_changes, state.config.sound)
        };
        // The borrow ends before the toast queues: `queue_toast` needs
        // `&mut self` while the toast data below borrows the user.
        // The quiet check runs first: it takes `&mut self`, so it cannot
        // overlap the user borrow below.
        let quiet = self.quiet(channel_id);
        let toast = {
            let Some((user, _)) = self.channels.get_mut(&channel_id) else {
                return;
            };
            let title_changed = user.stream_title.as_deref() != Some(status.as_str());
            let game_changed = game.as_deref() != user.game.as_ref().map(|game| game.name.as_str());
            let can_notify = notify_titles && !user.live && !quiet;

            let filtered = title_changed && title_filtered;
            let toast = if can_notify && (title_changed || game_changed) && !filtered {
                let mut lines = Vec::new();
                if title_changed {
                    lines.push(format!("{old_status} → {status}"));
                }
                if game_changed {
                    lines.push(format!("{old_game} → {}", game.as_deref().unwrap_or("")));
                }
                let what = [
                    title_changed.then_some("title"),
                    game_changed.then_some("game"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" & ");
                Some((
                    format!("[{}] {what} changed", user.channel_display_name),
                    lines.join("\n"),
                    user.profile_image_url.clone(),
                    user.channel_name.clone(),
                ))
            } else {
                None
            };
            user.stream_title = Some(status).filter(|title| !title.is_empty());
            let game_display_name = user.game.as_ref().map(|game| game.display_name.clone());
            user.game = game.zip(pubsub["game_id"].as_u64()).map(|(name, id)| Game {
                id,
                name,
                display_name: game_display_name.unwrap_or_default(),
            });
            toast
        };
        if let Some((summary, body, avatar, login)) = toast {
            self.queue_toast(
                summary,
                body,
                sound,
                Some(avatar).filter(|url| !url.is_empty()),
                Some(login),
            );
        }
    }

    /// Handles `video-playback-by-id` pubsub payloads. Known `type` values:
    /// - `stream-up` / `stream-down`: live state flips; acted on below.
    /// - `viewcount`: periodic viewer-count heartbeat while live; ignored.
    ///   Observed shape (2026-09-12):
    ///   `{"type":"viewcount","viewers":583,"server_time":1789245232.108976,`
    ///   `"collaboration_status":"none","collaboration_viewers":0,`
    ///   `"costream_status":"","costream_viewers":0}`
    fn on_video_playback(&mut self, channel_id: u64, pubsub: &Value) {
        match pubsub["type"].as_str() {
            Some("viewcount") => {
                if let Some((user, _)) = self.channels.get_mut(&channel_id) {
                    user.viewers = pubsub["viewers"]
                        .as_u64()
                        .and_then(|x| u32::try_from(x).ok());
                    user.collaboration_viewers = None;
                    if pubsub["collaboration_status"].as_str() == Some("in_collaboration") {
                        user.collaboration_viewers = pubsub["collaboration_viewers"]
                            .as_u64()
                            .and_then(|x| u32::try_from(x).ok());
                    }
                    self.refresh_views(None);
                }
            }
            Some("stream-up") => {
                // Notify from the cached baseline first; refresh from gql after.
                // A restream inside the offline window stays silent: state
                // still flips live and refreshes, only the toast is skipped.
                let quiet = self.quiet(channel_id);
                let cached = self.channels.get_mut(&channel_id).map(|(user, _)| {
                    user.live = true;
                    (
                        user.channel_name.clone(),
                        user.channel_display_name.clone(),
                        user.game.clone(),
                        user.stream_title.clone(),
                        user.profile_image_url.clone(),
                    )
                });
                self.refresh_views(None);
                let (sound, wanted) = {
                    let state = self.shared.read();
                    (
                        state.config.sound,
                        state
                            .config
                            .channels
                            .iter()
                            .any(|channel| channel.id == channel_id),
                    )
                };
                if !wanted && cached.is_none() {
                    return;
                }
                if quiet {
                    log::info!(
                        target: "hermes",
                        "stream-up for {channel_id} inside offline window, staying silent"
                    );
                } else if let Some((login, display_name, game, title, avatar)) = cached {
                    self.notify_live(
                        &display_name,
                        game.as_ref(),
                        title.as_deref(),
                        &avatar,
                        sound,
                        Some(&login),
                    );
                }
                self.jobs.push(Box::pin(async move {
                    JobDone::LiveRefreshed(channel_id, http::fetch_channels(&[channel_id]).await)
                }));
            }
            Some("stream-down") => {
                if let Some((user, quiet)) = self.channels.get_mut(&channel_id) {
                    // Muting lives next to the live flag it guards, so a
                    // quick restream stays silent.
                    *quiet = Some(Instant::now() + OFFLINE_QUIET);
                    user.live = false;
                    user.collaboration_viewers = None;
                    user.viewers = None;
                    self.refresh_views(None);
                }
            }
            _ => {}
        }
    }

    /// Applies a finished `stream-up` refresh: the fresh user replaces the
    /// optimistic baseline. Channels removed mid-flight are dropped;
    /// channels the baseline never knew still notify from the fresh data.
    pub(crate) fn complete_live_refresh(
        &mut self,
        channel_id: u64,
        result: anyhow::Result<Vec<http::ResolvedChannel>>,
    ) {
        let wanted = self.channels.contains_key(&channel_id)
            || self
                .shared
                .read()
                .config
                .channels
                .iter()
                .any(|channel| channel.id == channel_id);
        if !wanted {
            return;
        }
        match result {
            Ok(users) => {
                let Some(mut user) = users.into_iter().find(|user| user.channel_id == channel_id)
                else {
                    return;
                };
                user.live = true;
                if !self.channels.contains_key(&channel_id) && !self.quiet(channel_id) {
                    let sound = self.shared.read().config.sound;
                    let display_name = user.channel_display_name.clone();
                    let game = user.game.clone();
                    let title = user.stream_title.clone();
                    let avatar = user.profile_image_url.clone();
                    let login = user.channel_name.clone();
                    self.notify_live(
                        &display_name,
                        game.as_ref(),
                        title.as_deref(),
                        &avatar,
                        sound,
                        Some(&login),
                    );
                }
                self.track_channel(user);
                self.refresh_views(None);
            }
            Err(error) => {
                log::info!(target: "hermes", "stream-up refresh failed: {error}");
            }
        }
    }

    /// "is LIVE" notification shared by the optimistic (cached baseline)
    /// and fallback (fresh gql user) paths.
    fn notify_live(
        &mut self,
        display_name: &str,
        game: Option<&Game>,
        title: Option<&str>,
        avatar: &str,
        sound: bool,
        login: Option<&str>,
    ) {
        let body = match (game, title) {
            (Some(game), Some(title)) => format!("{title} — {}", game.name),
            (Some(game), None) => game.name.clone(),
            (None, Some(title)) => title.to_owned(),
            (None, None) => String::new(),
        };
        self.queue_toast(
            format!("[{display_name}] is LIVE"),
            body,
            sound,
            Some(avatar.to_owned()).filter(|url| !url.is_empty()),
            login.map(str::to_owned),
        );
    }
}

/// Whether a fresh resolve deserves a stream log: first sight logs only
/// when the request carried stream data, later resolves log only when the
/// stream identity moved.
fn should_log_stream(old: Option<&http::ResolvedChannel>, new: &http::ResolvedChannel) -> bool {
    match old {
        None => new.stream_created_at.is_some(),
        Some(prev) => {
            prev.stream_id != new.stream_id || prev.stream_created_at != new.stream_created_at
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::state::{AppState, SharedFrame, Worker, WorkerParams};

    fn test_worker(filtered_words: &[&str]) -> Worker {
        let config = Config {
            filtered_words: filtered_words.iter().map(ToString::to_string).collect(),
            ..Config::default()
        };
        let (_ui_tx, ui_rx) = kanal::unbounded();
        let (_tray_tx, tray_rx) = kanal::unbounded();
        let (shared, _gui_rx) = SharedFrame::pair(AppState::default());
        Worker::new(WorkerParams {
            config_path: std::env::temp_dir().join("siphon-hermes-test.json"),
            config,
            ui_rx,
            tray_rx,
            shared,
        })
    }

    #[test]
    fn title_filter_matches_case_insensitively() {
        let worker = test_worker(&["offline"]);
        assert!(worker.matcher.is_match("going OFFLINE for the night"));
        assert!(worker.matcher.is_match("Offline"));
        assert!(!worker.matcher.is_match("online and grinding ranked"));
    }

    #[test]
    fn title_filter_empty_word_list_matches_nothing() {
        let worker = test_worker(&[]);
        assert!(!worker.matcher.is_match("offline"));
    }

    fn resolved(stream_id: u64, created_at: Option<&str>) -> crate::http::ResolvedChannel {
        crate::http::ResolvedChannel {
            channel_id: 1,
            channel_name: "alice".to_owned(),
            channel_display_name: "Alice".to_owned(),
            profile_image_url: String::new(),
            stream_id,
            stream_title: None,
            stream_start: None,
            stream_created_at: created_at.map(str::to_owned),
            viewers: None,
            collaboration_viewers: None,
            game: None,
            live: created_at.is_some(),
        }
    }

    #[test]
    fn stream_log_baselines_only_with_data_and_changes_only() {
        // Startup baseline: log when the request carried stream data.
        assert!(super::should_log_stream(
            None,
            &resolved(9, Some("2026-09-10T18:35:06Z"))
        ));
        assert!(!super::should_log_stream(None, &resolved(9, None)));
        // Steady refresh: identical identity stays silent.
        let live = resolved(9, Some("2026-09-10T18:35:06Z"));
        assert!(!super::should_log_stream(
            Some(&live),
            &resolved(9, Some("2026-09-10T18:35:06Z"))
        ));
        // New stream id or timestamp logs, including going offline.
        assert!(super::should_log_stream(
            Some(&live),
            &resolved(10, Some("2026-09-10T18:35:06Z"))
        ));
        assert!(super::should_log_stream(
            Some(&live),
            &resolved(9, Some("2026-09-11T18:35:06Z"))
        ));
        assert!(super::should_log_stream(Some(&live), &resolved(9, None)));
    }
}
