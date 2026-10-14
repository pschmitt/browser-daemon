//! Server-side profile backups: `<state>/backups/<profile id>/<ms>[-label].tar.gz`,
//! next to the profiles directory. A backup is a gzipped tar of the profile's
//! user-data directory, restorable in place.

use crate::profiles::{self, Profile};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use uuid::Uuid;

const EXT: &str = ".tar.gz";
const MAX_LABEL_CHARS: usize = 40;
/// Label prefix of backups the daemon takes itself before a destructive action.
pub const AUTO_PREFIX: &str = "auto-";
/// Automatic backups kept per profile; older ones are pruned.
const KEEP_AUTO: usize = 3;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub id: String,
    pub label: Option<String>,
    pub created_at_secs: u64,
    pub size_bytes: u64,
    pub automatic: bool,
}

fn backups_root(profiles_dir: &Path) -> PathBuf {
    profiles_dir
        .parent()
        .unwrap_or(profiles_dir)
        .join("backups")
}

fn dir_for(profiles_dir: &Path, profile_id: Uuid) -> PathBuf {
    backups_root(profiles_dir).join(profile_id.to_string())
}

/// Lowercase letters, digits and dashes only: labels end up in file names.
pub fn clean_label(label: &str) -> Option<String> {
    let mut out = String::new();
    for c in label.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out
        .trim_matches('-')
        .chars()
        .take(MAX_LABEL_CHARS)
        .collect();
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && id.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn parse(file_name: &str, size_bytes: u64) -> Option<BackupInfo> {
    let id = file_name.strip_suffix(EXT)?;
    if !valid_id(id) {
        return None;
    }
    let (ms, label) = match id.split_once('-') {
        Some((ms, label)) => (ms, Some(label.to_string())),
        None => (id, None),
    };
    let ms: u64 = ms.parse().ok()?;
    Some(BackupInfo {
        id: id.to_string(),
        automatic: label.as_deref().is_some_and(|l| l.starts_with(AUTO_PREFIX)),
        label,
        created_at_secs: ms / 1000,
        size_bytes,
    })
}

/// Newest first.
pub async fn list(profiles_dir: &Path, profile_id: Uuid) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    let Ok(mut entries) = fs::read_dir(dir_for(profiles_dir, profile_id)).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if let Some(info) = parse(&entry.file_name().to_string_lossy(), meta.len()) {
            out.push(info);
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

pub async fn get(profiles_dir: &Path, profile_id: Uuid, id: &str) -> Option<BackupInfo> {
    list(profiles_dir, profile_id)
        .await
        .into_iter()
        .find(|b| b.id == id)
}

fn path_of(profiles_dir: &Path, profile_id: Uuid, id: &str) -> PathBuf {
    dir_for(profiles_dir, profile_id).join(format!("{id}{EXT}"))
}

async fn run_tar(args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let out = tokio::process::Command::new("tar")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run tar: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Archive the profile's directory. The caller makes sure no browser uses it.
pub async fn create(
    profiles_dir: &Path,
    profile: &Profile,
    label: Option<&str>,
) -> Result<BackupInfo, String> {
    let dir = dir_for(profiles_dir, profile.meta.id);
    fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("failed to create backup directory: {e}"))?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let id = match label.and_then(clean_label) {
        Some(label) => format!("{ms}-{label}"),
        None => ms.to_string(),
    };
    let target = path_of(profiles_dir, profile.meta.id, &id);
    let tmp = dir.join(format!(".{id}.tmp"));
    let result = run_tar(&[
        "-czf".as_ref(),
        tmp.as_os_str(),
        "-C".as_ref(),
        profile.dir.as_os_str(),
        ".".as_ref(),
    ])
    .await;
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp).await;
        return Err(e);
    }
    fs::rename(&tmp, &target)
        .await
        .map_err(|e| format!("failed to store backup: {e}"))?;
    get(profiles_dir, profile.meta.id, &id)
        .await
        .ok_or_else(|| "backup vanished".to_string())
}

/// Drop all but the newest automatic backups.
pub async fn prune_auto(profiles_dir: &Path, profile_id: Uuid) {
    let autos = list(profiles_dir, profile_id)
        .await
        .into_iter()
        .filter(|b| b.automatic);
    for old in autos.skip(KEEP_AUTO) {
        let _ = fs::remove_file(path_of(profiles_dir, profile_id, &old.id)).await;
    }
}

pub async fn delete(profiles_dir: &Path, profile_id: Uuid, id: &str) -> bool {
    valid_id(id)
        && fs::remove_file(path_of(profiles_dir, profile_id, id))
            .await
            .is_ok()
}

/// All backups of a profile (it was deleted).
pub async fn remove_all(profiles_dir: &Path, profile_id: Uuid) {
    let _ = fs::remove_dir_all(dir_for(profiles_dir, profile_id)).await;
}

/// Replace the profile's data with a backup, keeping its id and metadata.
/// The old data goes away only once the backup is fully extracted.
pub async fn restore(profiles_dir: &Path, profile: &Profile, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err("invalid backup id".into());
    }
    let archive = path_of(profiles_dir, profile.meta.id, id);
    if fs::metadata(&archive).await.is_err() {
        return Err("backup not found".into());
    }
    let staging = profiles_dir.join(format!(".restore-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging)
        .await
        .map_err(|e| format!("failed to create staging directory: {e}"))?;
    let fail = |e: String, staging: PathBuf| async move {
        let _ = fs::remove_dir_all(&staging).await;
        Err(e)
    };
    if let Err(e) = run_tar(&[
        "-xzf".as_ref(),
        archive.as_os_str(),
        "-C".as_ref(),
        staging.as_os_str(),
    ])
    .await
    {
        return fail(e, staging).await;
    }
    if let Err(e) = profiles::write_meta(&staging, &profile.meta).await {
        return fail(format!("failed to write profile metadata: {e}"), staging).await;
    }
    let old = profiles_dir.join(format!(".old-{}", Uuid::new_v4()));
    if let Err(e) = fs::rename(&profile.dir, &old).await {
        return fail(format!("failed to move current data away: {e}"), staging).await;
    }
    if let Err(e) = fs::rename(&staging, &profile.dir).await {
        let _ = fs::rename(&old, &profile.dir).await;
        return fail(format!("failed to put the backup in place: {e}"), staging).await;
    }
    let _ = fs::remove_dir_all(&old).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_file_name_safe() {
        assert_eq!(
            clean_label("  Before the Update! "),
            Some("before-the-update".into())
        );
        assert_eq!(clean_label("../etc"), Some("etc".into()));
        assert_eq!(clean_label("---"), None);
    }

    #[test]
    fn backup_file_names_round_trip() {
        let b = parse("1792003110123-auto-pre-clear.tar.gz", 5).unwrap();
        assert_eq!(b.created_at_secs, 1792003110);
        assert_eq!(b.label.as_deref(), Some("auto-pre-clear"));
        assert!(b.automatic);
        let b = parse("1792003110123.tar.gz", 5).unwrap();
        assert_eq!(b.label, None);
        assert!(parse("x.tar.gz", 1).is_none());
        assert!(parse("1-../x.tar.gz", 1).is_none());
    }
}
