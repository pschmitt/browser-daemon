//! WebDriver BiDi multiplexer for Firefox.
//!
//! Firefox allows a single WebDriver session per process ("Maximum number of
//! active sessions"), so the daemon owns that session and fans it out: every
//! client on `/firefox` (automation, the handoff page, the recorder) talks to
//! the hub as if it had its own session. Command ids are remapped per client,
//! `session.new`/`session.end`/`session.subscribe`/`session.unsubscribe` are
//! answered locally and events are delivered only to clients that subscribed.

use axum::extract::ws::{Message as ClientMsg, WebSocket as ClientWs};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message as ServerMsg;
use tracing::{debug, info, warn};
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long a command may take before it is given up on.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// For reads that feed overviews and polling: a hung tab must not stall them.
const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages queued for one client (or for the browser) before it counts as stuck.
const QUEUE: usize = 1024;
/// Realm the daemon's own page probes run in, so a page's scripts cannot
/// replace what they read (a hostile `document.title` getter).
const PROBE_SANDBOX: &str = "browser-daemon";

enum Route {
    Client { client: u64, id: Value },
    Internal(oneshot::Sender<Value>),
}

struct Subscription {
    id: String,
    events: Vec<String>,
    contexts: Option<Vec<String>>,
}

impl Subscription {
    fn matches(&self, method: &str, context: Option<&str>) -> bool {
        let event_ok = self.events.iter().any(|e| {
            e == method
                || method
                    .strip_prefix(e.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        });
        if !event_ok {
            return false;
        }
        match (&self.contexts, context) {
            (Some(ctxs), Some(ctx)) => ctxs.iter().any(|c| c == ctx),
            _ => true,
        }
    }
}

struct Client {
    tx: mpsc::Sender<String>,
    subs: Vec<Subscription>,
}

/// How many subscriptions use each event, so the browser streams an event only
/// as long as somebody wants it.
#[derive(Default)]
struct EventRefs(HashMap<String, usize>);

impl EventRefs {
    /// Count one more use of each event; the events nobody used before.
    fn acquire(&mut self, events: &[String]) -> Vec<String> {
        let mut fresh = Vec::new();
        for e in events {
            let n = self.0.entry(e.clone()).or_default();
            if *n == 0 {
                fresh.push(e.clone());
            }
            *n += 1;
        }
        fresh
    }

    /// Count one use of each event less; the events nobody uses any more.
    fn release(&mut self, events: &[String]) -> Vec<String> {
        let mut unused = Vec::new();
        for e in events {
            let Some(n) = self.0.get_mut(e) else {
                continue;
            };
            *n -= 1;
            if *n == 0 {
                self.0.remove(e);
                unused.push(e.clone());
            }
        }
        unused
    }
}

/// Hand a message to a client; false when its queue is full (it is stuck).
fn queue(clients: &HashMap<u64, Client>, client: u64, text: String) -> bool {
    clients
        .get(&client)
        .is_none_or(|c| c.tx.try_send(text).is_ok())
}

/// Removes a command's pending entry however its wait ends, a timeout and a
/// cancelled caller included.
struct PendingGuard<'a> {
    pending: &'a StdMutex<HashMap<u64, Route>>,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BidiContext {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
}

pub struct BidiHub {
    backend_tx: mpsc::Sender<String>,
    next_id: AtomicU64,
    next_client: AtomicU64,
    pending: StdMutex<HashMap<u64, Route>>,
    clients: StdMutex<HashMap<u64, Client>>,
    backend_events: StdMutex<EventRefs>,
    closed: AtomicBool,
    /// Most recently navigated/activated top-level context (used by the recorder).
    last_active: Mutex<Option<String>>,
    pub session_id: String,
    pub capabilities: Value,
}

impl BidiHub {
    /// End the WebDriver session: Firefox allows one, and it stays taken after
    /// the socket closes, so a daemon that is exiting must give it back.
    /// Ask Firefox to quit the way a user's quit does: it writes its storage and
    /// session first (a signal makes it leave without flushing localStorage).
    pub async fn close_browser(&self) {
        let _ = self
            .call_timeout("browser.close", json!({}), Duration::from_secs(3))
            .await;
    }

    pub async fn end(&self) {
        let _ = self
            .call_timeout("session.end", json!({}), Duration::from_secs(3))
            .await;
    }

    pub async fn connect(port: u16) -> Result<Arc<Self>, BoxError> {
        let url = format!("ws://127.0.0.1:{}/session", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let (mut write, mut read) = ws.split();

        let new_cmd = json!({
            "id": 0,
            "method": "session.new",
            "params": { "capabilities": { "alwaysMatch": {
                "acceptInsecureCerts": true,
                "unhandledPromptBehavior": { "default": "ignore" }
            }}}
        });
        write
            .send(ServerMsg::Text(new_cmd.to_string().into()))
            .await?;

        // Firefox answers late while the host is busy (a loaded node needs well over 10s).
        let result = loop {
            let msg = tokio::time::timeout(Duration::from_secs(30), read.next())
                .await
                .map_err(|_| "Timed out waiting for session.new")?
                .ok_or("BiDi socket closed during session.new")??;
            if let ServerMsg::Text(text) = msg {
                let v: Value = serde_json::from_str(text.as_str())?;
                if v.get("id").and_then(Value::as_u64) == Some(0) {
                    if v.get("type").and_then(Value::as_str) != Some("success") {
                        return Err(format!("session.new failed: {}", v).into());
                    }
                    break v["result"].clone();
                }
            }
        };

        let (backend_tx, mut backend_rx) = mpsc::channel::<String>(QUEUE);
        let hub = Arc::new(Self {
            backend_tx,
            next_id: AtomicU64::new(1),
            next_client: AtomicU64::new(1),
            pending: StdMutex::new(HashMap::new()),
            clients: StdMutex::new(HashMap::new()),
            backend_events: StdMutex::new(EventRefs::default()),
            closed: AtomicBool::new(false),
            last_active: Mutex::new(None),
            session_id: result["sessionId"].as_str().unwrap_or_default().to_string(),
            capabilities: result["capabilities"].clone(),
        });

        tokio::spawn(async move {
            while let Some(text) = backend_rx.recv().await {
                if write.send(ServerMsg::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        let reader = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                match msg {
                    ServerMsg::Text(text) => reader.on_backend_message(text.as_str()).await,
                    ServerMsg::Close(_) => break,
                    _ => {}
                }
            }
            reader.shutdown().await;
        });

        // The hub tracks navigation itself to know which tab is "current".
        hub.ensure_backend_events(&[
            "browsingContext.contextCreated".to_string(),
            "browsingContext.navigationStarted".to_string(),
            "browsingContext.load".to_string(),
        ])
        .await;

        info!(session = %hub.session_id, port, "Firefox BiDi hub connected");
        Ok(hub)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the senders closes every client connection.
        self.clients.lock().unwrap().clear();
        let pending: Vec<Route> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, r)| r)
            .collect();
        for route in pending {
            if let Route::Internal(tx) = route {
                let _ = tx.send(json!({ "type": "error", "error": "unknown error", "message": "browser closed" }));
            }
        }
    }

    async fn on_backend_message(&self, text: &str) {
        let Ok(mut v) = serde_json::from_str::<Value>(text) else {
            return;
        };

        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            let route = self.pending.lock().unwrap().remove(&id);
            match route {
                Some(Route::Client { client, id: orig }) => {
                    v["id"] = orig;
                    self.deliver(client, v.to_string()).await;
                }
                Some(Route::Internal(tx)) => {
                    let _ = tx.send(v);
                }
                None => {}
            }
            return;
        }

        if v.get("type").and_then(Value::as_str) != Some("event") {
            return;
        }
        let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = &v["params"];
        let context = params
            .get("context")
            .or_else(|| params.pointer("/source/context"))
            .and_then(Value::as_str);

        if matches!(
            method,
            "browsingContext.navigationStarted"
                | "browsingContext.load"
                | "browsingContext.contextCreated"
        ) && params.get("parent").is_none_or(Value::is_null)
        {
            if let Some(ctx) = context {
                *self.last_active.lock().await = Some(ctx.to_string());
            }
        }

        let unused = {
            let mut clients = self.clients.lock().unwrap();
            let stuck: Vec<u64> = clients
                .iter()
                .filter(|(_, c)| {
                    c.subs.iter().any(|s| s.matches(method, context))
                        && c.tx.try_send(text.to_string()).is_err()
                })
                .map(|(id, _)| *id)
                .collect();
            stuck
                .into_iter()
                .flat_map(|id| self.drop_stuck(&mut clients, id))
                .collect::<Vec<_>>()
        };
        self.unsubscribe_backend(&unused).await;
    }

    /// Queue a message for a client; one that does not keep up is cut off
    /// instead of letting memory grow.
    async fn deliver(&self, client: u64, text: String) {
        let unused = {
            let mut clients = self.clients.lock().unwrap();
            if queue(&clients, client, text) {
                return;
            }
            self.drop_stuck(&mut clients, client)
        };
        self.unsubscribe_backend(&unused).await;
    }

    /// Cut a client off (dropping its sender ends its connection, see
    /// `serve_client`); the events nobody wants any more as a result.
    fn drop_stuck(&self, clients: &mut HashMap<u64, Client>, client: u64) -> Vec<String> {
        let Some(c) = clients.remove(&client) else {
            return Vec::new();
        };
        warn!(client, "BiDi client does not keep up; disconnecting it");
        let events: Vec<String> = c.subs.into_iter().flat_map(|s| s.events).collect();
        self.backend_events.lock().unwrap().release(&events)
    }

    /// Subscribe the browser to events (once, however many want them).
    async fn ensure_backend_events(&self, events: &[String]) {
        let fresh = self.backend_events.lock().unwrap().acquire(events);
        if fresh.is_empty() {
            return;
        }
        if let Err(e) = self
            .call_timeout(
                "session.subscribe",
                json!({ "events": fresh }),
                QUICK_TIMEOUT,
            )
            .await
        {
            warn!(error = %e, "BiDi hub failed to subscribe to backend events");
            // Not subscribed: the next one to ask tries again.
            self.backend_events.lock().unwrap().release(&fresh);
        }
    }

    /// Let go of events a client no longer wants; the browser stops sending the
    /// ones nobody wants.
    async fn release_backend_events(&self, events: &[String]) {
        let unused = self.backend_events.lock().unwrap().release(events);
        self.unsubscribe_backend(&unused).await;
    }

    async fn unsubscribe_backend(&self, unused: &[String]) {
        if unused.is_empty() || self.is_closed() {
            return;
        }
        if let Err(e) = self
            .call_timeout(
                "session.unsubscribe",
                json!({ "events": unused }),
                QUICK_TIMEOUT,
            )
            .await
        {
            debug!(error = %e, "BiDi hub could not unsubscribe from backend events");
        }
    }

    fn send_backend(&self, id: u64, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "id": id, "method": method, "params": params });
        self.backend_tx
            .try_send(msg.to_string())
            .map_err(|_| format!("{method}: the browser does not keep up"))
    }

    /// Issue a command on the shared session and wait for its result.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_timeout(method, params, CALL_TIMEOUT).await
    }

    /// Like `call`, given up on after `limit`. The command's entry is removed
    /// however the wait ends, also when the caller stops waiting early.
    pub async fn call_timeout(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        if self.is_closed() {
            return Err("browser closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, Route::Internal(tx));
        let _entry = PendingGuard {
            pending: &self.pending,
            id,
        };
        self.send_backend(id, method, params)?;
        let res = match tokio::time::timeout(limit, rx).await {
            Ok(Ok(v)) => v,
            _ => return Err(format!("{} timed out", method)),
        };
        if res.get("type").and_then(Value::as_str) == Some("success") {
            Ok(res["result"].clone())
        } else {
            Err(res
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| res.get("error").and_then(Value::as_str))
                .unwrap_or("unknown error")
                .to_string())
        }
    }

    /// Top-level browsing contexts (tabs) with their titles.
    pub async fn contexts(&self) -> Vec<BidiContext> {
        let Ok(tree) = self
            .call_timeout(
                "browsingContext.getTree",
                json!({ "maxDepth": 0 }),
                QUICK_TIMEOUT,
            )
            .await
        else {
            return Vec::new();
        };
        let tabs = tree["contexts"].as_array().cloned().unwrap_or_default();
        // Together: a tab that never answers costs one wait, not one per tab.
        futures_util::future::join_all(tabs.into_iter().map(|c| async move {
            let id = c["context"].as_str().unwrap_or_default().to_string();
            let url = c["url"].as_str().unwrap_or_default().to_string();
            let title = self
                .call_timeout(
                    "script.evaluate",
                    json!({
                        "expression": "document.title",
                        "target": { "context": id, "sandbox": PROBE_SANDBOX },
                        "awaitPromise": false
                    }),
                    Duration::from_millis(800),
                )
                .await
                .ok()
                .and_then(|r| {
                    r.pointer("/result/value")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|t| !t.is_empty());
            BidiContext { id, url, title }
        }))
        .await
    }

    pub async fn create_tab(&self, url: &str) -> Result<BidiContext, String> {
        let res = self
            .call("browsingContext.create", json!({ "type": "tab" }))
            .await?;
        let id = res["context"].as_str().unwrap_or_default().to_string();
        if url != "about:blank" {
            self.call(
                "browsingContext.navigate",
                json!({ "context": id, "url": url, "wait": "none" }),
            )
            .await?;
        }
        *self.last_active.lock().await = Some(id.clone());
        Ok(BidiContext {
            id,
            url: url.to_string(),
            title: None,
        })
    }

    pub async fn close_tab(&self, id: &str) -> Result<(), String> {
        self.call("browsingContext.close", json!({ "context": id }))
            .await
            .map(|_| ())
    }

    pub async fn activate_tab(&self, id: &str) -> Result<(), String> {
        self.call("browsingContext.activate", json!({ "context": id }))
            .await?;
        *self.last_active.lock().await = Some(id.to_string());
        Ok(())
    }

    /// The tab worth recording: the last one navigated, else the visible one.
    pub async fn active_context(&self) -> Option<String> {
        let last = self.last_active.lock().await.clone();
        let tree = self
            .call_timeout(
                "browsingContext.getTree",
                json!({ "maxDepth": 0 }),
                QUICK_TIMEOUT,
            )
            .await
            .ok()?;
        let ids: Vec<String> = tree["contexts"]
            .as_array()?
            .iter()
            .filter_map(|c| c["context"].as_str().map(str::to_string))
            .collect();
        if let Some(l) = last.filter(|l| ids.contains(l)) {
            return Some(l);
        }
        // Nothing navigated since this hub started (a daemon restart adopted the
        // browser): the foreground tab is the one that is visible.
        for id in &ids {
            let visible = self
                .call_timeout(
                    "script.evaluate",
                    json!({
                        "expression": "document.visibilityState === 'visible'",
                        "target": { "context": id, "sandbox": PROBE_SANDBOX },
                        "awaitPromise": false
                    }),
                    Duration::from_millis(800),
                )
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_bool())
                .unwrap_or(false);
            if visible {
                return Some(id.clone());
            }
        }
        ids.into_iter().next()
    }

    pub async fn screenshot_jpeg(&self, context: &str) -> Result<String, String> {
        let res = self
            .call_timeout(
                "browsingContext.captureScreenshot",
                json!({
                    "context": context,
                    "origin": "viewport",
                    "format": { "type": "image/jpeg", "quality": 0.6 }
                }),
                Duration::from_secs(10),
            )
            .await?;
        res["data"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "no screenshot data".into())
    }

    async fn handle_client_command(&self, client: u64, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            self.reply(client, json!({ "type": "error", "id": null, "error": "invalid argument", "message": "invalid JSON" })).await;
            return;
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let method = v
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));

        match method.as_str() {
            "session.new" => {
                let result =
                    json!({ "sessionId": self.session_id, "capabilities": self.capabilities });
                self.reply(
                    client,
                    json!({ "type": "success", "id": id, "result": result }),
                )
                .await;
            }
            "session.end" => {
                self.reply(client, json!({ "type": "success", "id": id, "result": {} }))
                    .await;
            }
            "session.subscribe" => {
                let events: Vec<String> = params["events"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let contexts: Option<Vec<String>> =
                    params.get("contexts").and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(|c| c.as_str().map(str::to_string))
                            .collect()
                    });
                let mut events = events;
                events.dedup();
                self.ensure_backend_events(&events).await;
                let sub_id = Uuid::new_v4().to_string();
                let attached = {
                    let mut clients = self.clients.lock().unwrap();
                    clients.get_mut(&client).map(|c| {
                        c.subs.push(Subscription {
                            id: sub_id.clone(),
                            events: events.clone(),
                            contexts,
                        });
                    })
                };
                if attached.is_none() {
                    // The client left while the browser was being asked.
                    self.release_backend_events(&events).await;
                }
                self.reply(
                    client,
                    json!({ "type": "success", "id": id, "result": { "subscription": sub_id } }),
                )
                .await;
            }
            "session.unsubscribe" => {
                let ids: Option<HashSet<String>> = params
                    .get("subscriptions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    });
                let events: Vec<String> = params["events"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut dropped = Vec::new();
                if let Some(c) = self.clients.lock().unwrap().get_mut(&client) {
                    if let Some(ids) = ids {
                        for s in &mut c.subs {
                            if ids.contains(&s.id) {
                                dropped.append(&mut s.events);
                            }
                        }
                    } else {
                        for s in &mut c.subs {
                            let (gone, kept): (Vec<_>, Vec<_>) =
                                s.events.drain(..).partition(|e| events.contains(e));
                            dropped.extend(gone);
                            s.events = kept;
                        }
                    }
                    c.subs.retain(|s| !s.events.is_empty());
                }
                self.release_backend_events(&dropped).await;
                self.reply(client, json!({ "type": "success", "id": id, "result": {} }))
                    .await;
            }
            _ => {
                if self.is_closed() {
                    self.reply(client, json!({ "type": "error", "id": id, "error": "invalid session id", "message": "browser closed" })).await;
                    return;
                }
                if matches!(
                    method.as_str(),
                    "browsingContext.activate" | "browsingContext.navigate"
                ) {
                    if let Some(ctx) = params.get("context").and_then(Value::as_str) {
                        *self.last_active.lock().await = Some(ctx.to_string());
                    }
                }
                let backend_id = self.next_id.fetch_add(1, Ordering::SeqCst);
                self.pending.lock().unwrap().insert(
                    backend_id,
                    Route::Client {
                        client,
                        id: id.clone(),
                    },
                );
                if let Err(e) = self.send_backend(backend_id, &method, params) {
                    self.pending.lock().unwrap().remove(&backend_id);
                    self.reply(client, json!({ "type": "error", "id": id, "error": "unknown error", "message": e })).await;
                }
            }
        }
    }

    async fn reply(&self, client: u64, msg: Value) {
        self.deliver(client, msg.to_string()).await;
    }

    /// Serve one `/firefox` WebSocket client until either side closes.
    pub async fn serve_client(
        self: Arc<Self>,
        socket: ClientWs,
        activity: Arc<crate::idle::Activity>,
        observer: bool,
    ) {
        let client = self.next_client.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel::<String>(QUEUE);
        self.clients.lock().unwrap().insert(
            client,
            Client {
                tx,
                subs: Vec::new(),
            },
        );
        debug!(client, "BiDi client attached");

        let (mut sink, mut stream) = socket.split();
        let writer = async {
            while let Some(text) = rx.recv().await {
                if sink.send(ClientMsg::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        };
        let reader = async {
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    ClientMsg::Text(text) => {
                        activity.note_command(text.as_str(), observer);
                        self.handle_client_command(client, text.as_str()).await
                    }
                    ClientMsg::Close(_) => break,
                    _ => {}
                }
            }
        };
        tokio::select! {
            _ = writer => {},
            _ = reader => {},
        }

        let left = self.clients.lock().unwrap().remove(&client);
        self.pending
            .lock()
            .unwrap()
            .retain(|_, r| !matches!(r, Route::Client { client: c, .. } if *c == client));
        // Its subscriptions end with it: the browser stops streaming what nobody reads.
        if let Some(c) = left {
            let events: Vec<String> = c.subs.into_iter().flat_map(|s| s.events).collect();
            self.release_backend_events(&events).await;
        }
        debug!(client, "BiDi client detached");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_streamed_while_somebody_wants_them() {
        let mut refs = EventRefs::default();
        let net = vec!["network".to_string()];
        assert_eq!(refs.acquire(&net), net);
        assert!(refs.acquire(&net).is_empty());
        assert!(refs.release(&net).is_empty());
        assert_eq!(refs.release(&net), net);
        // Nothing left to release.
        assert!(refs.release(&net).is_empty());
        assert_eq!(refs.acquire(&net), net);
    }

    type Log = Arc<StdMutex<Vec<(String, Value)>>>;

    /// A stand-in for Firefox's WebDriver BiDi endpoint: answers everything but
    /// `never`, and records the commands it got.
    async fn fake_firefox() -> (u16, Log) {
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let seen = log.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = ws.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                let method = v["method"].as_str().unwrap_or_default().to_string();
                seen.lock()
                    .unwrap()
                    .push((method.clone(), v["params"].clone()));
                if method == "never" {
                    continue;
                }
                let result = if method == "session.new" {
                    json!({ "sessionId": "s", "capabilities": {} })
                } else {
                    json!({})
                };
                let reply = json!({ "type": "success", "id": v["id"], "result": result });
                write
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        (port, log)
    }

    fn calls(log: &Log, method: &str) -> Vec<Value> {
        let log = log.lock().unwrap();
        log.iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    #[tokio::test]
    async fn an_abandoned_command_leaves_nothing_pending() {
        let (port, _log) = fake_firefox().await;
        let hub = BidiHub::connect(port).await.unwrap();
        // Its own timeout, and a caller that stops waiting earlier.
        let own = hub
            .call_timeout("never", json!({}), Duration::from_millis(50))
            .await;
        assert!(own.unwrap_err().contains("timed out"));
        let outer = tokio::time::timeout(
            Duration::from_millis(50),
            hub.call_timeout("never", json!({}), Duration::from_secs(30)),
        )
        .await;
        assert!(outer.is_err());
        assert!(hub.pending.lock().unwrap().is_empty());
        // Still working.
        assert!(hub.call("browsingContext.getTree", json!({})).await.is_ok());
    }

    fn attach(hub: &BidiHub, client: u64) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(QUEUE);
        hub.clients.lock().unwrap().insert(
            client,
            Client {
                tx,
                subs: Vec::new(),
            },
        );
        rx
    }

    fn subscribe_to(events: &[&str]) -> String {
        json!({ "id": 1, "method": "session.subscribe", "params": { "events": events } })
            .to_string()
    }

    #[tokio::test]
    async fn the_browser_streams_events_only_while_a_client_wants_them() {
        let (port, log) = fake_firefox().await;
        let hub = BidiHub::connect(port).await.unwrap();
        let mut a = attach(&hub, 1);
        let mut b = attach(&hub, 2);
        hub.handle_client_command(1, &subscribe_to(&["network"]))
            .await;
        hub.handle_client_command(2, &subscribe_to(&["network"]))
            .await;
        let ours = |log: &Log, method: &str| {
            calls(log, method)
                .into_iter()
                .filter(|p| p["events"] == json!(["network"]))
                .count()
        };
        assert_eq!(ours(&log, "session.subscribe"), 1, "subscribed once");
        let sub = |m: &mut mpsc::Receiver<String>| {
            let reply: Value = serde_json::from_str(&m.try_recv().unwrap()).unwrap();
            reply["result"]["subscription"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let (sub_a, sub_b) = (sub(&mut a), sub(&mut b));
        let unsubscribe = |id: &str| {
            json!({ "id": 2, "method": "session.unsubscribe", "params": { "subscriptions": [id] } })
                .to_string()
        };
        hub.handle_client_command(1, &unsubscribe(&sub_a)).await;
        assert_eq!(
            ours(&log, "session.unsubscribe"),
            0,
            "another still wants it"
        );
        hub.handle_client_command(2, &unsubscribe(&sub_b)).await;
        assert_eq!(ours(&log, "session.unsubscribe"), 1);
        // The hub's own navigation events stay subscribed.
        assert!(hub
            .backend_events
            .lock()
            .unwrap()
            .0
            .contains_key("browsingContext.load"));
    }

    #[tokio::test]
    async fn a_client_that_leaves_gives_its_events_back() {
        let (port, log) = fake_firefox().await;
        let hub = BidiHub::connect(port).await.unwrap();
        let _rx = attach(&hub, 7);
        hub.handle_client_command(7, &subscribe_to(&["log", "network"]))
            .await;
        // What `serve_client` does when the socket ends.
        let left = hub.clients.lock().unwrap().remove(&7).unwrap();
        let events: Vec<String> = left.subs.into_iter().flat_map(|s| s.events).collect();
        hub.release_backend_events(&events).await;
        let unsubscribed: Vec<Value> = calls(&log, "session.unsubscribe");
        assert_eq!(unsubscribed.len(), 1);
        assert_eq!(unsubscribed[0]["events"], json!(["log", "network"]));
    }

    #[tokio::test]
    async fn a_client_that_does_not_read_is_cut_off() {
        let (port, _log) = fake_firefox().await;
        let hub = BidiHub::connect(port).await.unwrap();
        let _rx = attach(&hub, 3);
        for _ in 0..=QUEUE {
            hub.deliver(3, "x".into()).await;
        }
        assert!(!hub.clients.lock().unwrap().contains_key(&3));
    }
}
