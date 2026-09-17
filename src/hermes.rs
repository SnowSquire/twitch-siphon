use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use compio::net::TcpStream;
use compio::ws::tungstenite::Message;
use compio::ws::{WebSocketStream, connect_async};
use futures_util::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};

use crate::balesh::{NanoId, Rng, Topic};

use crate::gql::{self, Game, User};
use crate::notifier;
use crate::state::WorkState;

const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSED_LIMIT: u64 = 2;

pub enum Command {
    AddChannel(String),
    RemoveChannel(u64),
    SetNotifyTitleChanges(bool),
    SetSound(bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubState {
    Pending,
    Subscribed,
    Failed,
}

#[derive(Clone, Copy)]
pub enum SubStatus {
    Pending,
    Connected,
    Failed,
}

#[derive(Clone)]
pub struct ChannelStatus {
    pub channel_id: u64,
    pub login: String,
    pub display_name: String,
    pub title_status: SubStatus,
    pub live_status: SubStatus,
}

#[derive(Clone)]
pub struct StatusSnapshot {
    pub connected: bool,
    pub error: Option<String>,
    pub channels: Vec<ChannelStatus>,
    pub pending_adds: Arc<RwLock<Vec<String>>>,
}

struct Sub {
    id: NanoId,
    state: SubState,
}

type WsStream = WebSocketStream<TcpStream>;

/// Notification preferences the ui can change at runtime.
struct Preferences {
    notify_title_changes: bool,
    sound: bool,
}

pub struct Session {
    work: Rc<RefCell<WorkState>>,
    command_rx: kanal::AsyncReceiver<Command>,
    channels: HashMap<u64, User>,
    subs: HashMap<Topic, Sub>,
    preferences: Preferences,
    socket: Option<WsStream>,
    welcomed: bool,
    keepalive_secs: u64,
    last_message: Instant,
    reconnect_at: Instant,
    reconnect_attempt: u32,
    rng: Rng,
}

impl Session {
    pub fn new(work: Rc<RefCell<WorkState>>, command_rx: kanal::AsyncReceiver<Command>) -> Self {
        let (notify_title_changes, sound) = {
            let work = work.borrow();
            (
                work.config_snapshot().notify_title_changes,
                work.config_snapshot().sound,
            )
        };
        Self {
            work,
            command_rx,
            channels: HashMap::new(),
            subs: HashMap::new(),
            preferences: Preferences {
                notify_title_changes,
                sound,
            },

            socket: None,
            welcomed: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            rng: Rng::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0x853c_49e6_748f_ea9b, |elapsed| elapsed.as_nanos() as u64),
            ),
        }
    }

    pub async fn run(&mut self) {
        log::info!(target: "hermes", "worker started");
        let mut tick = compio::time::interval(Duration::from_secs(1));
        // Owned by the loop, not `self`: the recv future must not borrow
        // `self` while command/socket handling takes `&mut self`.
        let command_rx = self.command_rx.clone();
        loop {
            if self.socket.is_none() && self.has_work() && Instant::now() >= self.reconnect_at {
                self.connect().await;
            }
            let command_fut = command_rx.recv().fuse();
            let socket_fut = async {
                match self.socket.as_mut() {
                    Some(socket) => socket.next().await,
                    None => std::future::pending().await,
                }
            }
            .fuse();
            let tick_fut = tick.tick().fuse();
            futures_util::pin_mut!(command_fut, socket_fut, tick_fut);
            futures_util::select! {
                command = command_fut => {
                    let Ok(command) = command else { return };
                    self.on_command(command).await;
                }
                message = socket_fut => {
                    // `Message` owns refcounted bytes; drop before awaits below.
                    enum SocketEvent {
                        Text(compio::ws::tungstenite::Utf8Bytes),
                        Closed,
                        Ignored,
                    }
                    let event = match message {
                        Some(Ok(Message::Close(_))) => {
                            log::info!(target: "hermes", "server sent close frame");
                            SocketEvent::Closed
                        }
                        Some(Ok(message)) => message
                            .into_text()
                            .map_or_else(|_| SocketEvent::Ignored, SocketEvent::Text),
                        Some(Err(error)) => {
                            log::info!(target: "hermes", "read error: {error}");
                            SocketEvent::Closed
                        }
                        None => {
                            log::info!(target: "hermes", "connection closed");
                            SocketEvent::Closed
                        }
                    };
                    match event {
                        SocketEvent::Text(text) => {
                            self.handle_message(&text).await;
                        }
                        SocketEvent::Closed => {
                            self.on_disconnect().await;
                        }
                        SocketEvent::Ignored => {}
                    }
                }
                _ = tick_fut => {
                    self.on_tick().await;
                }
            }
        }
    }

    /// Anything to subscribe to. The session map stays empty until the first
    /// `connect` populates it, so the persisted list is checked as well.
    fn has_work(&self) -> bool {
        !self.channels.is_empty() || !self.work.borrow().config_snapshot().channels.is_empty()
    }

    /// Applies one ui mutation. Additions are subscribed on the live
    /// connection when there is one; removals are unsubscribed the same way.
    /// The socket itself is never restarted for config changes.
    async fn on_command(&mut self, command: Command) {
        match command {
            Command::AddChannel(login) => self.add_channel(login).await,
            Command::RemoveChannel(id) => self.remove_channel(id).await,
            Command::SetNotifyTitleChanges(value) => {
                log::info!(target: "hermes", "notify_title_changes={value}");
                self.preferences.notify_title_changes = value;
            }
            Command::SetSound(value) => {
                log::info!(target: "hermes", "sound={value}");
                self.preferences.sound = value;
            }
        }
    }

    async fn add_channel(&mut self, login: String) {
        if self
            .channels
            .values()
            .any(|user| user.channel_name.eq_ignore_ascii_case(&login))
        {
            return;
        }
        log::info!(target: "hermes", "channel {login} added");
        // One targeted fetch seeds the notify baseline (title/game/live).
        match gql::fetch_users(&[], std::slice::from_ref(&login)).await {
            Ok(mut users) => {
                let user = users
                    .pop()
                    .filter(|user| user.channel_name.eq_ignore_ascii_case(&login));
                if let Some(user) = user {
                    let id = user.channel_id;
                    self.upsert(user);
                    self.after_resolve(id).await;
                    self.emit_status(None);
                } else {
                    log::info!(
                        target: "hermes",
                        "gql returned no channel named {login}, is it a typo?"
                    );
                    self.emit_status(Some(&format!("channel {login} not found")));
                    WorkState::apply_prune_login(&self.work, &login).await;
                }
            }
            Err(error) => {
                log::info!(target: "hermes", "failed to fetch channel: {error}");
                self.emit_status(Some(&format!("failed to fetch {login}: {error}")));
            }
        }
    }

    /// Registers the new channel's topics and subscribes them immediately
    /// when the session is already live; otherwise the next welcome replays
    /// everything, including these.
    async fn after_resolve(&mut self, id: u64) {
        self.ensure_subs(id);
        if self.welcomed {
            self.subscribe_channel(id).await;
        }
    }

    async fn remove_channel(&mut self, id: u64) {
        if let Some(user) = self.channels.remove(&id) {
            log::info!(target: "hermes", "channel {} removed", user.channel_name);
            for topic in Topic::for_channel(id) {
                if let Some(sub) = self.subs.remove(&topic) {
                    log::info!(target: "hermes", "unsubscribing from {topic}");
                    let message_id = self.rng.nano_id();
                    self.send_message(&json!({
                        "type": "unsubscribe",
                        "id": message_id,
                        "unsubscribe": { "id": sub.id },
                        "timestamp": crate::logging::timestamp(),
                    }))
                    .await;
                }
            }
        }
        self.emit_status(None);
    }

    /// Drops one id's topics without touching the socket: the channel is
    /// gone (deleted/renamed), so there is nothing to unsubscribe from that
    /// the server would still recognize. Welcome replays never see it again.
    fn drop_channel(&mut self, id: u64) {
        self.channels.remove(&id);
        for topic in Topic::for_channel(id) {
            self.subs.remove(&topic);
        }
    }

    async fn connect(&mut self) {
        // One batched resolve per connect keeps the notify baseline fresh.
        // Ids that resolve to nothing are pruned from the persisted list.
        let ids: Vec<u64> = {
            let config = self.work.borrow().config_snapshot();
            let mut ids: Vec<u64> = config.channels.iter().map(|c| c.id).collect();
            for id in self.channels.keys() {
                if !ids.contains(id) {
                    ids.push(*id);
                }
            }
            ids
        };
        self.sync(&ids).await;
        if self.channels.is_empty() {
            // Connecting with no subscribable channel would just time out.
            log::info!(
                target: "hermes",
                "no channels resolved, retrying (typo in a login?)"
            );
            self.emit_status(Some("no channels found for the configured logins"));
            self.schedule_reconnect();
            return;
        }
        log::info!(target: "hermes", "connecting to {HERMES_URL}");
        match connect_async(HERMES_URL).await {
            Ok((socket, _)) => {
                log::info!(target: "hermes", "connected, waiting for welcome");
                self.socket = Some(socket);
                self.welcomed = false;
                self.last_message = Instant::now();
            }
            Err(error) => {
                log::info!(target: "hermes", "connect failed: {error}");
                self.on_disconnect().await;
            }
        }
    }

    /// Merges one gql response into the tracked state: every returned
    /// channel refreshes its baseline and gains its two subscription
    /// entries. Ids that resolve to nothing are dropped from the session
    /// and pruned from the persisted list with an error, instead of
    /// lingering as fake entries.
    async fn sync(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        match gql::fetch_users(ids, &[]).await {
            Ok(users) => {
                let mut seen = std::collections::HashSet::with_capacity(users.len());
                for user in users {
                    seen.insert(user.channel_id);
                    let id = user.channel_id;
                    self.upsert(user);
                    self.ensure_subs(id);
                }
                let missing: Vec<u64> = ids
                    .iter()
                    .copied()
                    .filter(|id| !seen.contains(id))
                    .collect();
                for id in &missing {
                    log::info!(target: "hermes", "channel {id} no longer resolves, dropping");
                    self.drop_channel(*id);
                }
                if !missing.is_empty() {
                    self.emit_status(Some("a channel no longer resolves and was removed"));
                    WorkState::apply_prune_ids(&self.work, &missing).await;
                }
            }
            Err(error) => {
                log::info!(target: "hermes", "channel refresh failed: {error}");
                // Still subscribe known ids: live notifications work from
                // pubsub alone, only the title baseline is stale.
                for id in ids {
                    self.ensure_subs(*id);
                }
            }
        }
    }

    fn upsert(&mut self, user: User) {
        self.channels.insert(user.channel_id, user);
    }

    /// Registers a channel's two topics if not registered already. Entries
    /// persist across reconnects; every welcome replays the whole map.
    fn ensure_subs(&mut self, id: u64) {
        for topic in Topic::for_channel(id) {
            self.subs.entry(topic).or_insert_with(|| Sub {
                id: self.rng.nano_id(),
                state: SubState::Pending,
            });
        }
    }

    /// Drops the socket and backs off. Reconnects always start fresh and
    /// replay all subscriptions; recovering server-side sessions is not
    /// worth the state machine for a handful of topics.
    async fn on_disconnect(&mut self) {
        self.close_socket().await;
        self.welcomed = false;
        self.emit_status(None);
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
    async fn on_tick(&mut self) {
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

    async fn handle_message(&mut self, text: &str) {
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
                notifier::notify(
                    summary,
                    &format!("watching {} channel(s)", self.channels.len()),
                    self.preferences.sound,
                    None,
                    None,
                )
                .await;
                self.emit_status(None);
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
                self.emit_status(None);
            }
            Some("unsubscribeResponse") => {
                log::info!(target: "hermes", "unsubscribe response: {text}");
            }
            Some("notification") => self.handle_notification(&message).await,
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
    async fn subscribe_channel(&mut self, id: u64) {
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

    async fn send_message(&mut self, message: &Value) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        if let Err(error) = socket.send(Message::text(message.to_string())).await {
            log::info!(target: "hermes", "send failed: {error}");
            self.on_disconnect().await;
        }
    }

    async fn handle_notification(&mut self, message: &Value) {
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
                self.on_broadcast_settings_update(channel_id, &pubsub).await;
            }
            Topic::VideoPlaybackById(channel_id) => {
                self.on_video_playback(channel_id, &pubsub).await;
            }
        }
    }

    async fn on_broadcast_settings_update(&mut self, channel_id: u64, pubsub: &Value) {
        let status = pubsub["status"].as_str().unwrap_or_default().to_owned();
        let old_status = pubsub["old_status"].as_str().unwrap_or_default();
        let game = pubsub["game"]
            .as_str()
            .filter(|game| !game.is_empty())
            .map(String::from);
        let old_game = pubsub["old_game"].as_str().unwrap_or_default();
        let Some(user) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let title_changed = user.stream_title.as_deref() != Some(status.as_str());
        let game_changed = game.as_deref() != user.game.as_ref().map(|game| game.name.as_str());
        let can_notify = self.preferences.notify_title_changes && !user.live;
        if can_notify && (title_changed || game_changed) {
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
            notifier::notify(
                &format!("[{}] {what} changed", user.channel_display_name),
                &lines.join("\n"),
                self.preferences.sound,
                Some(user.profile_image_url.as_str()).filter(|url| !url.is_empty()),
                Some(&user.channel_name),
            )
            .await;
        }
        user.stream_title = Some(status).filter(|title| !title.is_empty());
        let game_display_name = user.game.as_ref().map(|game| game.display_name.clone());
        user.game = game.zip(pubsub["game_id"].as_u64()).map(|(name, id)| Game {
            id,
            name,
            display_name: game_display_name.unwrap_or_default(),
        });
    }

    /// Handles `video-playback-by-id` pubsub payloads. Known `type` values:
    /// - `stream-up` / `stream-down`: live state flips; acted on below.
    /// - `viewcount`: periodic viewer-count heartbeat while live; ignored.
    ///   Observed shape (2026-09-12):
    ///   `{"type":"viewcount","viewers":583,"server_time":1789245232.108976,`
    ///   `"collaboration_status":"none","collaboration_viewers":0,`
    ///   `"costream_status":"","costream_viewers":0}`
    async fn on_video_playback(&mut self, channel_id: u64, pubsub: &Value) {
        match pubsub["type"].as_str() {
            Some("stream-up") => {
                // Notify from the cached baseline first; refresh from gql after.
                let cached = self.channels.get_mut(&channel_id).map(|user| {
                    user.live = true;
                    (
                        user.channel_name.clone(),
                        user.channel_display_name.clone(),
                        user.game.clone(),
                        user.stream_title.clone(),
                        user.profile_image_url.clone(),
                    )
                });
                self.emit_status(None);
                let notified = if let Some((login, display_name, game, title, avatar)) = cached {
                    Self::notify_live(
                        &display_name,
                        game.as_ref(),
                        title.as_deref(),
                        &avatar,
                        self.preferences.sound,
                        Some(&login),
                    )
                    .await;
                    true
                } else {
                    false
                };
                match gql::fetch_users(&[channel_id], &[]).await {
                    Ok(users) => {
                        let Some(mut user) =
                            users.into_iter().find(|user| user.channel_id == channel_id)
                        else {
                            return;
                        };
                        user.live = true;
                        if !notified {
                            Self::notify_live(
                                &user.channel_display_name,
                                user.game.as_ref(),
                                user.stream_title.as_deref(),
                                &user.profile_image_url,
                                self.preferences.sound,
                                Some(&user.channel_name),
                            )
                            .await;
                        }
                        self.upsert(user);
                        self.emit_status(None);
                    }
                    Err(error) => {
                        log::info!(target: "hermes", "stream-up refresh failed: {error}");
                    }
                }
            }
            Some("stream-down") => {
                if let Some(user) = self.channels.get_mut(&channel_id) {
                    user.live = false;
                }
            }
            _ => {}
        }
    }

    /// "is LIVE" notification shared by the optimistic (cached baseline)
    /// and fallback (fresh gql user) paths.
    async fn notify_live(
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
        notifier::notify(
            &format!("[{display_name}] is LIVE"),
            &body,
            sound,
            Some(avatar).filter(|url| !url.is_empty()),
            login,
        )
        .await;
    }

    fn status_for(&self, topic: Topic) -> SubStatus {
        match self.subs.get(&topic).map(|sub| sub.state) {
            None | Some(SubState::Pending) => SubStatus::Pending,
            Some(SubState::Subscribed) => SubStatus::Connected,
            Some(SubState::Failed) => SubStatus::Failed,
        }
    }

    fn emit_status(&self, error: Option<&str>) {
        let mut channels: Vec<ChannelStatus> = self
            .channels
            .iter()
            .map(|(id, user)| ChannelStatus {
                channel_id: *id,
                login: user.channel_name.clone(),
                display_name: user.channel_display_name.clone(),
                title_status: self.status_for(Topic::BroadcastSettingsUpdate(*id)),
                live_status: self.status_for(Topic::VideoPlaybackById(*id)),
            })
            .collect();
        channels.sort_by(|left, right| left.login.cmp(&right.login));
        let status = StatusSnapshot {
            connected: self.socket.is_some() && self.welcomed,
            error: error.map(String::from),
            channels,
            pending_adds: self.work.borrow().pending_adds.clone(),
        };
        self.work.borrow_mut().set_status(status);
    }
}
