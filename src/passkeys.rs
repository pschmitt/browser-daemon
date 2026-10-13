//! Passkeys approved in a live view. The vault isn't on the node: a helper
//! running as the vault's user (`browser-daemon passkey-helper`) polls for
//! approved requests and injects the passkey into the page with
//! `rbw passkey inject`, unlocking rbw from the phone first if needed.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Notify};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Request {
    pub id: Uuid,
    pub page_id: String,
    pub rp_id: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Broker {
    queue: Mutex<VecDeque<Request>>,
    waiting: Mutex<HashMap<Uuid, oneshot::Sender<Outcome>>>,
    helper_seen: Mutex<Option<Instant>>,
    wake: Notify,
}

/// A helper polls at least this often while running.
const HELPER_GONE: Duration = Duration::from_secs(90);
/// Unlocking from the phone can take a while.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(330);

impl Broker {
    /// Hand a request to the helper and wait until the passkey is in place.
    pub async fn submit(&self, page_id: String, rp_id: String) -> Result<(), String> {
        let helper = *self.helper_seen.lock().unwrap();
        if helper.is_none_or(|t| t.elapsed() > HELPER_GONE) {
            return Err("No passkey helper is running for this node".into());
        }
        let req = Request {
            id: Uuid::new_v4(),
            page_id,
            rp_id,
        };
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(req.id, tx);
        self.queue.lock().unwrap().push_back(req.clone());
        self.wake.notify_waiters();
        let outcome = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
        self.waiting.lock().unwrap().remove(&req.id);
        self.queue.lock().unwrap().retain(|r| r.id != req.id);
        match outcome {
            Ok(Ok(Outcome { ok: true, .. })) => Ok(()),
            Ok(Ok(Outcome { error, .. })) => {
                Err(error.unwrap_or_else(|| "The helper failed".into()))
            }
            _ => Err("The passkey helper did not answer".into()),
        }
    }

    /// The helper's long poll: the next request, or None after `wait`.
    pub async fn next(&self, wait: Duration) -> Option<Request> {
        let deadline = Instant::now() + wait;
        loop {
            *self.helper_seen.lock().unwrap() = Some(Instant::now());
            let woken = self.wake.notified();
            if let Some(req) = self.queue.lock().unwrap().pop_front() {
                return Some(req);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, woken).await.is_err() {
                return None;
            }
        }
    }

    pub fn complete(&self, id: Uuid, outcome: Outcome) -> bool {
        match self.waiting.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
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
            b.submit("p".into(), "x.test".into()).await.is_err(),
            "no helper yet"
        );
        assert!(b.next(Duration::from_millis(10)).await.is_none());
        let helper = {
            let b = b.clone();
            tokio::spawn(async move {
                let req = b.next(Duration::from_secs(5)).await.unwrap();
                assert_eq!(req.rp_id, "x.test");
                b.complete(
                    req.id,
                    Outcome {
                        ok: true,
                        error: None,
                    },
                )
            })
        };
        assert_eq!(b.submit("p".into(), "x.test".into()).await, Ok(()));
        assert!(helper.await.unwrap());
    }
}
