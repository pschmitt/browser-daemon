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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message as ServerMsg;
use tracing::{debug, info, warn};
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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
    tx: mpsc::UnboundedSender<String>,
    subs: Vec<Subscription>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BidiContext {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
}

pub struct BidiHub {
    backend_tx: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    next_client: AtomicU64,
    pending: Mutex<HashMap<u64, Route>>,
    clients: Mutex<HashMap<u64, Client>>,
    backend_events: Mutex<HashSet<String>>,
    closed: AtomicBool,
    /// Most recently navigated/activated top-level context (used by the recorder).
    last_active: Mutex<Option<String>>,
    pub session_id: String,
    pub capabilities: Value,
}

impl BidiHub {
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

        let (backend_tx, mut backend_rx) = mpsc::unbounded_channel::<String>();
        let hub = Arc::new(Self {
            backend_tx,
            next_id: AtomicU64::new(1),
            next_client: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            backend_events: Mutex::new(HashSet::new()),
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
        self.clients.lock().await.clear();
        for (_, route) in self.pending.lock().await.drain() {
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
            let route = self.pending.lock().await.remove(&id);
            match route {
                Some(Route::Client { client, id: orig }) => {
                    v["id"] = orig;
                    if let Some(c) = self.clients.lock().await.get(&client) {
                        let _ = c.tx.send(v.to_string());
                    }
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

        let clients = self.clients.lock().await;
        for c in clients.values() {
            if c.subs.iter().any(|s| s.matches(method, context)) {
                let _ = c.tx.send(text.to_string());
            }
        }
    }

    async fn ensure_backend_events(&self, events: &[String]) {
        let missing: Vec<String> = {
            let mut have = self.backend_events.lock().await;
            let missing: Vec<String> = events
                .iter()
                .filter(|e| {
                    !have.contains(*e)
                        && !have.iter().any(|h| {
                            e.strip_prefix(h.as_str())
                                .is_some_and(|r| r.starts_with('.'))
                        })
                })
                .cloned()
                .collect();
            have.extend(missing.iter().cloned());
            missing
        };
        if !missing.is_empty() {
            if let Err(e) = self
                .call("session.subscribe", json!({ "events": missing }))
                .await
            {
                warn!(error = %e, "BiDi hub failed to subscribe to backend events");
            }
        }
    }

    fn send_backend(&self, id: u64, method: &str, params: Value) {
        let msg = json!({ "id": id, "method": method, "params": params });
        let _ = self.backend_tx.send(msg.to_string());
    }

    /// Issue a command on the shared session and wait for its result.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.is_closed() {
            return Err("browser closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, Route::Internal(tx));
        self.send_backend(id, method, params);
        let res = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(v)) => v,
            _ => {
                self.pending.lock().await.remove(&id);
                return Err(format!("{} timed out", method));
            }
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
            .call("browsingContext.getTree", json!({ "maxDepth": 0 }))
            .await
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in tree["contexts"].as_array().cloned().unwrap_or_default() {
            let id = c["context"].as_str().unwrap_or_default().to_string();
            let url = c["url"].as_str().unwrap_or_default().to_string();
            let title = tokio::time::timeout(
                Duration::from_millis(800),
                self.call(
                    "script.evaluate",
                    json!({ "expression": "document.title", "target": { "context": id }, "awaitPromise": false }),
                ),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|r| r.pointer("/result/value").and_then(Value::as_str).map(str::to_string))
            .filter(|t| !t.is_empty());
            out.push(BidiContext { id, url, title });
        }
        out
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

    /// The tab worth recording: the last one navigated, else the first one.
    pub async fn active_context(&self) -> Option<String> {
        let last = self.last_active.lock().await.clone();
        let tree = self
            .call("browsingContext.getTree", json!({ "maxDepth": 0 }))
            .await
            .ok()?;
        let ids: Vec<String> = tree["contexts"]
            .as_array()?
            .iter()
            .filter_map(|c| c["context"].as_str().map(str::to_string))
            .collect();
        match last {
            Some(l) if ids.contains(&l) => Some(l),
            _ => ids.into_iter().next(),
        }
    }

    pub async fn screenshot_jpeg(&self, context: &str) -> Result<String, String> {
        let res = self
            .call(
                "browsingContext.captureScreenshot",
                json!({
                    "context": context,
                    "origin": "viewport",
                    "format": { "type": "image/jpeg", "quality": 0.6 }
                }),
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
                self.ensure_backend_events(&events).await;
                let sub_id = Uuid::new_v4().to_string();
                if let Some(c) = self.clients.lock().await.get_mut(&client) {
                    c.subs.push(Subscription {
                        id: sub_id.clone(),
                        events,
                        contexts,
                    });
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
                if let Some(c) = self.clients.lock().await.get_mut(&client) {
                    if let Some(ids) = ids {
                        c.subs.retain(|s| !ids.contains(&s.id));
                    } else {
                        for s in &mut c.subs {
                            s.events.retain(|e| !events.contains(e));
                        }
                        c.subs.retain(|s| !s.events.is_empty());
                    }
                }
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
                self.pending
                    .lock()
                    .await
                    .insert(backend_id, Route::Client { client, id });
                self.send_backend(backend_id, &method, params);
            }
        }
    }

    async fn reply(&self, client: u64, msg: Value) {
        if let Some(c) = self.clients.lock().await.get(&client) {
            let _ = c.tx.send(msg.to_string());
        }
    }

    /// Serve one `/firefox` WebSocket client until either side closes.
    pub async fn serve_client(
        self: Arc<Self>,
        socket: ClientWs,
        activity: Arc<crate::idle::Activity>,
        observer: bool,
    ) {
        let client = self.next_client.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        self.clients.lock().await.insert(
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

        self.clients.lock().await.remove(&client);
        self.pending
            .lock()
            .await
            .retain(|_, r| !matches!(r, Route::Client { client: c, .. } if *c == client));
        debug!(client, "BiDi client detached");
    }
}
