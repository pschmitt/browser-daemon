//! Session records: `<state>/sessions/<id>.json`, written while a session lives,
//! so a restarted daemon can adopt the browsers (and displays) that kept running
//! (see `proc`). A record belongs to one boot of the host: after a reboot the
//! processes are gone and the records are dropped.

use crate::browser::BrowserEngine;
use crate::profiles::ColorScheme;
use crate::ScreenMode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

/// Bumped when a record changes in a way an older daemon cannot read.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AudioRecord {
    pub sink: String,
    pub module: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayRecord {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub pid: u32,
    pub pid_start: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionRecord {
    pub version: u32,
    pub boot_id: String,
    pub id: Uuid,
    pub name: String,
    pub engine: BrowserEngine,
    pub persistent: bool,
    pub port: u16,
    pub ws_path: String,
    pub pid: u32,
    pub pid_start: u64,
    pub profile: Option<String>,
    pub profile_id: Option<Uuid>,
    /// User-data directory of a session without a profile.
    pub temp_dir: Option<PathBuf>,
    pub firefox_log: Option<PathBuf>,
    pub window_size: (u32, u32),
    pub adblock: bool,
    pub color_scheme: ColorScheme,
    pub screen: ScreenMode,
    pub show_touches: Option<bool>,
    pub idle_timeout_secs: u64,
    pub relay_port: u16,
    /// Proxy id the relay forwards to; none: direct.
    pub proxy: Option<String>,
    pub audio: Option<AudioRecord>,
    /// The session's own display (per-session mode).
    pub display: Option<DisplayRecord>,
    /// The shared display the browser runs on instead.
    pub shared_display: Option<String>,
    pub video: bool,
    /// Wall-clock start, for the uptime after an adoption.
    pub created_at_secs: u64,
    /// A recording was running: a new one continues it after the restart.
    pub recording: bool,
    /// ...with the sound (None: the node's default; records of older versions).
    #[serde(default)]
    pub recording_audio: Option<bool>,
}

impl SessionRecord {
    pub fn new_boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

fn file(state_dir: &Path, id: Uuid) -> PathBuf {
    dir(state_dir).join(format!("{id}.json"))
}

pub async fn save(state_dir: &Path, record: &SessionRecord) -> std::io::Result<()> {
    let dir = dir(state_dir);
    private_dir(&dir).await?;
    write_atomic(
        &file(state_dir, record.id),
        &serde_json::to_vec_pretty(record)?,
    )
    .await
}

/// Create `dir` (and missing parents) and make it reachable by the daemon's user only.
pub async fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).await?;
    fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await
}

/// Counter for temporary file names: two writers of one file never share one.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `path` replaced by `data` all at once (a temporary file in the same directory,
/// synced, renamed), readable by the daemon's user only. A crash leaves the old
/// content or the new, never half of it.
pub async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}-{seq}.tmp", std::process::id()));
    let result = async {
        let mut f = private_file(&tmp).await?;
        f.write_all(data).await?;
        f.sync_all().await?;
        drop(f);
        fs::rename(&tmp, path).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    result
}

/// A new (or truncated) file only the daemon's user can read.
pub async fn private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await
}

/// Like [`write_atomic`]'s file, but appended to.
pub async fn private_append(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await
}

pub async fn remove(state_dir: &Path, id: Uuid) {
    let _ = fs::remove_file(file(state_dir, id)).await;
}

/// The records this boot can still adopt; stale or unreadable ones are deleted.
pub async fn load_all(state_dir: &Path) -> Vec<SessionRecord> {
    let boot = SessionRecord::new_boot_id();
    let mut out = Vec::new();
    let Ok(mut entries) = fs::read_dir(dir(state_dir)).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tmp") {
            // A save the daemon died in the middle of.
            let _ = fs::remove_file(&path).await;
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let record = fs::read(&path)
            .await
            .ok()
            .and_then(|raw| serde_json::from_slice::<SessionRecord>(&raw).ok())
            .filter(|r| r.version == VERSION && r.boot_id == boot);
        match record {
            Some(r) => out.push(r),
            None => {
                let _ = fs::remove_file(&path).await;
            }
        }
    }
    out
}

pub fn record_version() -> u32 {
    VERSION
}
