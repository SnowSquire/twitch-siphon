use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub login: String,
    pub id: u64,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u64,
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
    pub const VERSION: u64 = 2;

    pub fn load(path: &Path) -> Self {
        match Self::load_versioned(path) {
            Ok(config) => config,
            Err(error) => {
                log::info!(target: "config", "starting with fresh config: {error}");
                Self::default()
            }
        }
    }

    fn load_versioned(path: &Path) -> anyhow::Result<Self> {
        let mut value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let from = value.get("version").and_then(Value::as_u64).unwrap_or(0);
        if from > Self::VERSION {
            return Err(anyhow::anyhow!("ignoring config version {from}"));
        }

        let map = value
            .as_object_mut()
            .context("config root is not an object")?;

        for version in from..Self::VERSION {
            migrate_step(map, version)?;
            map.insert("version".to_owned(), Value::from(version + 1));
        }

        let config: Self = serde_json::from_value(value)?;
        if from < Self::VERSION {
            log::info!(
                target: "config",
                "migrated config version {from} to {}",
                Self::VERSION
            );
        }
        Ok(config)
    }

    pub async fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(self)?;
        compio::fs::write(path, json).await.0?;
        Ok(())
    }
}

fn migrate_step(map: &mut Map<String, Value>, version: u64) -> anyhow::Result<()> {
    match version {
        // Unversioned files share v1's shape: stamping (done by the caller)
        // is the whole migration.
        0 => Ok(()),
        // Files without word filtering default it to on for "offline",
        // unless the file already says otherwise.
        1 => {
            map.entry("filteredWords")
                .or_insert_with(|| Value::from(vec!["offline"]));
            Ok(())
        }
        _ => {
            // Programmer error, not user data: the coverage test catches it,
            // and release builds still fall back to a fresh default below.
            debug_assert!(false, "missing migration from config version {version}");
            Err(anyhow::anyhow!(
                "no migration from config version {version}"
            ))
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
        for version in 0..Config::VERSION {
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
