//! What the passkey helper knows about the vault's logins without asking rbw
//! each time: names, users, folders, URIs and whether a password or TOTP is
//! there, never the secrets themselves. A lookup through rbw takes seconds
//! (`rbw search` decrypts every entry's names and URIs, and rbw-agent serves one
//! `rbw get` at a time), the index answers at once.
//!
//! It is built in the background from `rbw list --raw` and built again when
//! rbw's local database changes (rbw writes `~/.cache/rbw/*.json` when it
//! syncs). Until it's there, lookups go through rbw as before.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime};

use serde_json::Value;
use tracing::{info, warn};

use crate::secrets::{self, Uri};

#[derive(Clone)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub user: Option<String>,
    pub folder: Option<String>,
    pub uris: Vec<Uri>,
    pub password: bool,
    pub totp: bool,
    /// Name, user, folder and URIs, lowercase, for searches.
    haystack: String,
}

impl Entry {
    fn from_raw(raw: &Value) -> Option<Self> {
        let data = &raw["data"];
        let has = |f: &str| data[f].as_str().is_some_and(|v| !v.is_empty());
        if !has("username") && !has("password") {
            return None; // not a login
        }
        let s = |v: &Value| v.as_str().filter(|v| !v.is_empty()).map(str::to_string);
        let uris = secrets::uris(raw);
        let (name, user, folder) = (
            s(&raw["name"]).unwrap_or_default(),
            s(&data["username"]),
            s(&raw["folder"]),
        );
        let haystack = [
            name.as_str(),
            user.as_deref().unwrap_or_default(),
            folder.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .chain(uris.iter().map(|u| u.uri.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
        Some(Self {
            id: s(&raw["id"])?,
            name,
            user,
            folder,
            uris,
            password: has("password"),
            totp: has("totp"),
            haystack,
        })
    }

    /// Every word of `query` is in its name, user, folder or URIs.
    pub fn matches(&self, query: &str) -> bool {
        query
            .to_lowercase()
            .split_whitespace()
            .all(|w| self.haystack.contains(w))
    }
}

struct Index {
    /// The database's version it was built from (see `db_version`).
    version: SystemTime,
    entries: Vec<Entry>,
}

static INDEX: LazyLock<Mutex<Option<Index>>> = LazyLock::new(Mutex::default);
static BUILDING: AtomicBool = AtomicBool::new(false);

/// rbw's local databases (one per account), whose newest change dates the vault.
fn cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("rbw"))
}

fn db_version() -> Option<SystemTime> {
    std::fs::read_dir(cache_dir()?)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

/// The vault's logins when the index is up to date; otherwise None, and a new
/// one is built in the background (rbw must be unlocked).
pub fn entries(rbw: &Path) -> Option<Vec<Entry>> {
    let version = db_version();
    {
        let index = INDEX.lock().unwrap();
        if let (Some(i), Some(v)) = (index.as_ref(), version) {
            if i.version == v {
                return Some(i.entries.clone());
            }
        }
    }
    refresh(rbw);
    None
}

/// Builds the index in the background, unless that's underway.
pub fn refresh(rbw: &Path) {
    if BUILDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let rbw = rbw.to_path_buf();
    tokio::spawn(async move {
        let started = Instant::now();
        // Dated before the listing: a sync during it makes the next lookup build again.
        let version = db_version();
        match secrets::rbw(&rbw, &["list", "--raw"]).await {
            Ok(out) => match serde_json::from_str::<Vec<Value>>(&out) {
                Ok(raw) => {
                    let entries: Vec<Entry> = raw.iter().filter_map(Entry::from_raw).collect();
                    info!(
                        logins = entries.len(),
                        took_ms = started.elapsed().as_millis() as u64,
                        "Vault index built"
                    );
                    if let Some(version) = version {
                        *INDEX.lock().unwrap() = Some(Index { version, entries });
                    }
                }
                Err(_) => warn!("Vault index: unexpected rbw output"),
            },
            Err(e) => warn!(error = %e, "Vault index not built"),
        }
        BUILDING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logins_only_and_no_secrets() {
        let login = json!({ "id": "a", "name": "GitHub", "folder": "dev", "notes": "n",
            "data": { "username": "me@x.test", "password": "hunter22", "totp": "otpauth://x",
                      "uris": [{ "uri": "https://github.com/login", "match_type": null }] } });
        let note = json!({ "id": "b", "name": "Note", "data": { "type": "note" } });
        let e = Entry::from_raw(&login).unwrap();
        assert!(e.password && e.totp);
        assert!(!e.haystack.contains("hunter22") && !e.haystack.contains("otpauth"));
        assert!(e.matches("git ME@x") && e.matches("github.com dev"));
        assert!(!e.matches("gitlab"));
        assert!(Entry::from_raw(&note).is_none());
    }
}
