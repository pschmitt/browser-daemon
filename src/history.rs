//! Pages visited in a profile (kept across sessions) or in a throwaway session
//! (kept while the daemon runs), for the address bar's suggestions in the live view.
//!
//! The daemon notes the address of every tab it sees, whatever the engine; the
//! browsers' own history databases are not read.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Entries kept per profile or session: the least recently visited go first.
const MAX_ENTRIES: usize = 400;
const MAX_URL_LEN: usize = 2048;
/// Keys of throwaway sessions start with this and are never written to disk.
pub const SESSION_PREFIX: &str = "session:";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Times a tab arrived on the page.
    pub count: u32,
    /// Seconds since the epoch of the last visit.
    pub last: u64,
}

#[derive(Default)]
struct Inner {
    lists: HashMap<String, Vec<Entry>>,
    dirty: HashSet<String>,
}

pub struct History {
    dir: PathBuf,
    inner: Mutex<Inner>,
    /// Held while lists are written or removed: a flush that took its lists
    /// before a delete must not put the file back afterwards.
    io: tokio::sync::Mutex<()>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The address as it is remembered: http(s) only, without the fragment.
fn clean_url(url: &str) -> Option<String> {
    if url.len() > MAX_URL_LEN {
        return None;
    }
    let mut parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

impl History {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            inner: Mutex::default(),
            io: tokio::sync::Mutex::new(()),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn persistent(key: &str) -> bool {
        !key.starts_with(SESSION_PREFIX)
            && !key.is_empty()
            && key.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    }

    /// The list of `key`, read from disk the first time.
    fn list<'a>(&self, inner: &'a mut Inner, key: &str) -> &'a mut Vec<Entry> {
        inner.lists.entry(key.to_string()).or_insert_with(|| {
            if Self::persistent(key) {
                std::fs::read(self.path(key))
                    .ok()
                    .and_then(|raw| serde_json::from_slice(&raw).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        })
    }

    /// A tab is on `url` (with `title`, once the page has one).
    pub fn record(&self, key: &str, url: &str, title: Option<&str>, arrived: bool) {
        let Some(url) = clean_url(url) else { return };
        let title = title.map(str::trim).filter(|t| !t.is_empty() && *t != url);
        let mut inner = self.inner.lock().unwrap();
        let list = self.list(&mut inner, key);
        match list.iter_mut().find(|e| e.url == url) {
            Some(e) => {
                if arrived {
                    e.count = e.count.saturating_add(1);
                    e.last = now();
                }
                if let Some(t) = title {
                    if e.title.as_deref() != Some(t) {
                        e.title = Some(t.to_string());
                    }
                }
            }
            None => {
                list.push(Entry {
                    url,
                    title: title.map(str::to_string),
                    count: 1,
                    last: now(),
                });
                if list.len() > MAX_ENTRIES {
                    list.sort_by_key(|e| std::cmp::Reverse(e.last));
                    list.truncate(MAX_ENTRIES);
                }
            }
        }
        inner.dirty.insert(key.to_string());
    }

    /// Pages matching `query` (all when empty), the likeliest first: addresses whose
    /// host starts with it, then the rest of the address, then titles; within a
    /// group the more visited and the more recent.
    pub fn suggest(&self, key: &str, query: &str, limit: usize) -> Vec<Entry> {
        let q = query.trim().to_lowercase();
        let mut inner = self.inner.lock().unwrap();
        let list = self.list(&mut inner, key);
        let mut hits: Vec<(u8, &Entry)> = list
            .iter()
            .filter_map(|e| {
                if q.is_empty() {
                    return Some((0, e));
                }
                let url = e.url.to_lowercase();
                let host = url::Url::parse(&e.url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_lowercase))
                    .unwrap_or_default();
                let bare = url
                    .split_once("://")
                    .map_or(url.as_str(), |(_, rest)| rest)
                    .trim_start_matches("www.");
                if host.trim_start_matches("www.").starts_with(&q) || bare.starts_with(&q) {
                    Some((0, e))
                } else if url.contains(&q) {
                    Some((1, e))
                } else if e
                    .title
                    .as_ref()
                    .is_some_and(|t| t.to_lowercase().contains(&q))
                {
                    Some((2, e))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(b.1.count.cmp(&a.1.count))
                .then(b.1.last.cmp(&a.1.last))
        });
        hits.into_iter()
            .take(limit)
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// Drop a throwaway session's list.
    pub fn forget(&self, key: &str) {
        if !Self::persistent(key) {
            let mut inner = self.inner.lock().unwrap();
            inner.lists.remove(key);
            inner.dirty.remove(key);
        }
    }

    /// Write the lists of profiles that changed.
    pub async fn flush(&self) {
        let _io = self.io.lock().await;
        let pending: Vec<(String, Vec<Entry>)> = {
            let mut inner = self.inner.lock().unwrap();
            let keys: Vec<String> = inner
                .dirty
                .drain()
                .filter(|k| Self::persistent(k))
                .collect();
            keys.into_iter()
                .filter_map(|k| inner.lists.get(&k).cloned().map(|l| (k, l)))
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        let _ = crate::persist::private_dir(&self.dir).await;
        for (key, list) in pending {
            let written = match serde_json::to_vec(&list) {
                Ok(raw) => crate::persist::write_atomic(&self.path(&key), &raw)
                    .await
                    .is_ok(),
                Err(_) => false,
            };
            if !written {
                // Try again with the next flush.
                self.inner.lock().unwrap().dirty.insert(key);
            }
        }
    }

    /// Remove a deleted profile's list.
    pub async fn delete(&self, key: &str) {
        let _io = self.io.lock().await;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.lists.remove(key);
            inner.dirty.remove(key);
        }
        if Self::persistent(key) {
            let _ = tokio::fs::remove_file(self.path(key)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0b8a4a3e-0000-4000-8000-000000000001";

    #[test]
    fn only_web_pages_are_kept_without_their_fragment() {
        assert_eq!(
            clean_url("https://a.test/x?y=1#top").as_deref(),
            Some("https://a.test/x?y=1")
        );
        assert!(clean_url("about:blank").is_none());
        assert!(clean_url("chrome://settings").is_none());
        assert!(clean_url("data:text/html,hi").is_none());
    }

    #[test]
    fn suggestions_rank_hosts_before_paths_before_titles() {
        let h = History::new(std::env::temp_dir().join("bd-history-test-a"));
        h.record(
            KEY,
            "https://opsgenie.test/alert/list",
            Some("Alerts"),
            true,
        );
        h.record(
            KEY,
            "https://mail.test/inbox?from=opsgenie",
            Some("Inbox"),
            true,
        );
        h.record(KEY, "https://news.test/", Some("About opsgenie"), true);
        let hits: Vec<String> = h
            .suggest(KEY, "opsgenie", 10)
            .into_iter()
            .map(|e| e.url)
            .collect();
        assert_eq!(
            hits,
            [
                "https://opsgenie.test/alert/list",
                "https://mail.test/inbox?from=opsgenie",
                "https://news.test/",
            ]
        );
        assert!(h.suggest(KEY, "nothing-like-it", 10).is_empty());
    }

    #[test]
    fn visits_count_and_the_list_is_bounded() {
        let h = History::new(std::env::temp_dir().join("bd-history-test-b"));
        for _ in 0..3 {
            h.record(KEY, "https://a.test/", None, true);
        }
        h.record(KEY, "https://b.test/", None, true);
        assert_eq!(h.suggest(KEY, "", 1)[0].url, "https://a.test/");
        for i in 0..(MAX_ENTRIES + 20) {
            h.record(KEY, &format!("https://x.test/{i}"), None, true);
        }
        assert_eq!(h.suggest(KEY, "", 10_000).len(), MAX_ENTRIES);
    }

    #[tokio::test]
    async fn profiles_are_kept_on_disk_and_sessions_are_not() {
        let dir = std::env::temp_dir().join(format!("bd-history-test-c-{}", std::process::id()));
        let h = History::new(dir.clone());
        h.record(KEY, "https://a.test/", Some("A"), true);
        h.record("session:1234", "https://s.test/", None, true);
        h.flush().await;
        let again = History::new(dir.clone());
        assert_eq!(again.suggest(KEY, "", 5)[0].title.as_deref(), Some("A"));
        assert!(again.suggest("session:1234", "", 5).is_empty());
        assert!(!dir.join("session:1234.json").exists());
        // A deleted list stays deleted, and no temporary file is left behind.
        again.delete(KEY).await;
        again.flush().await;
        assert!(!dir.join(format!("{KEY}.json")).exists());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
