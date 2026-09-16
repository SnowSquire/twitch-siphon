# compio reference (for this repo's tokio → compio migration)

The old `https://compio.rs/docs/...` URLs are dead (404). Canonical docs are on
docs.rs: `compio 0.19.x`, `cyper 0.9.x`, `compio-runtime 0.12.x`, `compio-ws 0.4.x`.
Versions below were verified against those pages (Aug 2026).

## 1. Big picture

* compio is a **completion-based, thread-per-core** runtime (IOCP on Windows,
  io_uring on Linux, polling fallback). Name = "completion IO", inspired by monoio.
* `compio::runtime::Runtime` is **thread-local and `!Send + !Sync`**. There is no
  multi-threaded runtime and no `Handle` you can share across threads like
  `tokio::runtime::Handle`. Each OS thread that does async IO owns its own `Runtime`.
* Consequence for us: the UI thread (eframe) cannot own the async runtime while also
  running the event loop. Pattern is:
  ```rust
  let (tx, rx) = futures_channel::mpsc::unbounded();
  std::thread::Builder::new().name("hermes".into()).spawn(move || {
      compio::runtime::Runtime::new()
          .expect("compio runtime")
          .block_on(session_main_loop(rx));
  })?;
  // UI thread only holds `tx` (which is Send + Sync) and sends Commands.
  // One-shot fetches from the UI thread: spawn a short-lived OS thread with its
  // own fresh `Runtime::new().block_on(fetch)` — fetches are infrequent here.
  ```
* For real multi-threaded dispatch across compio workers there is
  `compio::dispatcher::Dispatcher` (`compio --features dispatcher`):
  ```rust
  use compio::dispatcher::Dispatcher;
  let d = Dispatcher::builder().worker_threads(4).build().unwrap();
  let out = d.dispatch(|| async { 42 }).await;
  ```
  We deliberately do **not** use it: one background runtime thread is enough and
  keeps the port minimal.
* Futures from compio (`WebSocketStream`, cyper responses) are often **`!Send`**.
  That's fine on compio (thread-per-core) but would not compile on
  `tokio::runtime::Builder::new_multi_thread`. Don't add `Send` bounds.
* Entry macro (only when a whole binary is async; we don't use it because eframe
  owns `main`):
  ```rust
  #[compio::main]
  async fn main() { /* ... */ }
  // requires compio --features macros
  ```

## 2. Cargo features we use

```toml
compio = { version = "0.19", features = [
  "macros",      # #[compio::main] (docs only; harmless to keep)
  "runtime",     # default; Runtime + spawn + block_on (default already)
  "time",        # compio::time::{sleep, timeout, interval}
  "ws",          # compio::ws::{connect_async, WebSocketStream}
  "ws-connect",  # enables connect_async client handshake
  "tls",         # MaybeTlsStream plumbing
  "rustls",      # TLS backend for wss
  "webpki-roots",# Mozilla roots (matches old hyper-rustls webpki-roots)
] }
cyper = { version = "0.9", features = ["json"] }
# default = native-tls (SChannel on Windows); json adds Response::json().
# If you want rustls instead of SChannel: features = ["json", "rustls"]
# (rustls pulls rustls-platform-verifier).
futures-channel = "0.3"  # UnboundedSender/Receiver replaces tokio::sync::mpsc
futures-util = { version = "0.3", features = ["std", "sink"] }  # select!, StreamExt, SinkExt
```

What we delete: `tokio`, `hyper`, `hyper-util`, `hyper-rustls`, `tokio-tungstenite`,
`http-body-util`. `http` types are no longer built by hand (cyper builds them).

## 3. Runtime (`compio::runtime`)

docs.rs: `compio::runtime`, struct `Runtime`.

```rust
use compio::runtime::Runtime;

// create + drive (thread-local! cannot be sent to another thread)
let rt = Runtime::new().unwrap();
rt.block_on(async { println!("hello"); 42 });

// inside a running runtime:
let h: compio::runtime::JoinHandle<T> = compio::runtime::spawn(async { 42 });
let v: T = h.await.unwrap(); // JoinError on cancel/panic
compio::runtime::spawn_blocking(|| std::fs::read("x")).await.unwrap();

// current-runtime access (panics if none):
Runtime::with_current(|rt| rt.spawn(async {}));
let opt: Option<Runtime> = Runtime::try_current();
```

tokio → compio:

| tokio | compio |
|---|---|
| `Builder::new_multi_thread().enable_all().build()` | `Runtime::new()` on a dedicated thread |
| `handle.spawn(fut)` (Send) | `rt.spawn(fut)` / `compio::runtime::spawn(fut)` inside runtime (!Send ok) |
| `handle.block_on` / `runtime.block_on` | `rt.block_on(fut)` (same thread only) |
| `runtime.shutdown_background()` | drop `Runtime`; thread exits when `block_on` future returns |
| `#[tokio::main]` | `#[compio::main]` (needs `macros` feature) |

## 4. Time (`compio::time`)

docs.rs: `compio::time::{sleep, sleep_until, timeout, timeout_at, interval, interval_at}`.
Same names/semantics as tokio, including `interval` first tick completes immediately.

```rust
use std::time::Duration;
compio::time::sleep(Duration::from_secs(1)).await;
let res: Result<T, compio::time::Elapsed> =
    compio::time::timeout(Duration::from_secs(15), fut).await;
let mut tick = compio::time::interval(Duration::from_secs(1));
loop { tick.tick().await; /* ... */ }
```

## 5. HTTP client (`cyper::Client` — high level, replaces hyper)

docs.rs: `cyper::{Client, ClientBuilder, RequestBuilder, Response, Body}`.

```rust
use cyper::Client;

// NOTE (verified against cyper 0.9.0 source): Client is thread-local
// (!Send + !Sync, backed by Rc<ClientInner>) despite what the docs.rs
// auto-trait list suggests. Do NOT put it in an Arc/static shared across
// threads. Build one per compio runtime thread (per fetch is fine here).
let client = Client::new()?; // -> cyper::Result<Client>
let resp = client
    .post("https://gql.twitch.tv/gql")?        // get/post/put/patch/delete/head/request
    .header("Client-ID", "kimne78kx3ncx6brgo4mv6wki5h1ko")? // -> Result<Self> (!)
    .header("Content-Type", "application/json")?
    .body(payload_bytes)?                       // Into<cyper::Body>: Vec<u8>, String, Bytes, &[u8]
    // .json(&value)?                           // needs `json` feature, sets content-type
    .send()                                     // async -> Result<Response>
    .await?;
```

Timeouts: cyper has no per-request timeout arg, wrap with `compio::time::timeout`:

```rust
let resp = compio::time::timeout(
    std::time::Duration::from_secs(15),
    client.post(url)?.header("Client-ID", ID)?.body(bytes)?.send(),
).await.map_err(|_| "gql request timed out")??;
```

`Response`:

```rust
resp.status().is_success(); // http::StatusCode, same as hyper
let bytes: bytes::Bytes = resp.bytes().await?;
let text: String = resp.text().await?;          // charset/BOM aware
let v: serde_json::Value = resp.json().await?;  // needs `json` feature
```

`Body` (`cyper::Body`): `Body::empty()`, `From<Vec<u8>/String/Bytes/&'static [u8]/&'static str/File>`.
`ClientBuilder` for non-default config: `Client::builder().default_headers(map).redirect(policy).build()?`,
`use_native_tls()` / `use_rustls_*()` to pin the TLS backend.

Gotchas vs hyper:

* every builder step that can fail returns `cyper::Result` — use `?` on
  `.post(url)?`, `.header(k, v)?`, not just on `.send()`.
* `Client::new()` itself returns `Result`; build one per call site / per runtime
  thread. A shared `static OnceLock<Arc<Client>>` does NOT compile (`!Send+!Sync`).
* no `TokioExecutor` / connectors to wire up — cyper owns the compio transport.

## 6. `cyper-core` (low level — we don't use it directly)

`cyper-core` is the connection/TLS/DNS layer underneath `cyper` (pooling, proxy,
`resolve::Resolve`, hickory-dns, http2/http3 toggles). The old doc slug
`/cyper/core` pointed here. Rule of thumb: if `Client/RequestBuilder/Response`
covers it, stay on `cyper`; only drop to `cyper-core` for custom resolvers,
proxy plumbing, or raw `hyper` interop (`Service<Request<Body>>` impl on `Client`
behind `stream` feature).

## 7. WebSocket (`compio::ws` — replaces tokio-tungstenite)

docs.rs: `compio::ws::{connect_async, WebSocketStream, Config}`, re-exports
`compio::ws::tungstenite::{Message, Error, protocol::...}` (same tungstenite API).

```rust
use compio::ws::{connect_async, WebSocketStream};
use compio::net::TcpStream; // concrete stream type in the return type

let (mut socket, _http_resp): (WebSocketStream<TcpStream>, _) =
    connect_async("wss://hermes.twitch.tv/v1?clientId=...").await?;

// direct async methods (preferred over Sink/Stream when you own the socket):
socket.send(compio::ws::tungstenite::Message::text(json)).await?;
let msg: compio::ws::tungstenite::Message = socket.read().await?;
socket.flush().await?;
socket.close(None).await?;

// Stream/Sink impls also exist (Item = Result<Message, Error>):
use futures_util::{SinkExt, StreamExt};
let text: String = match socket.next().await {
    Some(Ok(m)) => m.into_text()?,   // binary frames carry the same JSON
    Some(Err(e)) | None => { /* treat as disconnect */ }
};
```

Notes:

* `WebSocketStream<S: Splittable>` is **`!Send + !Sync`** — keep the whole hermes
  session future on the one compio thread; never move the socket to another thread.
* `Message::text(String)` takes an owned `String` (not `&str`); `into_text()`
  returns `Result<Utf8Bytes, Error>` — map `Ignored` on binary-parse failure as before.
* TLS (`wss://`) is handled inside `connect_async` (feature `ws-connect` + a TLS
  backend). No `MaybeTlsStream<TcpStream>` type alias to name in our code if we
  let inference pick `WebSocketStream<TcpStream>`; importing `compio::net::TcpStream`
  needs `compio --features net` (pulled in transitively by `ws-connect`, but be
  explicit if you name the type).

## 8. Channels + select (replaces `tokio::sync::mpsc` + `tokio::select!`)

compio has no mpsc — use `futures_channel::mpsc` (runtime-agnostic, works on compio):

```rust
let (tx, mut rx) = futures_channel::mpsc::unbounded::<Command>();
tx.unbounded_send(cmd).map_err(|e| e.to_string())?; // Send path is sync
// recv path is a Stream:
use futures_util::StreamExt;
while let Some(cmd) = rx.next().await { /* ... */ }
```

`UnboundedSender` is `Send + Sync + Clone`; `UnboundedReceiver` is `Send` but
`!Sync` — hold it on the runtime thread only (same as before with tokio).

Select over command / socket / ticker with `futures_util::select!` (futures must be
`FusedFuture`; fuse with `.fuse()` and pin with `futures_util::pin_mut!`):

```rust
use futures_util::{FutureExt, StreamExt};
futures_util::select! {
    cmd = rx.next().fuse() => { /* Option<Command>, None = senders dropped → shut down */ }
    msg = async {
        match socket.as_mut() {
            Some(s) => s.next().await,
            None => futures_util::future::pending().await, // park when disconnected
        }
    }.fuse() => { /* Option<Result<Message, _>> */ }
    _ = tick.tick().fuse() => { /* 1s housekeeping */ }
}
```

## 9. Architecture (what this repo ended up with)

Two threads, three channels, one doorbell — no `Arc`/`Mutex` for app state:

* **GUI thread** (eframe): purely presentational. Renders the latest
  `FrameState`, sends `UiIntent`s. Plain fields, `&mut self`.
* **Work thread**: one compio runtime hosting the hermes session task, a 50ms
  poll task (drains UI intents + tray events via non-blocking `try_recv`),
  and one-shot gql resolve tasks. All state lives here in `WorkState`,
  shared between tasks as `Rc<RefCell<…>>` with short scoped borrows.
* **Bridges** (`crossbeam-channel`, already in the tree): `UiIntent`
  GUI→work, `FrameState` work→GUI (pushed on change, GUI keeps latest),
  `TrayAction` work→GUI. Plus `GuiWaker` (a shared `egui::Context` handle):
  channels can't wake winit, so work pokes it after every push.

Per-file map:

* `Cargo.toml`: no tokio/hyper/hyper-util/hyper-rustls/tokio-tungstenite/
  http-body-util/gtk; compio (runtime/time/net/ws/ws-connect/tls/rustls/
  webpki-roots/macros), cyper/json, futures-channel (session inbox),
  crossbeam-channel (GUI↔work + tray receivers), `tray-icon` with `ksni`
  on Linux.
* `gql.rs`: `cyper::Client` built per call (thread-local `!Send+!Sync`,
  never shared); `compio::time::timeout` instead of tokio's;
  `resp.bytes().await` instead of body collect.
* `hermes.rs`: `compio::ws` instead of tokio-tungstenite;
  `futures_util::select!` instead of `tokio::select!`; `spawn(WorkContext)`
  bootstraps runtime + session + poll task; `Session` reports via
  `Rc<RefCell<WorkState>>`.
* `state.rs`: the wire protocol (`UiIntent`, `FrameState`, `GuiWaker`,
  `WorkContext`) plus single-threaded `WorkState` (config/persist/intents/
  resolve-apply/snapshots). No `Shared`, no `Mutex`.
* `tray.rs`: `tray-icon` build/menu/icon plus `drain_pending()` for the
  poll task. No watcher thread.
* `app.rs`/`main.rs`: channel wiring only; `SiphonApp` takes
  `Option<Tray>` and channel ends.
