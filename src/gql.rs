use compio::fs::File;
use compio::io::AsyncWriteAtExt;
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::error::Error as StdError;
use std::path::Path;
use std::time::Duration;

use crate::logging::parse_iso_ms;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

// cyper's `Client` is thread-local (`!Send + !Sync`, backed by `Rc`), so it
// cannot live in a shared static. Each compio runtime thread builds its own
// (fetches here are infrequent: connect/add/stream-up, plus avatar downloads).
fn client() -> Result<cyper::Client, Error> {
    Ok(cyper::Client::new()?)
}

const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName profileImageURL(width:70) broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";
const USERS_BY_LOGINS_QUERY: &str = "query UsersByLogins($logins:[String!]){users(logins:$logins){id login displayName profileImageURL(width:70) broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";

pub type Error = Box<dyn StdError + Send + Sync>;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Game {
    pub id: u64,
    pub name: String,
    pub display_name: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct User {
    pub channel_id: u64,
    pub channel_name: String,
    pub channel_display_name: String,
    pub profile_image_url: String,
    pub stream_id: u64,
    pub stream_title: Option<String>,
    /// Milliseconds since the unix epoch; `None` when offline.
    pub stream_start: Option<i64>,
    pub game: Option<Game>,
    pub live: bool,
}

// Typed mirror of the batched GQL response. Decoding straight into these
// (instead of `serde_json::Value`) skips the generic map allocation and
// gives every field a concrete type; unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct GqlOperationResponse {
    data: Option<GqlData>,
}

#[derive(Debug, Deserialize)]
struct GqlData {
    #[serde(default)]
    users: Option<Vec<Option<GqlUser>>>,
}

#[derive(Debug, Deserialize)]
struct GqlUser {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    login: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "profileImageURL")]
    profile_image_url: Option<String>,
    #[serde(rename = "broadcastSettings")]
    broadcast: Option<GqlBroadcast>,
    stream: Option<GqlStream>,
}

#[derive(Debug, Deserialize)]
struct GqlBroadcast {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    title: Option<String>,
    game: Option<GqlGame>,
}

#[derive(Debug, Deserialize)]
struct GqlGame {
    #[serde(default, deserialize_with = "de_opt_u64")]
    id: Option<u64>,
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlStream {
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

// GQL `ID` scalars arrive as strings ("12345") but may serialize as
// numbers; unparseable values collapse to `None` so the user is skipped
// instead of failing the whole batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IdRepr {
    Num(u64),
    Text(String),
    Other(serde::de::IgnoredAny),
}

fn de_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<IdRepr>::deserialize(deserializer)?;
    Ok(raw.and_then(|repr| match repr {
        IdRepr::Num(id) => Some(id),
        IdRepr::Text(text) => text.parse().ok(),
        IdRepr::Other(_) => None,
    }))
}

impl GqlUser {
    fn into_user(self) -> Option<User> {
        let broadcast = self.broadcast?;
        Some(User {
            channel_id: self.id?,
            channel_name: self.login.filter(|s| !s.is_empty())?,
            channel_display_name: self.display_name.filter(|s| !s.is_empty())?,
            profile_image_url: self.profile_image_url.filter(|s| !s.is_empty())?,
            stream_id: broadcast.id?,
            stream_title: broadcast.title.filter(|title| !title.is_empty()),
            stream_start: self
                .stream
                .as_ref()
                .and_then(|stream| stream.created_at.as_deref())
                .and_then(parse_iso_ms),
            game: broadcast.game.and_then(|game| {
                Some(Game {
                    id: game.id?,
                    name: game.name?,
                    display_name: game.display_name?,
                })
            }),
            live: self.stream.is_some(),
        })
    }
}

pub async fn fetch_users(ids: &[u64], logins: &[String]) -> Result<Vec<User>, Error> {
    let mut operations = Vec::with_capacity(2);
    if !ids.is_empty() {
        operations.push(json!({
            "query": USERS_BY_IDS_QUERY,
            "variables": {
                "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>()
            },
            "operationName": "UsersByIds",
        }));
    }
    if !logins.is_empty() {
        operations.push(json!({
            "query": USERS_BY_LOGINS_QUERY,
            "variables": { "logins": logins },
            "operationName": "UsersByLogins",
        }));
    }
    log::info!(target: "gql", "request: ids={ids:?} logins={logins:?}");

    let payload = serde_json::to_vec(&Value::Array(operations))?;
    let client = client()?;
    let response = compio::time::timeout(
        GQL_TIMEOUT,
        client
            .post(GQL_URL)?
            .header("Content-Type", "application/json")?
            .header("Client-ID", CLIENT_ID)?
            .body(payload)
            .send(),
    )
    .await
    .map_err(|_| "gql request timed out")??;

    if !response.status().is_success() {
        return Err(format!("gql returned status {}", response.status()).into());
    }
    let body = compio::time::timeout(GQL_TIMEOUT, response.bytes())
        .await
        .map_err(|_| "gql body read timed out")??;
    // Single-pass typed decode: no intermediate `Value` map. A shape
    // mismatch surfaces to the caller; per-user gaps still collapse to `None` above.
    let responses: Vec<GqlOperationResponse> = serde_json::from_slice(&body)?;
    let users = responses
        .into_iter()
        .filter_map(|response| response.data.and_then(|data| data.users))
        .flatten()
        .filter_map(|raw| raw.and_then(GqlUser::into_user))
        .collect::<Vec<_>>();

    log::info!(
        target: "gql",
        "parsed {} user(s): {}",
        users.len(),
        users
            .iter()
            .map(|user| format!(
                "{}({}){}",
                user.channel_name,
                user.channel_id,
                if user.live { " live" } else { "" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(users)
}

/// downloads an arbitrary cdn file (e.g. a profile picture) to `path`,
/// streaming body chunks straight to disk so the whole body is never
/// buffered in memory. A failed download removes the partial file, since
/// the avatar cache skips urls whose file already exists and would
/// otherwise pin the corruption.
pub async fn fetch_file(url: &str, path: impl AsRef<Path>) -> Result<(), Error> {
    let response = compio::time::timeout(GQL_TIMEOUT, client()?.get(url)?.send())
        .await
        .map_err(|_| "image request timed out")??;
    if !response.status().is_success() {
        return Err(format!("image returned status {}", response.status()).into());
    }
    let path = path.as_ref();
    if let Err(error) = stream_to_file(response, path).await {
        compio::fs::remove_file(path).await.ok();
        return Err(error);
    }
    Ok(())
}

/// Pipes one response body to `path` chunk by chunk over async file writes
/// (no intermediate buffer); each stalled chunk trips `GQL_TIMEOUT`.
async fn stream_to_file(response: cyper::Response, path: &Path) -> Result<(), Error> {
    let mut file = File::create(path).await?;
    let mut stream = response.bytes_stream();
    let mut pos: u64 = 0;
    loop {
        let next = compio::time::timeout(GQL_TIMEOUT, stream.next())
            .await
            .map_err(|_| "image body read timed out")?;
        let Some(result) = next else { break };
        let chunk = result?;
        let len = chunk.len() as u64;
        file.write_all_at(chunk, pos).await.0?;
        pos += len;
    }
    file.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::{GqlOperationResponse, GqlUser};

    fn decode_users(body: &[u8]) -> Vec<super::User> {
        let responses: Vec<GqlOperationResponse> = serde_json::from_slice(body).unwrap();
        responses
            .into_iter()
            .filter_map(|response| response.data.and_then(|data| data.users))
            .flatten()
            .filter_map(|raw| raw.and_then(GqlUser::into_user))
            .collect()
    }

    #[test]
    fn typed_decode_keeps_live_user_with_game() {
        let body = br#"[
            {"data": {"users": [
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}}
        ]"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.channel_id, 123);
        assert_eq!(user.channel_name, "alice");
        assert_eq!(user.channel_display_name, "Alice");
        assert_eq!(user.profile_image_url, "http://x/y.png");
        assert_eq!(user.stream_id, 999);
        assert_eq!(user.stream_title.as_deref(), Some("hi"));
        assert_eq!(
            user.stream_start,
            crate::logging::parse_iso_ms("2026-09-10T18:35:06Z")
        );
        let game = user.game.as_ref().expect("game should decode");
        assert_eq!(game.id, 10);
        assert_eq!(game.name, "g");
        assert_eq!(game.display_name, "G");
        assert!(user.live);
    }

    #[test]
    fn typed_decode_accepts_numeric_ids_and_offline_user() {
        let body = br#"[
            {"data": {"users": [
                {"id": 456, "login": "bob", "displayName": "Bob",
                 "profileImageURL": "http://x/z.png",
                 "broadcastSettings": {"id": 777, "title": "", "game": null},
                 "stream": null}
            ]}}
        ]"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.channel_id, 456);
        assert_eq!(user.stream_id, 777);
        assert!(user.stream_title.is_none());
        assert!(user.stream_start.is_none());
        assert!(user.game.is_none());
        assert!(!user.live);
    }

    #[test]
    fn typed_decode_skips_bad_entries_but_keeps_batch() {
        let body = br#"[
            {"data": {"users": [
                null,
                {"id": "bad", "login": "ghost",
                 "broadcastSettings": {"id": "1", "title": "t", "game": null},
                 "stream": null},
                {"id": "789", "login": "nobra",
                 "broadcastSettings": null, "stream": null},
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "broadcastSettings": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}},
            {"data": null},
            {"data": {"users": null}}
        ]"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(users[0].channel_id, 123);
    }

    /// Serves one static HTTP response on loopback; the returned future
    /// must be polled concurrently with the client.
    async fn serve_once(response: Vec<u8>) -> (u16, impl Future<Output = ()>) {
        use compio::io::{AsyncRead as _, AsyncWriteExt as _};

        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve = async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Drain the request headers first: closing with unread data
            // aborts the connection (RST on Windows) and trashes the
            // in-flight response.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let res = conn.read([0u8; 1024]).await;
                let n = res.0.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&res.1[..n]);
            }
            conn.write_all(response).await.0.unwrap();
        };
        (port, serve)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    #[test]
    fn fetch_file_streams_chunks_to_disk() {
        // Larger than a single TCP segment so the download actually spans
        // multiple chunks; positional contents verify chunk order.
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let path = std::env::temp_dir().join(format!("siphon-fetch-ok-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        block_on(async {
            let (port, serve) = serve_once(response).await;
            let url = format!("http://127.0.0.1:{port}/avatar.png");
            let ((), result) = futures_util::join!(serve, super::fetch_file(&url, &path));
            result.unwrap();
        });

        assert_eq!(std::fs::read(&path).unwrap(), body);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fetch_file_removes_partial_file_on_truncated_body() {
        let body: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        // Lie about the length, then close early: the client must error and
        // the partial file must go so the cache never pins it.
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len() + 1024
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let path =
            std::env::temp_dir().join(format!("siphon-fetch-truncated-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        block_on(async {
            let (port, serve) = serve_once(response).await;
            let url = format!("http://127.0.0.1:{port}/avatar.png");
            let ((), result) = futures_util::join!(serve, super::fetch_file(&url, &path));
            assert!(result.is_err(), "truncated body should fail");
        });

        assert!(!path.exists(), "partial file should be removed");
    }

    #[test]
    fn fetch_file_rejects_error_status_without_creating_file() {
        let response =
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let path =
            std::env::temp_dir().join(format!("siphon-fetch-404-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        block_on(async {
            let (port, serve) = serve_once(response).await;
            let url = format!("http://127.0.0.1:{port}/avatar.png");
            let ((), result) = futures_util::join!(serve, super::fetch_file(&url, &path));
            assert!(result.is_err(), "error status should fail");
        });

        assert!(!path.exists(), "no file should be created");
    }
}
