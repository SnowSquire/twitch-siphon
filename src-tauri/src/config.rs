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

    pub fn save(&self, path: &Path) -> Result<(), IoError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(IoError::other)?;
        std::fs::write(path, bytes)
    }
}
