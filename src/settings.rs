//! Settings of this node, changed from the dashboard (Nodes tab) and kept across
//! restarts in a JSON file next to the profiles.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSettings {
    /// Proxy id for sessions that choose none themselves and whose profile has
    /// none either; direct when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_proxy: Option<String>,
}

pub struct Store {
    path: PathBuf,
    current: RwLock<NodeSettings>,
}

impl Store {
    /// The settings in `path`; defaults when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let current = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Ignoring unreadable node settings");
                NodeSettings::default()
            }),
            Err(_) => NodeSettings::default(),
        };
        Self {
            path: path.to_path_buf(),
            current: RwLock::new(current),
        }
    }

    pub fn get(&self) -> NodeSettings {
        self.current.read().unwrap().clone()
    }

    /// Replace the settings, on disk first (written aside, then renamed).
    pub async fn set(&self, settings: NodeSettings) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(&settings)?).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        *self.current.write().unwrap() = settings;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kept_across_loads() {
        let dir = std::env::temp_dir().join(format!("bd-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node-settings.json");
        let store = Store::load(&path);
        assert_eq!(store.get(), NodeSettings::default());
        let s = NodeSettings {
            default_proxy: Some("tor".into()),
        };
        store.set(s.clone()).await.unwrap();
        assert_eq!(Store::load(&path).get(), s);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Store::load(&path).get(), NodeSettings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
