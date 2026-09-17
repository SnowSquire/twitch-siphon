use std::io::Error as IoError;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single tracked channel. Only resolved channels are ever stored; a login
/// that fails to resolve lives for the session only and never hits the disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub login: String,
    pub id: u64,
    pub display_name: Option<String>,
}

/// On-disk config. `version` dispatches decoding: a file stamped with a newer
/// version than this build understands is discarded (fresh default) rather
/// than misinterpreted, while older files run one migration step per version
/// with their data preserved. Files without a version predate versioning and
/// decode as version 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub channels: Vec<Channel>,
    pub notify_title_changes: bool,
    pub sound: bool,
    pub filtered_words: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            channels: Vec::new(),
            notify_title_changes: true,
            sound: true,
            filtered_words: vec!["offline".into()],
        }
    }
}

impl Config {
    pub const VERSION: u32 = 2;

    /// Synchronous on purpose: the single call site is `main`, on the GUI
    /// thread before any runtime exists, where blocking is harmless. The
    /// runtime path ([`WorkState`](crate::state::WorkState)) only saves.
    /// Anything undecodable yields a fresh default.
    pub fn load(path: &Path) -> Self {
        match Self::load_versioned(path) {
            Ok(config) => config,
            Err(error) => {
                log::info!(target: "config", "starting with fresh config: {error}");
                Self::default()
            }
        }
    }

    /// Decodes the file by version dispatch: the stamp is read before any
    /// typed decoding, so shapes that no longer parse still migrate. Newer
    /// files are refused; older ones run one [`migrate_step`] per version
    /// until current, and only the result decodes into [`Config`].
    fn load_versioned(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let from = value.get("version").and_then(Value::as_u64).unwrap_or(0);
        if from > u64::from(Self::VERSION) {
            return Err(format!("ignoring config version {from}"));
        }
        let map = value
            .as_object_mut()
            .ok_or_else(|| "config root is not an object".to_owned())?;
        let mut version = from;
        while version < u64::from(Self::VERSION) {
            migrate_step(map, version)?;
            version += 1;
            map.insert("version".to_owned(), Value::from(version));
        }
        let config: Self = serde_json::from_value(Value::Object(map.clone()))
            .map_err(|error| error.to_string())?;
        if from < u64::from(Self::VERSION) {
            log::info!(
                target: "config",
                "migrated config version {from} to {}",
                Self::VERSION
            );
        }
        Ok(config)
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

/// Advances `map` from exactly `version` to `version + 1`, preserving
/// everything already present. Add one arm per new version; the loop in
/// [`Config::load_versioned`] applies them in order until current, so a file
/// any number of versions behind still converges. The coverage test below
/// calls every version in `0..VERSION`, so bumping `VERSION` without adding
/// its arm fails `cargo test`.
fn migrate_step(map: &mut Map<String, Value>, version: u64) -> Result<(), String> {
    match version {
        // v0 predates versioning but shares v1's shape: stamping (done by the
        // caller) is the whole migration.
        0 => Ok(()),
        // v1 predates word filtering: default it to on for "offline", unless
        // the file already says otherwise.
        1 => {
            map.entry("filteredWords")
                .or_insert_with(|| Value::from(vec!["offline"]));
            Ok(())
        }
        _ => {
            // Programmer error, not user data: the coverage test catches it,
            // and release builds still fall back to a fresh default below.
            debug_assert!(false, "missing migration from config version {version}");
            Err(format!("no migration from config version {version}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("siphon-config-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn load_migrates_older_versions_preserving_channels() {
        for (name, version) in [("v1", 1), ("unversioned", 0)] {
            let path = scratch_path(name);
            std::fs::write(
                &path,
                format!(r#"{{"version":{version},"channels":[{{"login":"alice","id":7}}]}}"#),
            )
            .unwrap();
            let config = Config::load(&path);
            assert_eq!(config.version, Config::VERSION, "{name}");
            assert_eq!(
                config
                    .channels
                    .iter()
                    .map(|channel| channel.login.as_str())
                    .collect::<Vec<_>>(),
                ["alice"],
                "{name}"
            );
            assert_eq!(config.filtered_words, ["offline"], "{name}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn load_discards_newer_version_for_fresh_default() {
        let path = scratch_path("newer");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"channels":[{{"login":"alice","id":7}}]}}"#,
                Config::VERSION + 1
            ),
        )
        .unwrap();
        let config = Config::load(&path);
        assert_eq!(config.version, Config::VERSION);
        assert!(config.channels.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn migrate_step_covers_every_version_below_current() {
        for version in 0..u64::from(Config::VERSION) {
            let mut map = Map::new();
            assert!(
                migrate_step(&mut map, version).is_ok(),
                "missing migration from config version {version}"
            );
        }
    }

    #[test]
    fn migrate_v1_keeps_existing_filtered_words() {
        let mut map = Map::new();
        map.insert("version".to_owned(), Value::from(1));
        map.insert("filteredWords".to_owned(), Value::from(vec!["sponsored"]));
        migrate_step(&mut map, 1).unwrap();
        assert_eq!(map["filteredWords"], Value::from(vec!["sponsored"]));
    }

    #[test]
    fn load_matching_version_keeps_channels() {
        let path = scratch_path("current");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"channels":[{{"login":"alice","id":7}}],"filteredWords":[]}}"#,
                Config::VERSION
            ),
        )
        .unwrap();
        let config = Config::load(&path);
        assert_eq!(config.channels.len(), 1);
        assert_eq!(config.channels[0].login, "alice");
        // An explicitly cleared filter list is data too: migration must not
        // repopulate it on a current-version file.
        assert!(config.filtered_words.is_empty());
        std::fs::remove_file(&path).ok();
    }
}
