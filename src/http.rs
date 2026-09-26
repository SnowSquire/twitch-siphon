use compio::fs::File;
use compio::io::AsyncWriteAtExt;
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

use crate::logging::parse_iso_ms;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

// cyper's `Client` is thread-local (`!Send + !Sync`, backed by `Rc`), so it
// cannot live in a shared static. Each compio runtime thread builds its own
// (fetches here are infrequent: connect/add/stream-up, plus avatar downloads).
pub(crate) fn client() -> anyhow::Result<cyper::Client> {
    Ok(cyper::Client::new()?)
}

const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName profileImageURL(width:70) lastBroadcast{id title game{id name displayName}}stream{id createdAt viewersCount collaborationViewersCount}}}";
const USER_BY_LOGIN_QUERY: &str = "query UserByLogin($login:String!){user(login:$login){id login displayName profileImageURL(width:70) lastBroadcast{id title game{id name displayName}}stream{id createdAt viewersCount collaborationViewersCount}}}";

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Game {
    pub id: u64,
    pub name: String,
    pub display_name: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ResolvedChannel {
    pub channel_id: u64,
    pub channel_name: String,
    pub channel_display_name: String,
    pub profile_image_url: String,
    pub stream_id: u64,
    pub stream_title: Option<String>,
    /// Milliseconds since the unix epoch; `None` when offline.
    pub stream_start: Option<i64>,
    /// Raw `createdAt` from the stream, kept for logging; `None` offline.
    pub stream_created_at: Option<String>,
    pub viewers: Option<u32>,
    pub collaboration_viewers: Option<u32>,
    pub game: Option<Game>,
    pub live: bool,
}

// Typed mirror of the GQL responses. Decoding straight into these
// (instead of `serde_json::Value`) skips the generic map allocation and
// gives every field a concrete type; unknown fields are ignored. Every
// nullable layer stays `Option` so one missing user or null `data`
// collapses to `None` instead of failing the whole decode.
#[derive(Debug, Deserialize)]
struct GqlIdsResponse {
    data: Option<GqlIdsData>,
}

#[derive(Debug, Deserialize)]
struct GqlLoginResponse {
    data: Option<GqlLoginData>,
}
#[derive(Debug, Deserialize)]
struct GqlLoginData {
    #[serde(default)]
    user: Option<GqlUser>,
}

#[derive(Debug, Deserialize)]
struct GqlIdsData {
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
    #[serde(rename = "lastBroadcast")]
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
    #[serde(rename = "viewersCount")]
    viewers_count: Option<u32>,
    #[serde(rename = "collaborationViewersCount")]
    collaboration_viewers_count: Option<u32>,
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
    fn into_channel(self) -> Option<ResolvedChannel> {
        let broadcast = self.broadcast?;
        Some(ResolvedChannel {
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
            stream_created_at: self
                .stream
                .as_ref()
                .and_then(|stream| stream.created_at.clone()),
            game: broadcast.game.and_then(|game| {
                Some(Game {
                    id: game.id?,
                    name: game.name?,
                    display_name: game.display_name?,
                })
            }),
            collaboration_viewers: self
                .stream
                .as_ref()
                .and_then(|stream| stream.collaboration_viewers_count),
            viewers: self.stream.as_ref().and_then(|stream| stream.viewers_count),
            live: self.stream.is_some(),
        })
    }
}

/// Resolves one login. `Ok(None)` means the login does not resolve
/// (unknown login or unparseable user); `Err` is transport/decode failure.
pub async fn fetch_channel(login: &str) -> anyhow::Result<Option<ResolvedChannel>> {
    let client = client()?;
    log::info!(target: "gql", "request: login={login:?}");
    let response = compio::time::timeout(
        GQL_TIMEOUT,
        client
            .post(GQL_URL)?
            .header("Content-Type", "application/json")?
            .header("Client-ID", CLIENT_ID)?
            .body(serde_json::to_vec(&json!({
                "query": USER_BY_LOGIN_QUERY,
                "variables": {
                    "login": login
                },
                "operationName": "UserByLogin",
            }))?)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gql request timed out"))??;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("gql returned status {}", response.status()));
    }
    let body = compio::time::timeout(GQL_TIMEOUT, response.bytes())
        .await
        .map_err(|_| anyhow::anyhow!("gql body read timed out"))??;

    let response: GqlLoginResponse = serde_json::from_slice(&body)?;
    let user = response
        .data
        .and_then(|data| data.user)
        .and_then(GqlUser::into_channel)
        .filter(|user| user.channel_name.eq_ignore_ascii_case(login));

    if let Some(user) = &user {
        log::info!(
            target: "gql",
            "parsed user {}({}){}",
            user.channel_name,
            user.channel_id,
            if user.live { " live" } else { "" }
        );
    } else {
        log::info!(target: "gql", "no user resolved for login={login:?}");
    }
    Ok(user)
}

pub async fn fetch_channels(ids: &[u64]) -> anyhow::Result<Vec<ResolvedChannel>> {
    let client = client()?;
    log::info!(target: "gql", "request: ids={ids:?}");
    let response = compio::time::timeout(
        GQL_TIMEOUT,
        client
            .post(GQL_URL)?
            .header("Content-Type", "application/json")?
            .header("Client-ID", CLIENT_ID)?
            .body(serde_json::to_vec(&json!({
                "query": USERS_BY_IDS_QUERY,
                "variables": {
                    "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>()
                },
                "operationName": "UsersByIds",
            }))?)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gql request timed out"))??;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("gql returned status {}", response.status()));
    }
    let body = compio::time::timeout(GQL_TIMEOUT, response.bytes())
        .await
        .map_err(|_| anyhow::anyhow!("gql body read timed out"))??;

    let response: GqlIdsResponse = serde_json::from_slice(&body)?;
    let users = response
        .data
        .and_then(|data| data.users)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| raw.and_then(GqlUser::into_channel))
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

pub async fn fetch_file(url: &str, path: impl AsRef<Path>) -> anyhow::Result<()> {
    let response = compio::time::timeout(GQL_TIMEOUT, client()?.get(url)?.send())
        .await
        .map_err(|_| anyhow::anyhow!("image request timed out"))??;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "image returned status {}",
            response.status()
        ));
    }
    let path = path.as_ref();
    if let Err(error) = stream_to_file(response, path).await {
        compio::fs::remove_file(path).await.ok();
        return Err(error);
    }
    Ok(())
}

async fn stream_to_file(response: cyper::Response, path: &Path) -> anyhow::Result<()> {
    let mut file = File::create(path).await?;
    let mut stream = response.bytes_stream();
    let mut pos: u64 = 0;
    while let Some(result) = compio::time::timeout(GQL_TIMEOUT, stream.next())
        .await
        .map_err(|_| anyhow::anyhow!("image body read timed out"))?
    {
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

    use super::{GqlIdsResponse, GqlLoginResponse, GqlUser};

    fn decode_users(body: &[u8]) -> Vec<super::ResolvedChannel> {
        let response: GqlIdsResponse = serde_json::from_slice(body).unwrap();
        response
            .data
            .and_then(|data| data.users)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|raw| raw.and_then(GqlUser::into_channel))
            .collect()
    }

    fn decode_login(body: &[u8]) -> Option<super::ResolvedChannel> {
        let response: GqlLoginResponse = serde_json::from_slice(body).unwrap();
        response
            .data
            .and_then(|data| data.user)
            .and_then(GqlUser::into_channel)
    }

    #[test]
    fn typed_decode_keeps_live_user_with_game() {
        let body = br#"{"data": {"users": [
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "lastBroadcast": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z",
                    "viewersCount": 583, "collaborationViewersCount": null}}
            ]}}"#;
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
        assert_eq!(user.viewers, Some(583));
        assert_eq!(user.collaboration_viewers, None);
        assert!(user.live);
    }

    #[test]
    fn typed_decode_accepts_numeric_ids_and_offline_user() {
        let body = br#"{"data": {"users": [
                {"id": 456, "login": "bob", "displayName": "Bob",
                 "profileImageURL": "http://x/z.png",
                 "lastBroadcast": {"id": 777, "title": "", "game": null},
                 "stream": null}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        let user = &users[0];
        assert_eq!(user.channel_id, 456);
        assert_eq!(user.stream_id, 777);
        assert!(user.stream_title.is_none());
        assert!(user.stream_start.is_none());
        assert!(user.viewers.is_none());
        assert!(user.collaboration_viewers.is_none());
        assert!(user.game.is_none());
        assert!(!user.live);
    }

    #[test]
    fn typed_decode_skips_bad_entries_but_keeps_batch() {
        let body = br#"{"data": {"users": [
                null,
                {"id": "bad", "login": "ghost",
                 "lastBroadcast": {"id": "1", "title": "t", "game": null},
                 "stream": null},
                {"id": "789", "login": "nobra",
                 "lastBroadcast": null, "stream": null},
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "lastBroadcast": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z"}}
            ]}}"#;
        let users = decode_users(body);
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(users[0].channel_id, 123);
    }

    #[test]
    fn typed_decode_tolerates_null_data_and_users() {
        assert!(decode_users(br#"{"data": null}"#).is_empty());
        assert!(decode_users(br#"{"data": {}}"#).is_empty());
        assert!(decode_users(br#"{"data": {"users": null}}"#).is_empty());
    }

    #[test]
    fn login_decode_keeps_live_user_with_game() {
        let body = br#"{"data": {"user":
                {"id": "123", "login": "alice", "displayName": "Alice",
                 "profileImageURL": "http://x/y.png",
                 "lastBroadcast": {"id": "999", "title": "hi",
                    "game": {"id": "10", "name": "g", "displayName": "G"}},
                 "stream": {"id": "1", "createdAt": "2026-09-10T18:35:06Z",
                    "viewersCount": 145, "collaborationViewersCount": 889}}
            }}"#;
        let user = decode_login(body).expect("user should decode");
        assert_eq!(user.channel_id, 123);
        assert_eq!(user.channel_name, "alice");
        assert_eq!(user.stream_id, 999);
        assert_eq!(user.viewers, Some(145));
        assert_eq!(user.collaboration_viewers, Some(889));
        assert!(user.live);
    }

    #[test]
    fn login_decode_maps_unknown_or_bad_user_to_none() {
        assert!(decode_login(br#"{"data": {"user": null}}"#).is_none());
        assert!(decode_login(br#"{"data": null}"#).is_none());
        // Missing broadcast settings cannot build a `User`.
        assert!(
            decode_login(
                br#"{"data": {"user":
                    {"id": "123", "login": "alice", "displayName": "Alice",
                     "profileImageURL": "http://x/y.png",
                     "lastBroadcast": null, "stream": null}
                }}"#
            )
            .is_none()
        );
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
            let ((), result) = futures_util::join!(serve, crate::http::fetch_file(&url, &path));
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
            let ((), result) = futures_util::join!(serve, crate::http::fetch_file(&url, &path));
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
            let ((), result) = futures_util::join!(serve, crate::http::fetch_file(&url, &path));
            assert!(result.is_err(), "error status should fail");
        });

        assert!(!path.exists(), "no file should be created");
    }
}
