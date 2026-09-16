use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use compio::net::TcpStream;
use compio::ws::tungstenite::Message;
use compio::ws::{connect_async, WebSocketStream};
use futures_channel::mpsc;
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt as _, StreamExt as _};
use serde_json::{json, Value};

use crate::gql::{self, Game, User};
use crate::notifier;
use crate::state::{UiIntent, WorkContext, WorkState};
use crate::tray;

const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSED_LIMIT: u64 = 2;
const TOPIC_PREFIXES: [&str; 2] = ["broadcast-settings-update", "video-playback-by-id"];

/// One imperative mutation, sent by the ui layer instead of diffing whole
/// settings snapshots against the session state. Adds carry only the login
/// (the only thing the UI knows); removes carry the resolved id. Only
/// resolved channels are ever tracked — unresolvable logins are dropped and
/// surfaced as errors, never stored.
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
}

struct Sub {
    id: String,
    state: SubState,
}

type WsStream = WebSocketStream<TcpStream>;

/// How often the poll task drains the sync inboxes (ui intents, tray
/// events). Bounds click-to-effect latency; the runtime otherwise sleeps.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One finished channel resolve: the login plus the gql result.
type ResolveDone = (String, Result<Vec<User>, gql::Error>);
/// In-flight resolve future. `!Send` is fine: everything stays on this one
/// thread-per-core runtime thread.
type ResolveFuture = Pin<Box<dyn Future<Output = ResolveDone>>>;

/// Drives the whole work side on the calling thread, which must be a
/// dedicated OS thread owning this compio runtime: the hermes session plus a
/// poll routine that bridges the sync inboxes (GUI intents, tray events) into
/// it. Everything here is single-threaded (`Rc`, no `Mutex`); crossbeam
/// channels are the only bridge to the GUI thread.
///
/// This function never returns (both routines run until the process exits);
/// the caller owns thread creation so this module never spawns anything.
pub fn run(ctx: WorkContext) {
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(async move {
            let (session_tx, command_rx) = mpsc::unbounded();
            let work = Rc::new(RefCell::new(WorkState::new(ctx, session_tx)));
            work.borrow().seed();
            let mut session = Session::new(Rc::clone(&work), command_rx);
            // Joined, not spawned: both routines are polled on this one
            // thread until the process exits.
            futures_util::future::join(poll_loop(work), session.main_loop()).await;
        });
}

/// Bridges the sync inboxes into the runtime: drains UI intents and tray
/// events with non-blocking `try_recv` on a short interval, so neither the
/// GUI thread nor this runtime ever blocks. Intent handling is synchronous;
/// resolves wait in a `FuturesUnordered` polled by this same routine, so no
/// task is ever spawned.
///
/// The `select!` over the boxed resolve stream trips `tail_expr_drop_order`
/// (boxed `dyn Future` counts as a custom destructor); drop order is
/// irrelevant here, and the crate is edition 2021, so the lint is allowed
/// locally instead of restructuring the poll.
#[allow(tail_expr_drop_order)]
async fn poll_loop(work: Rc<RefCell<WorkState>>) {
    let mut tick = compio::time::interval(POLL_INTERVAL);
    let mut pending: FuturesUnordered<ResolveFuture> = FuturesUnordered::new();
    loop {
        if pending.is_empty() {
            tick.tick().await;
            drain_once(&work, &pending).await;
        } else {
            let tick_fut = tick.tick().fuse();
            let next_fut = pending.next().fuse();
            futures_util::pin_mut!(tick_fut, next_fut);
            futures_util::select! {
                _ = tick_fut => {
                    drain_once(&work, &pending).await;
                }
                done = next_fut => {
                    if let Some((login, result)) = done {
                        WorkState::finish_add_login(&work, login, result).await;
                    }
                }
            }
        }
    }
}

/// One poll tick: drains UI intents (queuing resolves instead of spawning
/// them) and tray events.
async fn drain_once(work: &Rc<RefCell<WorkState>>, pending: &FuturesUnordered<ResolveFuture>) {
    // Owned up front: the `Ref` must not survive into the awaits below.
    let intents: Vec<UiIntent> = work.borrow().drain_intents();
    for intent in intents {
        match intent {
            UiIntent::AddLogin(login) => {
                if let Some(login) = work.borrow_mut().begin_add_login(&login) {
                    pending.push(Box::pin(async move {
                        let users = gql::fetch_users(&[], std::slice::from_ref(&login)).await;
                        (login, users)
                    }));
                }
            }
            UiIntent::RemoveChannel(id) => {
                WorkState::apply_remove_channel(work, id).await;
            }
            UiIntent::SetNotifyTitleChanges(value) => {
                WorkState::apply_notify_title_changes(work, value).await;
            }
            UiIntent::SetSound(value) => {
                WorkState::apply_sound(work, value).await;
            }
        }
    }
    for action in tray::drain_pending() {
        work.borrow().forward_tray(action);
    }
}

/// Notification preferences the ui can change at runtime.
struct Preferences {
    notify_title_changes: bool,
    sound: bool,
}

struct Session {
    work: Rc<RefCell<WorkState>>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    channels: HashMap<u64, User>,
    subs: HashMap<String, Sub>,
    preferences: Preferences,
    socket: Option<WsStream>,
    welcomed: bool,
    keepalive_secs: u64,
    last_message: Instant,
    reconnect_at: Instant,
    reconnect_attempt: u32,
    /// `SplitMix64` state for [`Session::nano_id`]
    rng_state: u64,
}

impl Session {
    fn new(work: Rc<RefCell<WorkState>>, command_rx: mpsc::UnboundedReceiver<Command>) -> Self {
        Self {
            work,
            command_rx,
            channels: HashMap::new(),
            subs: HashMap::new(),
            preferences: Preferences {
                notify_title_changes: true,
                sound: true,
            },
            socket: None,
            welcomed: false,
            keepalive_secs: 15,
            last_message: Instant::now(),
            reconnect_at: Instant::now(),
            reconnect_attempt: 0,
            rng_state: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0x853c_49e6_748f_ea9b, |elapsed| elapsed.as_nanos() as u64),
        }
    }

    async fn main_loop(&mut self) {
        log::info!(target: "hermes", "worker started");
        let mut tick = compio::time::interval(Duration::from_secs(1));
        loop {
            if self.socket.is_none() && self.has_work() && Instant::now() >= self.reconnect_at {
                self.connect().await;
            }
            // When disconnected there is no socket to poll, so park that
            // branch on a never-resolving future until the next connect.
            let command_fut = self.command_rx.next().fuse();
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
                    let Some(command) = command else { return };
                    self.on_command(command).await;
                }
                message = socket_fut => {
                    // Convert to an owned event first so the tungstenite
                    // `Message` (which owns refcounted bytes) is dropped
                    // before any await below runs.
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
                        // Text and binary frames carry the same json payloads.
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

    /// Channels exist that could be subscribed once resolved.
    fn has_work(&self) -> bool {
        !self.channels.is_empty()
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
        // The bridge may have fetched already for its persist decision; this
        // second fetch is deliberate (baseline seeding is this task's job)
        // and costs one request per explicit user add.
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
            for prefix in TOPIC_PREFIXES {
                let topic = format!("{prefix}.{id}");
                if let Some(sub) = self.subs.remove(&topic) {
                    log::info!(target: "hermes", "unsubscribing from {topic}");
                    let message_id = self.nano_id();
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
        for prefix in TOPIC_PREFIXES {
            self.subs.remove(&format!("{prefix}.{id}"));
        }
    }

    async fn connect(&mut self) {
        // One batch fetch refreshes the notify baseline, so every
        // (re)connect starts from fresh gql state. Ids that no longer
        // resolve are pruned from the persisted list instead of lingering.
        let ids: Vec<u64> = self.channels.keys().copied().collect();
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
        for prefix in TOPIC_PREFIXES {
            let topic = format!("{prefix}.{id}");
            if self.subs.contains_key(&topic) {
                continue;
            }
            let sub_id = self.nano_id();
            self.subs.insert(
                topic,
                Sub {
                    id: sub_id,
                    state: SubState::Pending,
                },
            );
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
                // a nonzero attempt count means this welcome followed at
                // least one disconnect, so it is a reconnect
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
                let Some(subscription_id) =
                    message["subscribeResponse"]["subscription"]["id"].as_str()
                else {
                    return;
                };
                let ok = message["subscribeResponse"]["result"].as_str() == Some("ok");
                let topic = match self
                    .subs
                    .iter_mut()
                    .find(|(_, sub)| sub.id == subscription_id)
                {
                    Some((topic, sub)) => {
                        sub.state = if ok {
                            SubState::Subscribed
                        } else {
                            SubState::Failed
                        };
                        topic.clone()
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
        let pending: Vec<(String, String)> = self
            .subs
            .iter_mut()
            .map(|(topic, sub)| {
                sub.state = SubState::Pending;
                (sub.id.clone(), topic.clone())
            })
            .collect();
        for (sub_id, topic) in pending {
            self.send_subscribe(&sub_id, &topic).await;
        }
    }

    /// Subscribes a single channel's topics; used when a channel is added
    /// to an already-live session (welcomes replay everything instead).
    async fn subscribe_channel(&mut self, id: u64) {
        let pending: Vec<(String, String)> = TOPIC_PREFIXES
            .iter()
            .map(|prefix| format!("{prefix}.{id}"))
            .filter_map(|topic| self.subs.get(&topic).map(|sub| (sub.id.clone(), topic)))
            .collect();
        if !pending.is_empty() {
            log::info!(
                target: "hermes",
                "subscribing {} topic(s) for {id}",
                pending.len()
            );
        }
        for (sub_id, topic) in pending {
            self.send_subscribe(&sub_id, &topic).await;
        }
    }

    async fn send_subscribe(&mut self, subscription_id: &str, topic: &str) {
        log::info!(target: "hermes", "subscribing to {topic}");
        let id = self.nano_id();
        self.send_message(&json!({
            "type": "subscribe",
            "id": id,
            "subscribe": {
                "id": subscription_id,
                "type": "pubsub",
                "pubsub": { "topic": topic },
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
        let Some(subscription_id) = message["notification"]["subscription"]["id"].as_str() else {
            return;
        };
        let Some((topic, state)) = self
            .subs
            .iter()
            .find(|(_, sub)| sub.id == subscription_id)
            .map(|(topic, sub)| (topic.clone(), sub.state))
        else {
            return;
        };
        if state != SubState::Subscribed {
            return;
        }
        let Some((kind, channel_id)) = topic.split_once('.') else {
            return;
        };
        let Ok(channel_id) = channel_id.parse::<u64>() else {
            return;
        };
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
        match kind {
            "broadcast-settings-update" => {
                self.on_broadcast_settings_update(channel_id, &pubsub).await;
            }
            "video-playback-by-id" => self.on_video_playback(channel_id, &pubsub).await,
            _ => {}
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
        // a local because nursery's suspicious_operation_groupings misfires
        // when the two fields appear in one && chain
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
        // mirrors the TS client's state updates for these events
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
                // Optimistic: flip live and notify from the cached baseline
                // first, so neither waits for the gql round-trip below.
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
                // Refresh the details before returning; a channel unknown
                // at event time still notifies here, as before.
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

    fn status_for(&self, prefix: &str, id: u64) -> SubStatus {
        match self
            .subs
            .get(&format!("{prefix}.{id}"))
            .map(|sub| sub.state)
        {
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
                title_status: self.status_for("broadcast-settings-update", *id),
                live_status: self.status_for("video-playback-by-id", *id),
            })
            .collect();
        channels.sort_by(|left, right| left.login.cmp(&right.login));
        let status = StatusSnapshot {
            connected: self.socket.is_some() && self.welcomed,
            error: error.map(String::from),
            channels,
        };
        self.work.borrow_mut().set_status(status);
    }

    fn nano_id(&mut self) -> String {
        const ALPHABET: &[u8; 64] =
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_-";
        const LEN: usize = 21;
        let mut acc: u128 = (u128::from(self.next_u64()) << 64) | u128::from(self.next_u64());
        let mut out = String::with_capacity(LEN);
        for _ in 0..LEN {
            out.push(ALPHABET[(acc & 63) as usize] as char);
            acc >>= 6;
        }
        out
    }

    /// One `SplitMix64` step (Steele et al.) over the session seed. Zero is
    /// a valid state and nearby seeds decorrelate immediately, which suits
    /// a wall-clock-seeded `u64`.
    #[allow(clippy::missing_const_for_fn)]
    fn next_u64(&mut self) -> u64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}
