use std::io::Error as IoError;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single tracked channel. Only resolved channels are ever stored; a login
/// that fails to resolve lives for the session only and never hits the disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub login: String,
    pub id: u64,
    pub display_name: Option<String>,
}

/// On-disk config. `version` gates future migrations: a file stamped with a
/// newer version than this build understands is discarded (fresh default)
/// rather than misinterpreted. Files without a version predate versioning
/// and load as version 0, which is stamped to current on the next save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub channels: Vec<Channel>,
    pub notify_title_changes: bool,
    pub sound: bool,
}

impl Config {
    pub const VERSION: u32 = 1;

    /// Synchronous on purpose: the single call site is `main`, on the GUI
    /// thread before any runtime exists, where blocking is harmless. The
    /// runtime path ([`WorkState`](crate::state::WorkState)) only saves.
    pub fn load(path: &Path) -> Self {
        let config: Self = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if config.version > Self::VERSION {
            Self::default()
        } else {
            config
        }
    }

    /// Async so the work-thread runtime (thread-per-core) never blocks on
    /// disk: one chunked write straight from the serialized string, with no
    /// intermediate buffer beyond it.
    pub async fn save(&self, path: &Path) -> Result<(), IoError> {
        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(self).map_err(IoError::other)?;
        compio::fs::write(path, json).await.0
    }
}
