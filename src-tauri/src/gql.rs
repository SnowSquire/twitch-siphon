use std::error::Error as StdError;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};

use crate::logging::{log, parse_iso_ms};

const GQL_URL: &str = "https://gql.twitch.tv/gql";
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_TIMEOUT: Duration = Duration::from_secs(15);

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

// Built once and shared across all gql requests so connections are pooled.
// hyper's legacy Client is internally immutable and its connection pool is
// already Send + Sync, so a plain Arc (no Mutex) is enough.
static GQL_CLIENT: OnceLock<Arc<HttpsClient>> = OnceLock::new();

fn client() -> Arc<HttpsClient> {
    GQL_CLIENT
        .get_or_init(|| {
            Arc::new(
                Client::builder(TokioExecutor::new()).build(
                    HttpsConnectorBuilder::new()
                        .with_webpki_roots()
                        .https_or_http()
                        .enable_http1()
                        .build(),
                ),
            )
        })
        .clone()
}

// The full user record: ids and start times are stored for future use
// (history/dedup) even though notifications only read a subset.
const USERS_BY_IDS_QUERY: &str = "query UsersByIds($ids:[ID!]){users(ids:$ids){id login displayName profileImageURL(width:70) broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";
const USERS_BY_LOGINS_QUERY: &str = "query UsersByLogins($logins:[String!]){users(logins:$logins){id login displayName profileImageURL(width:70) broadcastSettings{id title game{id name displayName}}stream{id createdAt}}}";

pub type Error = Box<dyn StdError + Send + Sync>;

// Mirrors the original hermesnotifier.ts `StreamInfo`: every field the TS
// client tracked is kept here even though only a subset is read.
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
    // millisseconds since 1970, technically should be in sync with live but we don't get them at the same time so  they are separate
    pub stream_start: Option<i64>,
    pub game: Option<Game>,
    pub live: bool,
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
    log("gql", format!("request: ids={ids:?} logins={logins:?}"));

    let payload = serde_json::to_vec(&Value::Array(operations))?;
    let client = client();
    let request = Request::builder()
        .method(Method::POST)
        .uri(GQL_URL)
        .header(CONTENT_TYPE, "application/json")
        .header("Client-ID", CLIENT_ID)
        .body(Full::new(Bytes::from(payload)))?;
    let response = tokio::time::timeout(GQL_TIMEOUT, client.request(request))
        .await
        .map_err(|_| "gql request timed out")??;
    if !response.status().is_success() {
        return Err(format!("gql returned status {}", response.status()).into());
    }
    let body = tokio::time::timeout(GQL_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| "gql body read timed out")??
        .to_bytes();
    let response: Value = serde_json::from_slice(&body)?;

    let Some(responses) = response.as_array() else {
        log("gql", "unexpected response shape, expected an array");
        return Ok(Vec::new());
    };
    let users = responses
        .iter()
        .flat_map(|response| {
            response["data"]["users"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
        })
        .filter_map(|raw| {
            let channel_id = raw["id"].as_str()?.parse().ok()?;
            let broadcast = &raw["broadcastSettings"];
            Some(User {
                channel_id,
                channel_name: raw["login"].as_str().unwrap_or_default().into(),
                channel_display_name: raw["displayName"].as_str().unwrap_or_default().into(),
                profile_image_url: raw["profileImageURL"].as_str().unwrap_or_default().into(),
                stream_id: broadcast["id"].as_str()?.parse().ok()?,
                stream_title: broadcast["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .map(Into::into),
                stream_start: raw["stream"]["createdAt"].as_str().and_then(parse_iso_ms),
                game: broadcast["game"].as_object().and_then(|game| {
                    Some(Game {
                        id: game["id"].as_str()?.parse().ok()?,
                        name: game["name"].as_str()?.into(),
                        display_name: game["displayName"].as_str()?.into(),
                    })
                }),
                live: raw["stream"].is_object(),
            })
        })
        .collect::<Vec<_>>();
    log(
        "gql",
        format!(
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
        ),
    );
    Ok(users)
}

/// downloads an arbitrary cdn file (e.g. a profile picture) to `path`
pub async fn fetch_file(url: &str, path: &Path) -> Result<(), Error> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .body(Full::new(Bytes::new()))?;
    let response = tokio::time::timeout(GQL_TIMEOUT, client().request(request))
        .await
        .map_err(|_| "image request timed out")??;
    if !response.status().is_success() {
        return Err(format!("image returned status {}", response.status()).into());
    }
    let bytes = tokio::time::timeout(GQL_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| "image body read timed out")??
        .to_bytes();
    Ok(std::fs::write(path, &bytes)?)
}
