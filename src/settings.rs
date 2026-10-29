//! Settings of this node, changed from the dashboard (Nodes tab) and kept across
//! restarts in a JSON file next to the profiles.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::browser::BrowserEngine;
use crate::profiles::ColorScheme;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSettings {
    /// Proxy id for sessions that choose none themselves and whose profile has
    /// none either; direct when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_proxy: Option<String>,
    /// Defaults per browser engine (Engines tab).
    #[serde(default)]
    pub engines: Engines,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Engines {
    #[serde(default)]
    pub chromium: EngineDefaults,
    #[serde(default)]
    pub firefox: EngineDefaults,
}

impl Engines {
    pub fn of(&self, engine: BrowserEngine) -> &EngineDefaults {
        match engine {
            BrowserEngine::Chromium => &self.chromium,
            BrowserEngine::Firefox => &self.firefox,
        }
    }
}

/// What a new session of an engine starts with, unless the request or its
/// profile says otherwise.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineDefaults {
    /// Ad blocker; the node's --adblock-default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adblock: Option<bool>,
    /// Dark when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<ColorScheme>,
    /// Live views mark taps and clicks (a viewer's own choice wins).
    #[serde(default)]
    pub show_touches: bool,
}

pub struct Store {
    path: PathBuf,
    current: RwLock<NodeSettings>,
    /// One change at a time: read, modify and save are a unit.
    write: tokio::sync::Mutex<()>,
    tmp_counter: AtomicU64,
}

/// Why `Store::update` changed nothing.
#[derive(Debug)]
pub enum UpdateError<E> {
    /// The change itself said no.
    Rejected(E),
    /// The settings could not be saved.
    Io(std::io::Error),
}

impl Store {
    /// Load settings, using defaults only when the file does not exist.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let current = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NodeSettings::default(),
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", path.display()),
                ))
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            current: RwLock::new(current),
            write: tokio::sync::Mutex::new(()),
            tmp_counter: AtomicU64::new(0),
        })
    }

    pub fn get(&self) -> NodeSettings {
        self.current.read().unwrap().clone()
    }

    /// Replace the settings, on disk first (written aside, then renamed).
    #[cfg(test)]
    pub async fn set(&self, settings: NodeSettings) -> std::io::Result<()> {
        let _guard = self.write.lock().await;
        self.save(settings).await
    }

    /// Change the settings: `change` sees the current ones and edits them; the
    /// result is saved before it is taken over. Changes run one after the other,
    /// so none is lost to a concurrent one.
    pub async fn update<E>(
        &self,
        change: impl FnOnce(&mut NodeSettings) -> Result<(), E>,
    ) -> Result<NodeSettings, UpdateError<E>> {
        let _guard = self.write.lock().await;
        let mut settings = self.get();
        change(&mut settings).map_err(UpdateError::Rejected)?;
        self.save(settings.clone()).await.map_err(UpdateError::Io)?;
        Ok(settings)
    }

    /// The caller holds `write`.
    async fn save(&self, settings: NodeSettings) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        // A name of its own: a leftover of a crashed save never gets in the way.
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}.{n}", std::process::id()));
        let written = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            file.write_all(&serde_json::to_vec_pretty(&settings)?)
                .await?;
            // On disk before the rename makes it the settings.
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&tmp, &self.path).await
        }
        .await;
        if written.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        written?;
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
        let store = Store::load(&path).unwrap();
        assert_eq!(store.get(), NodeSettings::default());
        let mut s = NodeSettings {
            default_proxy: Some("tor".into()),
            ..Default::default()
        };
        s.engines.firefox.show_touches = true;
        s.engines.chromium.color_scheme = Some(ColorScheme::Light);
        store.set(s.clone()).await.unwrap();
        assert_eq!(Store::load(&path).unwrap().get(), s);
        std::fs::write(&path, "not json").unwrap();
        assert!(Store::load(&path).is_err());
        assert!(Store::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_updates_are_all_kept() {
        let dir = std::env::temp_dir().join(format!("bd-settings-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node-settings.json");
        let store = std::sync::Arc::new(Store::load(&path).unwrap());
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .update(|s| {
                            s.default_proxy.get_or_insert_with(String::new).push('x');
                            Ok::<_, ()>(())
                        })
                        .await
                        .unwrap();
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            store.get().default_proxy.as_deref(),
            Some("xxxxxxxxxxxxxxxx")
        );
        assert_eq!(Store::load(&path).unwrap().get(), store.get());
        // A rejected change leaves everything as it was, and no temp file behind.
        let err = store.update(|_| Err::<(), _>("no")).await;
        assert!(matches!(err, Err(UpdateError::Rejected("no"))));
        assert_eq!(
            store.get().default_proxy.as_deref(),
            Some("xxxxxxxxxxxxxxxx")
        );
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
