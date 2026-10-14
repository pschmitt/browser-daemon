//! Persistent browser profiles: `<profiles_dir>/<uuid>/` is the browser's
//! user-data directory and holds a `profile.json` with the profile's name.
//! Names are free text and never touch the filesystem.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tracing::{info, warn};
use uuid::Uuid;

use crate::browser::BrowserEngine;

pub const META_FILE: &str = "profile.json";
const MAX_NAME_CHARS: usize = 128;
/// Directory prefix the name-based layout used for Firefox profiles.
const LEGACY_FIREFOX_PREFIX: &str = "firefox_";

/// Light or dark appearance pages see (prefers-color-scheme) and the browser's own UI.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProfileMeta {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub engine: BrowserEngine,
    pub created_at_secs: u64,
    /// Proxy id sessions of this profile use unless they ask for another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Appearance of the profile's sessions (dark when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<ColorScheme>,
    /// Page a session's start tab opens (blank when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub meta: ProfileMeta,
    pub dir: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A usable display name: trimmed, non-empty, bounded, no control characters.
pub fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control);
    ok.then(|| name.to_string())
}

pub async fn read_meta(dir: &Path) -> Option<ProfileMeta> {
    let raw = fs::read(dir.join(META_FILE)).await.ok()?;
    serde_json::from_slice(&raw).ok()
}

pub async fn write_meta(dir: &Path, meta: &ProfileMeta) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{META_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(meta)?).await?;
    fs::rename(&tmp, dir.join(META_FILE)).await
}

pub async fn list(profiles_dir: &Path) -> Vec<Profile> {
    let mut profiles = Vec::new();
    let Ok(mut entries) = fs::read_dir(profiles_dir).await else {
        return profiles;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let dir = entry.path();
        // Hidden directories are staging areas (restores), not profiles.
        if entry.file_name().to_string_lossy().starts_with('.')
            || !entry.file_type().await.is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        if let Some(meta) = read_meta(&dir).await {
            profiles.push(Profile { meta, dir });
        }
    }
    profiles.sort_by_key(|p| p.meta.name.to_lowercase());
    profiles
}

/// Look a profile up by its id or by its exact name.
pub async fn find(profiles_dir: &Path, key: &str) -> Option<Profile> {
    let key = key.trim();
    let id = Uuid::parse_str(key).ok();
    list(profiles_dir)
        .await
        .into_iter()
        .find(|p| Some(p.meta.id) == id || p.meta.name == key)
}

pub async fn name_taken(profiles_dir: &Path, name: &str, except: Option<Uuid>) -> bool {
    list(profiles_dir)
        .await
        .iter()
        .any(|p| p.meta.name == name && Some(p.meta.id) != except)
}

/// Allocate a new, empty profile directory with its metadata.
pub async fn create(
    profiles_dir: &Path,
    name: &str,
    engine: BrowserEngine,
) -> std::io::Result<Profile> {
    let meta = ProfileMeta {
        id: Uuid::new_v4(),
        name: name.to_string(),
        engine,
        created_at_secs: now_secs(),
        proxy: None,
        color_scheme: None,
        homepage: None,
    };
    let dir = profiles_dir.join(meta.id.to_string());
    fs::create_dir_all(&dir).await?;
    write_meta(&dir, &meta).await?;
    Ok(Profile { meta, dir })
}

/// Give a directory fresh metadata (after a copy or an import).
pub async fn adopt(dir: PathBuf, name: &str, engine: BrowserEngine) -> std::io::Result<Profile> {
    // A copied or imported profile keeps its proxy, appearance and homepage.
    let old = read_meta(&dir).await;
    let proxy = old.as_ref().and_then(|m| m.proxy.clone());
    let homepage = old.as_ref().and_then(|m| m.homepage.clone());
    let color_scheme = old.and_then(|m| m.color_scheme);
    let id = dir
        .file_name()
        .and_then(|n| Uuid::parse_str(&n.to_string_lossy()).ok())
        .unwrap_or_else(Uuid::new_v4);
    let meta = ProfileMeta {
        id,
        name: name.to_string(),
        engine,
        created_at_secs: now_secs(),
        proxy,
        color_scheme,
        homepage,
    };
    write_meta(&dir, &meta).await?;
    Ok(Profile { meta, dir })
}

/// Engine and name a directory imported without `profile.json` stands for,
/// following the old `firefox_<name>` convention.
pub fn legacy_identity(dir_name: &str) -> (String, BrowserEngine) {
    match dir_name.strip_prefix(LEGACY_FIREFOX_PREFIX) {
        Some(name) if !name.is_empty() => (name.to_string(), BrowserEngine::Firefox),
        _ => (dir_name.to_string(), BrowserEngine::Chromium),
    }
}

/// Move name-based profile directories to `<uuid>/` with a `profile.json`.
/// Runs at startup, before any session can hold a profile open.
pub async fn migrate(profiles_dir: &Path) {
    let Ok(mut entries) = fs::read_dir(profiles_dir).await else {
        return;
    };
    let mut legacy = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let dir = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden
            && entry.file_type().await.is_ok_and(|t| t.is_dir())
            && read_meta(&dir).await.is_none()
        {
            legacy.push(dir);
        }
    }
    for old in legacy {
        let dir_name = old
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let (name, engine) = legacy_identity(&dir_name);
        let created_at_secs = fs::metadata(&old)
            .await
            .ok()
            .and_then(|m| m.created().or_else(|_| m.modified()).ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or_else(now_secs, |d| d.as_secs());
        let id = Uuid::parse_str(&dir_name).unwrap_or_else(|_| Uuid::new_v4());
        let meta = ProfileMeta {
            id,
            name: name.clone(),
            engine,
            created_at_secs,
            proxy: None,
            color_scheme: None,
            homepage: None,
        };
        let new = profiles_dir.join(id.to_string());
        if new != old {
            if let Err(e) = fs::rename(&old, &new).await {
                warn!(profile = %name, error = %e, "Could not migrate profile directory");
                continue;
            }
        }
        match write_meta(&new, &meta).await {
            Ok(()) => {
                info!(profile = %name, id = %id, engine = %engine, "Migrated profile to id-based directory")
            }
            Err(e) => warn!(profile = %name, error = %e, "Could not write profile metadata"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_free_text_but_bounded() {
        assert_eq!(
            clean_name("  Google (p@pschmitt.dev) ").as_deref(),
            Some("Google (p@pschmitt.dev)")
        );
        assert_eq!(
            clean_name("../etc/passwd").as_deref(),
            Some("../etc/passwd")
        );
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name("a\nb"), None);
        assert_eq!(clean_name(&"x".repeat(129)), None);
    }

    #[test]
    fn legacy_firefox_directories_keep_their_engine() {
        assert_eq!(
            legacy_identity("firefox_work"),
            ("work".to_string(), BrowserEngine::Firefox)
        );
        assert_eq!(
            legacy_identity("hrworks"),
            ("hrworks".to_string(), BrowserEngine::Chromium)
        );
    }

    #[tokio::test]
    async fn migrate_moves_named_directories_to_ids() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("hrworks/Default"))
            .await
            .unwrap();
        fs::create_dir_all(root.path().join("firefox_work"))
            .await
            .unwrap();
        migrate(root.path()).await;
        migrate(root.path()).await;

        let profiles = list(root.path()).await;
        assert_eq!(profiles.len(), 2);
        let hr = find(root.path(), "hrworks").await.unwrap();
        assert_eq!(hr.meta.engine, BrowserEngine::Chromium);
        assert_eq!(hr.dir, root.path().join(hr.meta.id.to_string()));
        assert!(hr.dir.join("Default").is_dir());
        let ff = find(
            root.path(),
            &find(root.path(), "work").await.unwrap().meta.id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(ff.meta.engine, BrowserEngine::Firefox);
    }

    #[tokio::test]
    async fn create_rejects_nothing_but_lookup_is_exact() {
        let root = tempfile::tempdir().unwrap();
        let p = create(
            root.path(),
            "Google (p@pschmitt.dev)",
            BrowserEngine::Chromium,
        )
        .await
        .unwrap();
        assert!(find(root.path(), "Google (p@pschmitt.dev)").await.is_some());
        assert!(find(root.path(), "google (p@pschmitt.dev)").await.is_none());
        assert!(name_taken(root.path(), "Google (p@pschmitt.dev)", None).await);
        assert!(!name_taken(root.path(), "Google (p@pschmitt.dev)", Some(p.meta.id)).await);
    }
}
