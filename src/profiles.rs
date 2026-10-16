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
    /// Pages a session of the profile opens with (its default tabs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<String>,
    /// Record the profile's sessions (the node's default when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    /// Their recordings take the sound too (the node's default when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_audio: Option<bool>,
    /// The single start page of earlier versions: folded into `tabs` when read.
    #[serde(default, rename = "homepage", skip_serializing)]
    legacy_homepage: Option<String>,
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
    let mut meta: ProfileMeta = serde_json::from_slice(&raw).ok()?;
    if let Some(page) = meta.legacy_homepage.take() {
        if meta.tabs.is_empty() {
            meta.tabs.push(page);
        }
    }
    Some(meta)
}

pub async fn write_meta(dir: &Path, meta: &ProfileMeta) -> std::io::Result<()> {
    crate::persist::write_atomic(&dir.join(META_FILE), &serde_json::to_vec_pretty(meta)?).await
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
        tabs: Vec::new(),
        record: None,
        record_audio: None,
        legacy_homepage: None,
    };
    let dir = profiles_dir.join(meta.id.to_string());
    crate::persist::private_dir(profiles_dir).await?;
    crate::persist::private_dir(&dir).await?;
    write_meta(&dir, &meta).await?;
    Ok(Profile { meta, dir })
}

/// Give a directory fresh metadata (after a copy or an import).
pub async fn adopt(dir: PathBuf, name: &str, engine: BrowserEngine) -> std::io::Result<Profile> {
    // A copied or imported profile keeps its proxy, appearance, default tabs and
    // recording settings.
    // An imported or restored directory holds logins: the archive's mode is not trusted.
    crate::persist::private_dir(&dir).await?;
    let old = read_meta(&dir).await;
    let (record, record_audio) = old
        .as_ref()
        .map_or((None, None), |m| (m.record, m.record_audio));
    let proxy = old.as_ref().and_then(|m| m.proxy.clone());
    let tabs = old.as_ref().map(|m| m.tabs.clone()).unwrap_or_default();
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
        tabs,
        record,
        record_audio,
        legacy_homepage: None,
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

/// Finish or undo what a crash left in a restore: `.restore-*` is a half-extracted
/// backup, `.old-*` the data it was replacing. Data whose profile has no directory
/// any more (the crash came between the two renames) goes back to its place.
pub async fn recover_interrupted(profiles_dir: &Path) {
    let Ok(mut entries) = fs::read_dir(profiles_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name.starts_with(".restore-") {
            warn!(dir = %name, "Removing an unfinished restore");
            let _ = fs::remove_dir_all(&path).await;
        } else if name.starts_with(".old-") {
            let Some(meta) = read_meta(&path).await else {
                warn!(dir = %name, "Data left by an interrupted restore has no profile.json: kept");
                continue;
            };
            let home = profiles_dir.join(meta.id.to_string());
            if fs::metadata(&home).await.is_err() {
                warn!(dir = %name, profile = %meta.name, "Putting a profile back after an interrupted restore");
                if let Err(e) = fs::rename(&path, &home).await {
                    warn!(dir = %name, error = %e, "Could not put the profile back");
                }
            } else {
                let _ = fs::remove_dir_all(&path).await;
            }
        }
    }
}

/// Move name-based profile directories to `<uuid>/` with a `profile.json`.
/// Runs at startup, before any session can hold a profile open.
pub async fn migrate(profiles_dir: &Path) {
    recover_interrupted(profiles_dir).await;
    crate::backups::sweep(profiles_dir).await;
    if fs::metadata(profiles_dir).await.is_ok() {
        let _ = crate::persist::private_dir(profiles_dir).await;
    }
    let Ok(mut entries) = fs::read_dir(profiles_dir).await else {
        return;
    };
    let mut legacy = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let dir = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden || !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        // Only a directory without a profile.json is a legacy one: an unreadable
        // file is somebody's metadata, not to be replaced by a guess.
        match fs::try_exists(dir.join(META_FILE)).await {
            Ok(false) => legacy.push(dir),
            Ok(true) if read_meta(&dir).await.is_none() => {
                warn!(dir = %dir.display(), "profile.json is unreadable: the profile is left alone");
            }
            _ => {}
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
            tabs: Vec::new(),
            record: None,
            record_audio: None,
            legacy_homepage: None,
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

    #[tokio::test]
    async fn an_old_homepage_becomes_the_only_default_tab() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        std::fs::write(
            root.path().join(META_FILE),
            format!(
                r#"{{"id":"{id}","name":"work","engine":"chromium","created_at_secs":1,"homepage":"https://a.test/"}}"#
            ),
        )
        .unwrap();
        let meta = read_meta(root.path()).await.unwrap();
        assert_eq!(meta.tabs, vec!["https://a.test/".to_string()]);
        // Written back as tabs only.
        write_meta(root.path(), &meta).await.unwrap();
        let raw = std::fs::read_to_string(root.path().join(META_FILE)).unwrap();
        assert!(
            raw.contains("\"tabs\"") && !raw.contains("homepage"),
            "{raw}"
        );
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

    #[tokio::test]
    async fn an_unreadable_profile_json_is_not_overwritten_by_the_migration() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let dir = root.path().join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(META_FILE), b"{ torn").unwrap();
        migrate(root.path()).await;
        assert_eq!(std::fs::read(dir.join(META_FILE)).unwrap(), b"{ torn");
    }

    #[tokio::test]
    async fn an_interrupted_restore_is_undone() {
        let root = tempfile::tempdir().unwrap();
        let p = create(root.path(), "work", BrowserEngine::Chromium)
            .await
            .unwrap();
        // Crash between the two renames: the data is under .old-*, the profile's directory is gone.
        let old = root.path().join(".old-1");
        std::fs::rename(&p.dir, &old).unwrap();
        std::fs::create_dir_all(root.path().join(".restore-2/Default")).unwrap();
        // Another restore got as far as removing the old data's copy.
        let done = create(root.path(), "other", BrowserEngine::Chromium)
            .await
            .unwrap();
        std::fs::create_dir_all(root.path().join(".old-3")).unwrap();
        std::fs::copy(
            done.dir.join(META_FILE),
            root.path().join(".old-3").join(META_FILE),
        )
        .unwrap();
        recover_interrupted(root.path()).await;
        assert!(p.dir.join(META_FILE).exists());
        assert!(!old.exists());
        assert!(!root.path().join(".restore-2").exists());
        assert!(!root.path().join(".old-3").exists());
        assert_eq!(list(root.path()).await.len(), 2);
    }
}
