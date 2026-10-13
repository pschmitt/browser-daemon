//! Passkeys approved in a live view. The vault isn't on the node: a helper
//! running as the vault's user (`browser-daemon passkey-helper`) polls for
//! approved requests and injects the passkey into the page with
//! `rbw passkey inject`, unlocking rbw from the phone first if needed.
//!
//! The view doesn't wait on one long request (browsers and proxies drop those):
//! it starts a request and polls its status, which the helper may annotate
//! ("confirm the unlock on your phone") before finishing it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Request {
    pub id: Uuid,
    pub page_id: String,
    pub rp_id: String,
    /// "inject" (put the passkey into the page) or "find" (only say which vault
    /// entry would be used, or that the vault is locked).
    #[serde(default = "inject")]
    pub kind: String,
    /// The vault entry (id) to use, when the user picked one of several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

fn inject() -> String {
    "inject".into()
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// What a "find" found (no secrets): {locked} or {name, user, ...}.
    #[serde(default)]
    pub info: Option<serde_json::Value>,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Pending { note: Option<String> },
    Ready { info: Option<serde_json::Value> },
    Failed { error: String },
}

struct Tracked {
    status: Status,
    since: Instant,
}

#[derive(Default)]
pub struct Broker {
    queue: Mutex<VecDeque<Request>>,
    tracked: Mutex<HashMap<Uuid, Tracked>>,
    helper_seen: Mutex<Option<Instant>>,
    wake: Notify,
}

/// A helper polls at least this often while running.
const HELPER_GONE: Duration = Duration::from_secs(90);
/// Unlocking from the phone can take a while.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(330);

impl Broker {
    /// Queue a request for the helper; its progress is read with `status`.
    pub fn start(
        &self,
        page_id: String,
        rp_id: String,
        kind: &str,
        entry: Option<String>,
    ) -> Result<Uuid, String> {
        let helper = *self.helper_seen.lock().unwrap();
        if helper.is_none_or(|t| t.elapsed() > HELPER_GONE) {
            return Err("No passkey helper is running for this node".into());
        }
        let req = Request {
            id: Uuid::new_v4(),
            page_id,
            rp_id,
            kind: kind.into(),
            entry,
        };
        let mut tracked = self.tracked.lock().unwrap();
        tracked.retain(|_, t| t.since.elapsed() < 2 * ANSWER_TIMEOUT);
        tracked.insert(
            req.id,
            Tracked {
                status: Status::Pending { note: None },
                since: Instant::now(),
            },
        );
        let id = req.id;
        self.queue.lock().unwrap().push_back(req);
        self.wake.notify_waiters();
        Ok(id)
    }

    pub fn status(&self, id: Uuid) -> Option<Status> {
        let mut tracked = self.tracked.lock().unwrap();
        let t = tracked.get_mut(&id)?;
        if matches!(t.status, Status::Pending { .. }) && t.since.elapsed() > ANSWER_TIMEOUT {
            t.status = Status::Failed {
                error: "The passkey helper did not answer".into(),
            };
            self.queue.lock().unwrap().retain(|r| r.id != id);
        }
        Some(t.status.clone())
    }

    /// The helper's long poll: the next request, or None after `wait`. A helper
    /// that predates "find" requests (it would inject instead) gets none of them.
    pub async fn next(&self, wait: Duration, finds: bool) -> Option<Request> {
        let deadline = Instant::now() + wait;
        loop {
            *self.helper_seen.lock().unwrap() = Some(Instant::now());
            let woken = self.wake.notified();
            let popped = self.queue.lock().unwrap().pop_front();
            if let Some(req) = popped {
                if req.kind == "inject" || finds {
                    return Some(req);
                }
                self.complete(
                    req.id,
                    Outcome {
                        ok: false,
                        error: Some("The passkey helper is too old to look entries up".into()),
                        info: None,
                    },
                );
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, woken).await.is_err() {
                return None;
            }
        }
    }

    /// The helper tells the user something while the request is pending.
    pub fn note(&self, id: Uuid, note: String) -> bool {
        match self.tracked.lock().unwrap().get_mut(&id) {
            Some(Tracked {
                status: Status::Pending { note: n },
                ..
            }) => {
                *n = Some(note);
                true
            }
            _ => false,
        }
    }

    pub fn complete(&self, id: Uuid, outcome: Outcome) -> bool {
        match self.tracked.lock().unwrap().get_mut(&id) {
            Some(t) if matches!(t.status, Status::Pending { .. }) => {
                t.status = if outcome.ok {
                    Status::Ready { info: outcome.info }
                } else {
                    Status::Failed {
                        error: outcome.error.unwrap_or_else(|| "The helper failed".into()),
                    }
                };
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn request_reaches_helper_and_back() {
        let b = Arc::new(Broker::default());
        assert!(
            b.start("p".into(), "x.test".into(), "inject", None)
                .is_err(),
            "no helper yet"
        );
        assert!(b.next(Duration::from_millis(10), true).await.is_none());
        let id = b
            .start("p".into(), "x.test".into(), "inject", None)
            .unwrap();
        let req = b.next(Duration::from_secs(1), true).await.unwrap();
        assert_eq!(req.rp_id, "x.test");
        assert!(b.note(id, "unlock".into()));
        assert_eq!(
            b.status(id),
            Some(Status::Pending {
                note: Some("unlock".into())
            })
        );
        assert!(b.complete(
            id,
            Outcome {
                ok: true,
                error: None,
                info: None,
            }
        ));
        assert_eq!(b.status(id), Some(Status::Ready { info: None }));
    }
}
