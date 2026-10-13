//! Live views of one session in Native layout share a size: the smallest
//! width and height among them, so each viewer sees the whole page instead of
//! fighting the others over the window.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

struct Viewer {
    /// Screen size while in Native layout; None in the other layouts.
    native: Option<(u32, u32)>,
    tx: UnboundedSender<String>,
}

#[derive(Default)]
pub struct Viewers {
    next: AtomicU64,
    sessions: Mutex<HashMap<Uuid, HashMap<u64, Viewer>>>,
}

impl Viewers {
    /// A new viewer of `session`; it receives the shared size on `rx`.
    pub fn join(&self, session: Uuid) -> (u64, UnboundedReceiver<String>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = unbounded_channel();
        let mut sessions = self.sessions.lock().unwrap();
        let viewers = sessions.entry(session).or_default();
        viewers.insert(id, Viewer { native: None, tx });
        broadcast(viewers);
        (id, rx)
    }

    pub fn set(&self, session: Uuid, id: u64, native: Option<(u32, u32)>) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(viewers) = sessions.get_mut(&session) else {
            return;
        };
        match viewers.get_mut(&id) {
            Some(v) if v.native != native => v.native = native,
            _ => return,
        }
        broadcast(viewers);
    }

    pub fn leave(&self, session: Uuid, id: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(viewers) = sessions.get_mut(&session) else {
            return;
        };
        viewers.remove(&id);
        if viewers.is_empty() {
            sessions.remove(&session);
        } else {
            broadcast(viewers);
        }
    }
}

/// Fits every screen: the smallest width and the smallest height, each on its own.
fn smallest(sizes: impl Iterator<Item = (u32, u32)>) -> Option<(u32, u32)> {
    sizes.reduce(|(w, h), (w2, h2)| (w.min(w2), h.min(h2)))
}

fn broadcast(viewers: &HashMap<u64, Viewer>) {
    let size = smallest(viewers.values().filter_map(|v| v.native));
    let msg = serde_json::json!({ "native": size, "viewers": viewers.len() }).to_string();
    for v in viewers.values() {
        let _ = v.tx.send(msg.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smallest_fits_every_screen() {
        assert_eq!(
            smallest([(1920, 1000), (1280, 1300)].into_iter()),
            Some((1280, 1000))
        );
        assert_eq!(smallest(std::iter::empty()), None);
    }

    #[test]
    fn size_follows_joins_and_leaves() {
        let v = Viewers::default();
        let s = Uuid::new_v4();
        let (a, mut ra) = v.join(s);
        let (b, _rb) = v.join(s);
        v.set(s, a, Some((1920, 1080)));
        v.set(s, b, Some((800, 1200)));
        let last = |r: &mut UnboundedReceiver<String>| {
            let mut m = None;
            while let Ok(x) = r.try_recv() {
                m = Some(x);
            }
            serde_json::from_str::<serde_json::Value>(&m.unwrap()).unwrap()
        };
        assert_eq!(last(&mut ra)["native"], serde_json::json!([800, 1080]));
        v.leave(s, b);
        let after = last(&mut ra);
        assert_eq!(after["native"], serde_json::json!([1920, 1080]));
        assert_eq!(after["viewers"], 1);
    }
}
