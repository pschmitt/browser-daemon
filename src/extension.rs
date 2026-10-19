//! Temporary, outbound-only browser nodes. An extension is never a trusted peer:
//! it gets no cluster credential and can only answer requests for its own tabs.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Semaphore};
use uuid::Uuid;

use crate::{api_error, auth, ApiError, AppState};

const MAX_NODES: usize = 32;
const MAX_TABS: usize = 128;
const MAX_PENDING: usize = 32;
const MAX_FRAME: usize = 8 * 1024 * 1024;

pub struct Broker {
    pairs: Mutex<HashMap<[u8; 32], Pair>>,
    nodes: Mutex<HashMap<Uuid, Arc<Node>>>,
    // Includes sockets still waiting for their first (authentication) message.
    slots: Arc<Semaphore>,
}

impl Broker {
    pub fn new() -> Self {
        Self {
            pairs: Mutex::default(),
            nodes: Mutex::default(),
            slots: Arc::new(Semaphore::new(MAX_NODES)),
        }
    }
}

struct Pair {
    origin: String,
    deadline: Instant,
    lifetime: Duration,
}

struct Node {
    info: Mutex<NodeInfo>,
    deadline: Instant,
    outbound: mpsc::Sender<Value>,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Value>>>,
    stop: tokio::sync::Notify,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub id: u32,
    pub title: String,
    pub url: String,
    pub control: bool,
}

#[derive(Clone, Serialize)]
pub struct NodeInfo {
    pub id: Uuid,
    pub name: String,
    pub browser: String,
    pub native_control: bool,
    pub expires_at: u64,
    pub tabs: Vec<Tab>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub origin: String,
    #[serde(default = "default_lifetime")]
    pub lifetime_secs: u64,
}

fn default_lifetime() -> u64 {
    3600
}

/// Full extension origins only, never a wildcard or a web page origin.
pub fn extension_origin(origin: &str) -> bool {
    let Ok(u) = url::Url::parse(origin) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    matches!(u.scheme(), "chrome-extension" | "moz-extension")
        && !host.is_empty()
        && host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
        && matches!(u.path(), "" | "/")
        && u.query().is_none()
        && u.fragment().is_none()
        && origin == format!("{}://{host}", u.scheme())
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn failure(status: StatusCode, msg: &str) -> ApiError {
    api_error(status, msg)
}

pub async fn pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PairRequest>,
) -> Result<Response, ApiError> {
    // Deliberately unavailable even on a daemon configured without authentication.
    if state.auth.credential(&headers, None, auth::now()) != Some(auth::Credential::Secret) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Pairing needs the cluster secret",
        ));
    }
    if !extension_origin(&body.origin) || !(60..=28800).contains(&body.lifetime_secs) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "Expected an exact extension origin and a lifetime of 60–28800 seconds",
        ));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut pairs = state.extensions.pairs.lock().unwrap();
    pairs.retain(|_, p| p.deadline > Instant::now());
    if pairs.len() >= MAX_NODES {
        return Err(failure(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many unused pairing codes",
        ));
    }
    pairs.insert(
        digest(&token),
        Pair {
            origin: body.origin,
            deadline: Instant::now() + Duration::from_secs(300),
            lifetime: Duration::from_secs(body.lifetime_secs),
        },
    );
    Ok((
        [("cache-control", "no-store")],
        Json(json!({
            "code": token, "expires_in_secs": 300, "lifetime_secs": body.lifetime_secs,
        })),
    )
        .into_response())
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    let nodes: Vec<NodeInfo> = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .values()
        .filter(|n| n.deadline > Instant::now())
        .map(|n| n.info.lock().unwrap().clone())
        .collect();
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"nodes": nodes})),
    )
        .into_response())
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    state.check_auth(&headers)?;
    if let Some(node) = state.extensions.nodes.lock().unwrap().remove(&id) {
        node.stop.notify_one();
        node.pending.lock().unwrap().clear();
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub tab: u32,
    pub action: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}
fn empty_object() -> Value {
    json!({})
}

fn valid_command(command: &Command, control: bool) -> bool {
    if !command.params.is_object() || command.params.to_string().len() > 128 * 1024 {
        return false;
    }
    if matches!(command.action.as_str(), "screenshot" | "snapshot") {
        return true;
    }
    if !control {
        return false;
    }
    match command.action.as_str() {
        "navigate" => command.params["url"].as_str().is_some_and(web_url),
        "click" | "type" | "key" | "scroll" | "reload" | "back" | "forward" | "focus" | "close"
        | "evaluate" => true,
        "cdp" => command.params["method"].as_str().is_some_and(cdp_allowed),
        _ => false,
    }
}

/// No Target/Browser/Storage/Network commands: several act on the entire profile,
/// despite being sent through a tab's debugger. Runtime executes with the shared
/// page's authority and is intentionally only available under a control grant.
pub fn cdp_allowed(method: &str) -> bool {
    matches!(
        method,
        "Runtime.evaluate"
            | "Runtime.callFunctionOn"
            | "Runtime.getProperties"
            | "Runtime.releaseObject"
            | "Runtime.releaseObjectGroup"
            | "DOM.getDocument"
            | "DOM.querySelector"
            | "DOM.querySelectorAll"
            | "DOM.describeNode"
            | "DOM.resolveNode"
            | "DOM.getBoxModel"
            | "DOM.focus"
            | "DOM.scrollIntoViewIfNeeded"
            | "Input.dispatchMouseEvent"
            | "Input.dispatchKeyEvent"
            | "Input.insertText"
            | "Input.dispatchTouchEvent"
            | "Input.synthesizeScrollGesture"
            | "Accessibility.getFullAXTree"
            | "Accessibility.getPartialAXTree"
            | "Page.getFrameTree"
            | "Page.getLayoutMetrics"
            | "Page.captureScreenshot"
    )
}

fn web_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| {
        matches!(u.scheme(), "https" | "http")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

pub async fn command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(command): Json<Command>,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    let node = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Browser disconnected"))?;
    if node.deadline <= Instant::now() {
        return Err(failure(StatusCode::GONE, "Browser grant expired"));
    }
    let control = node
        .info
        .lock()
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == command.tab)
        .map(|t| t.control)
        .ok_or_else(|| failure(StatusCode::FORBIDDEN, "Tab is not shared"))?;
    if !valid_command(&command, control) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Action is not permitted for this tab",
        ));
    }
    let request = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = node.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING {
            return Err(failure(StatusCode::TOO_MANY_REQUESTS, "Browser is busy"));
        }
        pending.insert(request, tx);
    }
    // A cancelled HTTP request must not leave entries behind.
    struct Pending(Arc<Node>, Uuid);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.pending.lock().unwrap().remove(&self.1);
        }
    }
    let _pending = Pending(node.clone(), request);
    node.outbound
        .try_send(json!({"type": "command", "id": request, "tab": command.tab,
        "action": command.action, "params": command.params}))
        .map_err(|_| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Browser is busy or disconnected",
            )
        })?;
    let response = tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "Browser did not answer"))?
        .map_err(|_| failure(StatusCode::GONE, "Browser disconnected"))?;
    // Revocation/expiry wins even when a response was already being delivered.
    if node.deadline <= Instant::now() || !state.extensions.nodes.lock().unwrap().contains_key(&id)
    {
        return Err(failure(StatusCode::GONE, "Browser grant ended"));
    }
    let still_shared = node.info.lock().unwrap().tabs.iter().any(|t| {
        t.id == command.tab
            && (matches!(command.action.as_str(), "screenshot" | "snapshot") || t.control)
    });
    if !still_shared {
        return Err(failure(StatusCode::FORBIDDEN, "Tab grant ended"));
    }
    Ok(([("cache-control", "no-store")], Json(response)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    code: String,
    name: String,
    browser: String,
}

pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let origin = headers
        .get("origin")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !extension_origin(&origin) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "An extension origin is required",
        ));
    }
    let permit = state
        .extensions
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            failure(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many extension connections",
            )
        })?;
    Ok(ws
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |mut ws| async move {
            let _permit = permit;
            let _ = serve(&state.extensions, &origin, &mut ws).await;
            let _ = ws.close().await;
        }))
}

async fn send(ws: &mut WebSocket, value: Value) -> Result<(), ()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        ws.send(Message::Text(value.to_string().into())),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
}

async fn serve(broker: &Arc<Broker>, origin: &str, ws: &mut WebSocket) -> Result<(), ()> {
    // The code never appears in a URL, a cookie, or a log. No reconnect credential
    // is issued: losing the socket ends the grant, and pairing must be repeated.
    let Some(Ok(Message::Text(text))) = tokio::time::timeout(Duration::from_secs(5), ws.recv())
        .await
        .map_err(|_| ())?
    else {
        return Err(());
    };
    if text.len() > 4096 {
        return Err(());
    }
    let hello: Hello = serde_json::from_str(&text).map_err(|_| ())?;
    if hello.code.len() != 64
        || hello.name.is_empty()
        || hello.name.len() > 80
        || !matches!(hello.browser.as_str(), "chromium" | "firefox")
    {
        return Err(());
    }
    let pair = {
        let mut pairs = broker.pairs.lock().unwrap();
        let key = digest(&hello.code);
        let pair = pairs.get(&key).ok_or(())?;
        if pair.deadline <= Instant::now() || pair.origin != origin {
            return Err(());
        }
        pairs.remove(&key).ok_or(())?
    };
    let id = Uuid::new_v4();
    let expires_at = auth::now() + pair.lifetime.as_secs();
    let (outbound, mut commands) = mpsc::channel(MAX_PENDING);
    let node = Arc::new(Node {
        info: Mutex::new(NodeInfo {
            id,
            name: hello.name,
            browser: hello.browser,
            native_control: false,
            expires_at,
            tabs: vec![],
        }),
        deadline: Instant::now() + pair.lifetime,
        outbound,
        pending: Mutex::default(),
        stop: tokio::sync::Notify::new(),
    });
    broker.nodes.lock().unwrap().insert(id, node.clone());
    // All exits, including a task cancellation, remove the registration.
    struct Registration(Arc<Broker>, Uuid);
    impl Drop for Registration {
        fn drop(&mut self) {
            if let Some(n) = self.0.nodes.lock().unwrap().remove(&self.1) {
                n.pending.lock().unwrap().clear();
            }
        }
    }
    let _registration = Registration(broker.clone(), id);
    send(
        ws,
        json!({"type": "registered", "id": id, "expires_at": expires_at}),
    )
    .await?;
    let mut last_seen = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = node.stop.notified() => break,
            _ = tick.tick() => {
                if Instant::now() >= node.deadline || last_seen.elapsed() > Duration::from_secs(50) { break; }
            }
            Some(command) = commands.recv() => send(ws, command).await?,
            incoming = ws.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).map_err(|_| ())?;
                        last_seen = Instant::now();
                        match value["type"].as_str() {
                            Some("heartbeat") => send(ws, json!({"type": "heartbeat"})).await?,
                            Some("tabs") => {
                                let tabs: Vec<Tab> = serde_json::from_value(value["tabs"].clone()).map_err(|_| ())?;
                                if tabs.len() > MAX_TABS || tabs.iter().any(|t| !web_url(&t.url) || t.url.len() > 8192 || t.title.len() > 1024)
                                    || tabs.iter().map(|t| t.id).collect::<std::collections::HashSet<_>>().len() != tabs.len() { return Err(()); }
                                let mut info = node.info.lock().unwrap();
                                info.tabs = tabs;
                                info.native_control = value["native_control"].as_bool().unwrap_or(false);
                            }
                            Some("result") => {
                                let request = value["id"].as_str().and_then(|s| Uuid::parse_str(s).ok()).ok_or(())?;
                                if let Some(tx) = node.pending.lock().unwrap().remove(&request) {
                                    let _ = tx.send(json!({"result": value["result"], "error": value["error"]}));
                                }
                            }
                            _ => return Err(()),
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(data) => ws.send(Message::Pong(data)).await.map_err(|_| ())?,
                    Message::Pong(_) => {},
                    _ => return Err(()),
                }
            }
        }
    }
    Ok(())
}

pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    Ok(([("cache-control", "no-store"), ("referrer-policy", "no-referrer"),
        ("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")],
        Html(include_str!("../extension.html"))).into_response())
}

pub async fn javascript() -> impl IntoResponse {
    (
        [
            ("content-type", "text/javascript"),
            ("cache-control", "no-cache"),
        ],
        include_str!("../assets/extension.js"),
    )
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        [("content-type", "text/css"), ("cache-control", "no-cache")],
        include_str!("../assets/extension.css"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    async fn receive_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Option<Value>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let message = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .ok()??
            .ok()?;
        match message {
            tokio_tungstenite::tungstenite::Message::Text(t) => serde_json::from_str(&t).ok(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn pairing_is_origin_bound_single_use_and_disconnect_removes_the_node() {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Wire};
        let broker = Arc::new(Broker::new());
        let code = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let origin = "chrome-extension://abcdefghijklmnop";
        broker.pairs.lock().unwrap().insert(
            digest(&code),
            Pair {
                origin: origin.into(),
                deadline: Instant::now() + Duration::from_secs(60),
                lifetime: Duration::from_secs(600),
            },
        );
        let handler_broker = broker.clone();
        let app = axum::Router::new().route(
            "/connect",
            axum::routing::get(move |headers: HeaderMap, upgrade: WebSocketUpgrade| {
                let broker = handler_broker.clone();
                async move {
                    let origin = headers.get("origin").unwrap().to_str().unwrap().to_string();
                    upgrade.on_upgrade(move |mut ws| async move {
                        let _ = serve(&broker, &origin, &mut ws).await;
                        let _ = ws.close().await;
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let hello =
            json!({"code": code, "name": "Test browser", "browser": "chromium"}).to_string();
        // Wrong origin cannot consume a legitimate pairing code.
        let mut bad = format!("ws://{address}/connect")
            .into_client_request()
            .unwrap();
        bad.headers_mut()
            .insert("origin", "chrome-extension://wrong".parse().unwrap());
        let (mut wrong, _) = tokio_tungstenite::connect_async(bad).await.unwrap();
        wrong.send(Wire::Text(hello.clone().into())).await.unwrap();
        assert!(receive_json(&mut wrong).await.is_none());
        assert_eq!(broker.pairs.lock().unwrap().len(), 1);
        let mut request = format!("ws://{address}/connect")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(request.clone())
            .await
            .unwrap();
        ws.send(Wire::Text(hello.clone().into())).await.unwrap();
        let registered = receive_json(&mut ws).await.unwrap();
        assert_eq!(registered["type"], "registered");
        assert_eq!(broker.nodes.lock().unwrap().len(), 1);
        assert!(broker.pairs.lock().unwrap().is_empty());
        let (mut replay, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        replay.send(Wire::Text(hello.into())).await.unwrap();
        assert!(receive_json(&mut replay).await.is_none());
        ws.send(Wire::Text(json!({"type": "tabs", "tabs": [{"id": 7, "title": "Example", "url": "https://example.org/", "control": false}]}).to_string().into())).await.unwrap();
        ws.send(Wire::Text(json!({"type": "heartbeat"}).to_string().into()))
            .await
            .unwrap();
        assert_eq!(receive_json(&mut ws).await.unwrap()["type"], "heartbeat");
        let id = Uuid::parse_str(registered["id"].as_str().unwrap()).unwrap();
        let node = broker.nodes.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(node.info.lock().unwrap().tabs[0].id, 7);
        let request_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        node.pending.lock().unwrap().insert(request_id, tx);
        ws.send(Wire::Text(
            json!({"type": "result", "id": request_id, "result": {"data": "example"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rx)
                .await
                .unwrap()
                .unwrap()["result"]["data"],
            "example"
        );
        ws.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !broker.nodes.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }

    #[test]
    fn origin_must_be_an_exact_extension_origin() {
        for origin in [
            "chrome-extension://abcdefghijklmnop",
            "moz-extension://abcdef01-1234",
        ] {
            assert!(extension_origin(origin));
        }
        for origin in [
            "https://evil.test",
            "null",
            "chrome-extension://abc/",
            "chrome-extension://abc/path",
            "moz-extension://abc?x",
            "chrome-extension://abc:80",
            "chrome-extension://abc.evil",
            "chrome-extension://user@abc",
            "chrome-extension://abc#x",
        ] {
            assert!(!extension_origin(origin), "{origin}");
        }
    }

    #[test]
    fn viewing_never_allows_script_or_input() {
        for action in [
            "screenshot",
            "snapshot",
            "navigate",
            "type",
            "cdp",
            "evaluate",
            "focus",
            "close",
        ] {
            let c = Command {
                tab: 1,
                action: action.into(),
                params: json!({"method": "Runtime.evaluate", "url": "https://example.org"}),
            };
            assert_eq!(
                valid_command(&c, false),
                matches!(action, "screenshot" | "snapshot")
            );
        }
    }

    #[test]
    fn control_cannot_escape_via_profile_wide_protocol_commands() {
        for method in [
            "Target.attachToTarget",
            "Target.createTarget",
            "Target.sendMessageToTarget",
            "Browser.close",
            "Storage.getCookies",
            "Network.getAllCookies",
            "Page.navigate",
            "Page.addScriptToEvaluateOnNewDocument",
            "IO.read",
        ] {
            assert!(!cdp_allowed(method), "{method}");
        }
        for address in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "data:text/html,hi",
            "https://user:password@example.org",
        ] {
            assert!(!valid_command(
                &Command {
                    tab: 1,
                    action: "navigate".into(),
                    params: json!({"url": address})
                },
                true
            ));
        }
        assert!(cdp_allowed("Runtime.evaluate"));
    }
}
