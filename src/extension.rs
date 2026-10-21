//! Temporary, outbound-only browser nodes. An extension is never a trusted peer:
//! it gets no cluster credential and can only answer requests for its own tabs.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path as FilePath, PathBuf};
use tokio::sync::{mpsc, oneshot, Semaphore};
use uuid::Uuid;

use crate::{api_error, auth, ApiError, AppState};

const MAX_NODES: usize = 32;
const MAX_TABS: usize = 128;
const MAX_PENDING: usize = 32;
const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_FAVICON_BYTES: usize = 128 * 1024;
const MAX_FAVICON_DATA_URL: usize = 176 * 1024;
const MAX_FAVICON_BATCH: usize = 4;
// Live views send every input event; the socket applies backpressure beyond this.
const MAX_OUTBOUND: usize = 256;
const MAX_CHANNELS: usize = 16;

pub fn normalize_origins(
    origins: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, std::io::Error> {
    let mut normalized = Vec::new();
    for value in origins {
        let url = url::Url::parse(&value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let loopback = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        );
        if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "extension origins must be HTTPS origins (HTTP is allowed on loopback)",
            ));
        }
        let origin = url.origin().ascii_serialization();
        if !normalized.contains(&origin) {
            normalized.push(origin);
        }
    }
    Ok(normalized)
}

pub async fn config(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let mut origins = state.extension_origins.clone();
    origins.extend(state.peers.read().await.values().cloned());
    let mut origins = normalize_origins(origins).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    origins.sort();
    let mut response = Json(json!({"origins": origins})).into_response();
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .filter(|origin| extension_origin(origin))
    {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_str(origin).expect("validated extension origin"),
        );
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub struct Broker {
    pairs: Mutex<HashMap<[u8; 32], Pair>>,
    nodes: Mutex<HashMap<Uuid, Arc<Node>>>,
    // Includes sockets still waiting for their first (authentication) message.
    slots: Arc<Semaphore>,
    credentials: Mutex<Vec<Remembered>>,
    credential_path: Option<PathBuf>,
    // Offline remembered browsers are forgotten after this long (None: never).
    forget_after: Option<Duration>,
}

impl Broker {
    pub fn new() -> Self {
        Self {
            pairs: Mutex::default(),
            nodes: Mutex::default(),
            slots: Arc::new(Semaphore::new(MAX_NODES)),
            credentials: Mutex::default(),
            credential_path: None,
            forget_after: None,
        }
    }
    pub fn load(path: &FilePath, forget_after: Option<Duration>) -> Self {
        let mut broker = Self::new();
        broker.forget_after = forget_after;
        let records = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Vec<Remembered>>(&bytes)
                .expect("Invalid remembered browser authorization store"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => panic!("Cannot read remembered browser authorization store"),
        };
        // Records from before last_seen existed count from now.
        let now = auth::now();
        *broker.credentials.get_mut().unwrap() = records
            .into_iter()
            .map(|r| Remembered {
                last_seen: if r.last_seen == 0 { now } else { r.last_seen },
                ..r
            })
            .collect();
        broker.credential_path = Some(path.to_path_buf());
        broker
    }

    /// Remembered browsers that stayed offline too long are forgotten: their
    /// key is revoked and they leave the node list.
    fn sweep(&self) {
        let Some(after) = self.forget_after else {
            return;
        };
        let online: Vec<Uuid> = self.nodes.lock().unwrap().keys().copied().collect();
        let now = auth::now();
        let mut records = self.credentials.lock().unwrap();
        let stale =
            |r: &Remembered| !online.contains(&r.id) && r.last_seen + after.as_secs() <= now;
        if records.iter().any(stale) {
            let keep: Vec<_> = records.iter().filter(|r| !stale(r)).cloned().collect();
            if self.save_credentials(&keep).is_ok() {
                *records = keep;
            }
        }
    }

    fn touch(&self, id: Uuid) {
        let mut records = self.credentials.lock().unwrap();
        if let Some(i) = records.iter().position(|r| r.id == id) {
            let mut updated = records.clone();
            updated[i].last_seen = auth::now();
            if self.save_credentials(&updated).is_ok() {
                *records = updated;
            }
        }
    }

    fn save_credentials(&self, records: &[Remembered]) -> Result<(), ()> {
        let Some(path) = &self.credential_path else {
            return Err(());
        };
        let parent = path.parent().ok_or(())?;
        std::fs::create_dir_all(parent).map_err(|_| ())?;
        let temporary = parent.join(format!(".browser-keys-{}", Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|_| ())?;
            file.write_all(&serde_json::to_vec(records).map_err(|_| ())?)
                .map_err(|_| ())?;
            file.sync_all().map_err(|_| ())?;
            std::fs::rename(&temporary, path).map_err(|_| ())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }

    fn forget(&self, id: Uuid) -> Result<(), ()> {
        let mut records = self.credentials.lock().unwrap();
        if records.iter().any(|r| r.id == id) {
            let remaining: Vec<_> = records.iter().filter(|r| r.id != id).cloned().collect();
            self.save_credentials(&remaining)?;
            *records = remaining;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Remembered {
    id: Uuid,
    origin: String,
    name: String,
    browser: String,
    // Only a SHA-256 digest reaches disk; the random credential stays client-side.
    key_digest: String,
    // Unix seconds of its last connection or disconnection.
    #[serde(default)]
    last_seen: u64,
}

fn key_digest(key: &str) -> String {
    digest(key).iter().map(|b| format!("{b:02x}")).collect()
}

struct Pair {
    origin: String,
    deadline: Instant,
    lifetime: Duration,
    remember: bool,
}

struct Node {
    info: Mutex<NodeInfo>,
    public_grant: Mutex<Option<Uuid>>,
    deadline: Option<Instant>,
    outbound: mpsc::Sender<Value>,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Value>>>,
    // Live views of shared tabs (see `cdp`). Dropping one closes its socket.
    channels: Mutex<HashMap<Uuid, Channel>>,
    stop: tokio::sync::Notify,
    // Bumped when its shared tabs change or it goes away (see `changes`).
    revision: std::sync::atomic::AtomicU64,
    changed: tokio::sync::Notify,
}

struct Channel {
    tab: u32,
    tx: mpsc::Sender<String>,
    // Internal recording channels are not interactive clients.
    client: bool,
    // A CDP client's session (see automation.rs): all of the tab's events, not
    // only the live view's.
    automation: bool,
}

impl Node {
    fn end(&self) {
        self.pending.lock().unwrap().clear();
        self.channels.lock().unwrap().clear();
        self.bump();
    }
    fn bump(&self) {
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_waiters();
    }
    fn control(&self, tab: u32) -> Option<bool> {
        let info = self.info.lock().unwrap();
        info.tabs.iter().find(|t| t.id == tab).map(|t| t.control)
    }
    fn client_connected(&self) -> bool {
        let channel = self
            .channels
            .lock()
            .unwrap()
            .values()
            .any(|channel| channel.client);
        channel || !self.pending.lock().unwrap().is_empty()
    }
    fn notify_client_presence(&self) {
        let _ = self.outbound.try_send(json!({
            "type": "client_presence",
            "connected": self.client_connected(),
        }));
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub id: u32,
    pub title: String,
    pub url: String,
    /// The browser-reported favicon, embedded separately or proxied by the daemon.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    pub control: bool,
    /// Position in the browser window; used to preserve the owner's tab order.
    #[serde(default)]
    pub index: Option<u32>,
    /// Window identity is only used to keep handoff drag-and-drop within a window.
    #[serde(default)]
    pub window_id: Option<i32>,
    /// Firefox/Chromium pinned-tab state.
    #[serde(default)]
    pub pinned: bool,
    /// In front of its window: what the owner sees, and what a recording shows.
    #[serde(default)]
    pub active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FaviconUpdate {
    id: u32,
    favicon: String,
}

#[derive(Clone, Serialize)]
pub struct NodeInfo {
    pub id: Uuid,
    pub name: String,
    pub browser: String,
    pub native_control: bool,
    pub expires_at: u64,
    pub remembered: bool,
    pub online: bool,
    /// The extension's version; empty for releases before live views (0.3.0).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// When an offline remembered browser will be forgotten (Unix seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forget_at: Option<u64>,
    /// Its owner lets controllers open tabs (they are shared with control).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub can_open: bool,
    /// Whether the owner lets live views activate the tab they are showing.
    pub follow: bool,
    /// Owner enabled media-element sound sharing.
    pub audio: bool,
    /// Owner enabled viewer file transfers under a control grant.
    pub files: bool,
    pub public_share: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<crate::shares::ShareView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recording_paused: bool,
    pub tabs: Vec<Tab>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub origin: String,
    #[serde(default = "default_lifetime")]
    pub lifetime_secs: u64,
    #[serde(default)]
    pub remember: bool,
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
    // CLI secret, or an authenticated same-origin HTTPS dashboard request.
    // An open daemon and a viewer bearer token alone cannot pair.
    if !state.auth.can_pair(&headers, auth::now()) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Pairing needs the CLI secret or a signed-in HTTPS dashboard",
        ));
    }
    if !extension_origin(&body.origin)
        || (body.lifetime_secs != 0 && !(60..=28800).contains(&body.lifetime_secs))
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "Expected an exact extension origin and a lifetime of 0 (no time limit) or 60–28800 seconds",
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
            lifetime: Duration::from_secs(if body.remember { 0 } else { body.lifetime_secs }),
            remember: body.remember,
        },
    );
    Ok((
        [("cache-control", "no-store")],
        Json(json!({
            "code": token, "expires_in_secs": 300, "lifetime_secs": if body.remember { 0 } else { body.lifetime_secs }, "remember": body.remember,
        })),
    )
        .into_response())
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    state.extensions.sweep();
    let forget_after = state.extensions.forget_after;
    let mut nodes: Vec<NodeInfo> = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .values()
        .filter(|n| n.deadline.is_none_or(|d| d > Instant::now()))
        .map(|n| n.info.lock().unwrap().clone())
        .collect();
    for record in state.extensions.credentials.lock().unwrap().iter() {
        if !nodes.iter().any(|n| n.id == record.id) {
            nodes.push(NodeInfo {
                id: record.id,
                name: record.name.clone(),
                browser: record.browser.clone(),
                native_control: false,
                expires_at: 0,
                remembered: true,
                online: false,
                version: String::new(),
                forget_at: forget_after.map(|a| record.last_seen + a.as_secs()),
                can_open: false,
                follow: false,
                audio: false,
                files: false,
                public_share: false,
                share: None,
                recording: false,
                recording_paused: false,
                tabs: vec![],
            });
        }
    }
    for info in &mut nodes {
        if let Some((_, grant)) = state.extensions.public_browser(info.id, None) {
            if let Some(share) = state
                .shares
                .all()
                .into_iter()
                .find(|s| s.target == info.id && s.browser_grant == Some(grant))
            {
                info.share = Some(state.share_view(share));
            }
        }
    }
    {
        let recordings = state.active_recordings.read().await;
        for node in &mut nodes {
            if let Some(handle) = recordings.get(&node.id) {
                node.recording = true;
                node.recording_paused = handle.is_paused();
            }
        }
    }
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
    state.extensions.forget(id).map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not revoke remembered browser authorization",
        )
    })?;
    if let Some(node) = state.extensions.nodes.lock().unwrap().remove(&id) {
        node.stop.notify_one();
        node.end();
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
        "suggest" => command.params["query"]
            .as_str()
            .is_some_and(|q| q.len() <= 200),
        "navigate" | "open" => command.params["url"].as_str().is_some_and(page_url),
        "click" | "type" | "key" | "scroll" | "reload" | "back" | "forward" | "focus" | "close"
        | "evaluate" => true,
        "move" => command.params["index"]
            .as_u64()
            .is_some_and(|i| i <= u32::MAX as u64),
        "pin" => command.params["pinned"].is_boolean(),
        "follow" => command.tab == 0 && command.params["enabled"].is_boolean(),
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

/// The live view's protocol: frames and page state for every viewer, and the
/// page's own input, navigation and dialogs only under a control grant.
pub fn relay_allowed(method: &str, params: &Value, control: bool) -> bool {
    if !params.is_object() || params.to_string().len() > 128 * 1024 {
        return false;
    }
    if matches!(
        method,
        "Page.startAudio"
            | "Page.stopAudio"
            | "Page.enable"
            | "Page.bringToFront"
            | "Page.startScreencast"
            | "Page.stopScreencast"
            | "Page.screencastFrameAck"
            | "Page.captureScreenshot"
            | "Page.getLayoutMetrics"
            | "Page.getNavigationHistory"
            | "Page.getFrameTree"
            | "Accessibility.getFullAXTree"
            | "Accessibility.getPartialAXTree"
    ) {
        return true;
    }
    if !control {
        return false;
    }
    match method {
        "Page.navigate" => params["url"].as_str().is_some_and(page_url),
        "Page.reload"
        | "Page.navigateToHistoryEntry"
        | "Page.handleJavaScriptDialog"
        | "Page.startFiles"
        | "Page.stopFiles"
        | "Page.fileTarget"
        | "Page.beginFile"
        | "Page.fileChunk"
        | "Page.cancelFile"
        | "Page.applyFiles"
        | "Page.listDownloads"
        | "Page.readDownload"
        | "Page.removeDownload" => true,
        _ => cdp_allowed(method),
    }
}

/// Events a view-only viewer may see: the page's frames and navigation state.
fn view_event(method: &str) -> bool {
    method.starts_with("Page.") || method == "Inspector.detached"
}

/// What CDP clients (Playwright's `connectOverCDP`, see automation.rs) may do in
/// a tab shared with control, on top of the live view's protocol: the page's own
/// domains, never the profile's. No cookies or cache of other sites, no requests
/// made with the browser's credentials outside the page, no files from the
/// owner's disk and no downloads to it.
pub fn automation_allowed(method: &str, params: &Value) -> bool {
    if matches!(
        method,
        "Page.setDownloadBehavior"
            | "Page.crash"
            | "Network.getAllCookies"
            | "Network.getCookies"
            | "Network.setCookie"
            | "Network.setCookies"
            | "Network.deleteCookies"
            | "Network.clearBrowserCookies"
            | "Network.clearBrowserCache"
            | "Network.loadNetworkResource"
            | "DOM.setFileInputFiles"
    ) {
        return false;
    }
    if method == "Page.navigate" {
        return params["url"].as_str().is_some_and(page_url);
    }
    if matches!(method, "IO.read" | "IO.close") {
        return true;
    }
    matches!(
        method.split_once('.').map(|(domain, _)| domain),
        Some(
            "Page"
                | "Runtime"
                | "DOM"
                | "Input"
                | "Emulation"
                | "Network"
                | "Fetch"
                | "Log"
                | "Accessibility"
                | "CSS"
                | "Overlay"
                | "Performance"
                | "DOMSnapshot"
        )
    )
}

/// A web page, or the blank page a CDP client opens before navigating.
fn page_url(s: &str) -> bool {
    s == "about:blank" || web_url(s)
}

fn web_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| {
        matches!(u.scheme(), "https" | "http")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

fn valid_favicon(value: &str) -> bool {
    if web_url(value) {
        return value.len() <= 2048;
    }
    if value.len() > MAX_FAVICON_DATA_URL {
        return false;
    }
    let Some((header, payload)) = value.split_once(',') else {
        return false;
    };
    let Some(mime) = header
        .strip_prefix("data:")
        .and_then(|header| header.strip_suffix(";base64"))
    else {
        return false;
    };
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/x-icon" | "image/webp"
    ) {
        return false;
    }
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .is_ok_and(|bytes| bytes.len() <= MAX_FAVICON_BYTES)
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
    if node.deadline.is_some_and(|d| d <= Instant::now()) {
        return Err(failure(StatusCode::GONE, "Browser grant expired"));
    }
    // Tab 0: open the browser's first tab, where its owner allows that.
    let tabless = command.tab == 0 && matches!(command.action.as_str(), "open" | "follow");
    let control = if command.action == "open" && tabless {
        node.info.lock().unwrap().can_open
    } else if command.action == "follow" && tabless {
        true
    } else {
        node.control(command.tab)
            .ok_or_else(|| failure(StatusCode::FORBIDDEN, "Tab is not shared"))?
    };
    if !valid_command(&command, control) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Action is not permitted for this tab",
        ));
    }
    let response = ask(&node, command.tab, &command.action, command.params).await?;
    // Revocation/expiry wins even when a response was already being delivered.
    if node.deadline.is_some_and(|d| d <= Instant::now())
        || !state.extensions.nodes.lock().unwrap().contains_key(&id)
    {
        return Err(failure(StatusCode::GONE, "Browser grant ended"));
    }
    let still_shared = tabless
        || node.info.lock().unwrap().tabs.iter().any(|t| {
            t.id == command.tab
                && (matches!(command.action.as_str(), "screenshot" | "snapshot") || t.control)
        });
    if !still_shared {
        return Err(failure(StatusCode::FORBIDDEN, "Tab grant ended"));
    }
    Ok(([("cache-control", "no-store")], Json(response)).into_response())
}

/// Sends one command to the browser and waits for its answer ({result, error}).
async fn ask(node: &Arc<Node>, tab: u32, action: &str, params: Value) -> Result<Value, ApiError> {
    ask_granted(node, tab, action, params, None).await
}

async fn ask_granted(
    node: &Arc<Node>,
    tab: u32,
    action: &str,
    params: Value,
    grant: Option<Uuid>,
) -> Result<Value, ApiError> {
    let request = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = node.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING {
            return Err(failure(StatusCode::TOO_MANY_REQUESTS, "Browser is busy"));
        }
        pending.insert(request, tx);
    }
    node.notify_client_presence();
    // A cancelled HTTP request must not leave entries behind.
    struct Pending(Arc<Node>, Uuid);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.pending.lock().unwrap().remove(&self.1);
            self.0.notify_client_presence();
        }
    }
    let _pending = Pending(node.clone(), request);
    node.outbound
        .try_send(json!({"type": "command", "id": request, "tab": tab,
        "action": action, "params": params, "public_grant": grant}))
        .map_err(|_| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Browser is busy or disconnected",
            )
        })?;
    tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "Browser did not answer"))?
        .map_err(|_| failure(StatusCode::GONE, "Browser disconnected"))
}

impl Broker {
    pub(crate) fn public_browser(&self, id: Uuid, grant: Option<Uuid>) -> Option<(NodeInfo, Uuid)> {
        let node = self.nodes.lock().unwrap().get(&id).cloned()?;
        if node.deadline.is_some_and(|d| d <= Instant::now()) {
            return None;
        }
        let info = node.info.lock().unwrap().clone();
        let current = (*node.public_grant.lock().unwrap())?;
        if grant.is_some_and(|g| g != current) || !info.public_share {
            return None;
        }
        Some((info, current))
    }

    pub(crate) async fn public_command(
        &self,
        id: Uuid,
        grant: Uuid,
        tab: u32,
        action: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        let (info, _) = self
            .public_browser(id, Some(grant))
            .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Public sharing ended"))?;
        let control = info.tabs.iter().find(|t| t.id == tab).map(|t| t.control);
        let first_tab = tab == 0 && action == "open" && info.can_open;
        if !first_tab
            && (control.is_none()
                || (!matches!(action, "screenshot" | "publicAudio") && control != Some(true)))
        {
            return Err(failure(
                StatusCode::FORBIDDEN,
                "Tab is not shared with this permission",
            ));
        }
        let node = self
            .nodes
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Browser disconnected"))?;
        let result = ask_granted(&node, tab, action, params, Some(grant)).await?;
        self.public_browser(id, Some(grant))
            .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Public sharing ended"))?;
        if !first_tab && action != "close" && node.control(tab).is_none() {
            return Err(failure(StatusCode::FORBIDDEN, "Tab grant ended"));
        }
        if result["error"].is_string() {
            return Err(failure(
                StatusCode::BAD_GATEWAY,
                "The browser rejected the action",
            ));
        }
        Ok(result["result"].clone())
    }
}

/// Vault requests name a shared tab as a page: "ext-<browser>-<tab>".
pub(crate) fn page_id(node: Uuid, tab: u32) -> String {
    format!("ext-{node}-{tab}")
}
pub(crate) fn parse_page_id(page: &str) -> Option<(Uuid, u32)> {
    let (node, tab) = page.strip_prefix("ext-")?.rsplit_once('-')?;
    Some((Uuid::parse_str(node).ok()?, tab.parse().ok()?))
}

/// A shared tab its owner lets viewers control: the browser and the tab's address
/// (as the browser reports it). Vault logins and passkeys are only for these.
pub(crate) struct Controlled {
    node: Arc<Node>,
    pub(crate) tab: u32,
    pub(crate) url: String,
    pub(crate) firefox: bool,
}
pub(crate) fn controlled(state: &AppState, id: Uuid, tab: u32) -> Result<Controlled, ApiError> {
    let node = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Browser disconnected"))?;
    if node.deadline.is_some_and(|d| d <= Instant::now()) {
        return Err(failure(StatusCode::GONE, "Browser grant expired"));
    }
    let (url, firefox) = {
        let info = node.info.lock().unwrap();
        let t = info
            .tabs
            .iter()
            .find(|t| t.id == tab && t.control)
            .ok_or_else(|| failure(StatusCode::FORBIDDEN, "Tab is not shared with control"))?;
        (t.url.clone(), info.browser == "firefox")
    };
    Ok(Controlled {
        node,
        tab,
        url,
        firefox,
    })
}

impl Controlled {
    /// Fills a vault login into the tab, in its frames on the entry's sites only
    /// (the browser lists the frames; each is filled only if still at that address).
    pub(crate) async fn fill(
        &self,
        frame_ok: impl Fn(&str) -> bool,
        creds: &crate::autofill::Creds,
    ) -> Result<Vec<String>, ApiError> {
        let answer = |v: Value| -> Result<Value, ApiError> {
            if let Some(e) = v["error"].as_str() {
                return Err(failure(StatusCode::BAD_GATEWAY, e));
            }
            Ok(v["result"].clone())
        };
        let listed = answer(ask(&self.node, self.tab, "frames", json!({})).await?)?;
        let frames: Vec<Value> = listed
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| {
                f["url"].as_str().is_some_and(&frame_ok)
                    && (f["frame"].is_u64() || f["frame"].is_string())
            })
            .map(|f| json!({"frame": f["frame"], "url": f["url"]}))
            .collect();
        if frames.is_empty() {
            return Ok(Vec::new());
        }
        let params = json!({
            "frames": frames,
            "css": crate::autofill::secret_css(self.firefox),
            "username": creds.username,
            "password": creds.password,
            "totp": creds.totp,
            "custom": creds.custom,
        });
        let filled = answer(ask(&self.node, self.tab, "fill", params).await?)?;
        Ok(filled["filled"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect())
    }
}

#[path = "extension_automation.rs"]
pub mod automation;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    #[serde(default)]
    code: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    remember: bool,
    name: String,
    browser: String,
    #[serde(default)]
    version: String,
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
            let _ = serve(
                &state.extensions,
                &state.extension_origins,
                &state.peers,
                &origin,
                &mut ws,
            )
            .await;
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

async fn current_extension_origins(
    extension_origins: &[String],
    peers: &tokio::sync::RwLock<HashMap<String, String>>,
) -> Result<Vec<String>, ()> {
    let mut origins = extension_origins.to_vec();
    origins.extend(peers.read().await.values().cloned());
    let mut origins = normalize_origins(origins).map_err(|_| ())?;
    origins.sort();
    Ok(origins)
}

async fn serve(
    broker: &Arc<Broker>,
    extension_origins: &[String],
    peers: &tokio::sync::RwLock<HashMap<String, String>>,
    origin: &str,
    ws: &mut WebSocket,
) -> Result<(), ()> {
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
    if (hello.code.len() != 64 && hello.key.len() != 64)
        || (!hello.code.is_empty() && !hello.key.is_empty())
        || hello.name.is_empty()
        || hello.name.len() > 80
        || !matches!(hello.browser.as_str(), "chromium" | "firefox")
        || hello.version.len() > 32
        || !hello
            .version
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    {
        return Err(());
    }
    let mut remembered = false;
    let mut new_key = None;
    let (id, lifetime) = if !hello.key.is_empty() {
        let record = {
            let records = broker.credentials.lock().unwrap();
            records
                .iter()
                .find(|r| {
                    r.origin == origin
                        && r.browser == hello.browser
                        && r.key_digest == key_digest(&hello.key)
                })
                .cloned()
        };
        // A failover URL can temporarily reach another daemon. An unknown key
        // must be refused without telling the client to erase its authorization
        // for the original host. Explicit revocation of a live node sends the
        // separate authorization_revoked message below.
        let Some(record) = record else { return Err(()) };
        remembered = true;
        (record.id, Duration::ZERO)
    } else {
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
        if pair.remember && hello.remember {
            let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let mut records = broker.credentials.lock().unwrap();
            if records.len() >= 128 {
                return Err(());
            }
            let mut updated = records.clone();
            updated.push(Remembered {
                id,
                origin: origin.to_string(),
                name: hello.name.clone(),
                browser: hello.browser.clone(),
                key_digest: key_digest(&key),
                last_seen: auth::now(),
            });
            broker.save_credentials(&updated)?;
            *records = updated;
            remembered = true;
            new_key = Some(key);
        }
        (
            id,
            if pair.remember && !hello.remember {
                Duration::from_secs(3600)
            } else {
                pair.lifetime
            },
        )
    };
    let expires_at = if lifetime.is_zero() {
        0
    } else {
        auth::now() + lifetime.as_secs()
    };
    let (outbound, mut commands) = mpsc::channel(MAX_OUTBOUND);
    let node = Arc::new(Node {
        info: Mutex::new(NodeInfo {
            id,
            name: hello.name,
            browser: hello.browser,
            native_control: false,
            expires_at,
            remembered,
            online: true,
            version: hello.version,
            forget_at: None,
            can_open: false,
            follow: false,
            audio: false,
            files: false,
            public_share: false,
            share: None,
            recording: false,
            recording_paused: false,
            tabs: vec![],
        }),
        public_grant: Mutex::default(),
        deadline: (!lifetime.is_zero()).then(|| Instant::now() + lifetime),
        outbound,
        pending: Mutex::default(),
        channels: Mutex::default(),
        stop: tokio::sync::Notify::new(),
        revision: std::sync::atomic::AtomicU64::new(0),
        changed: tokio::sync::Notify::new(),
    });
    if let Some(previous) = broker.nodes.lock().unwrap().insert(id, node.clone()) {
        previous.stop.notify_one();
        previous.end();
    }
    // All exits, including a task cancellation, remove the registration.
    struct Registration(Arc<Broker>, Uuid, Arc<Node>);
    impl Drop for Registration {
        fn drop(&mut self) {
            {
                let mut nodes = self.0.nodes.lock().unwrap();
                if nodes.get(&self.1).is_some_and(|n| Arc::ptr_eq(n, &self.2)) {
                    if let Some(n) = nodes.remove(&self.1) {
                        n.end();
                    }
                }
            }
            // The forget timer counts from the disconnection.
            self.0.touch(self.1);
        }
    }
    let _registration = Registration(broker.clone(), id, node.clone());
    broker.touch(id);
    let origins = current_extension_origins(extension_origins, peers).await?;
    send(
        ws,
        json!({"type": "registered", "id": id, "expires_at": expires_at, "remembered": remembered, "remember_key": new_key, "origins": origins}),
    )
    .await?;
    let mut last_seen = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = node.stop.notified() => {
                send(ws, json!({"type": "authorization_revoked"})).await?;
                break;
            },
            _ = tick.tick() => {
                if node.deadline.is_some_and(|d| Instant::now() >= d) || last_seen.elapsed() > Duration::from_secs(50) { break; }
            }
            Some(command) = commands.recv() => send(ws, command).await?,
            incoming = ws.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).map_err(|_| ())?;
                        last_seen = Instant::now();
                        match value["type"].as_str() {
                            Some("forget") => { broker.forget(id)?; break; }
                            Some("heartbeat") => {
                                let origins = current_extension_origins(extension_origins, peers).await?;
                                send(ws, json!({"type": "heartbeat", "client_connected": node.client_connected(), "origins": origins})).await?;
                            }
                            Some("tabs") => {
                                let tabs: Vec<Tab> = serde_json::from_value(value["tabs"].clone()).map_err(|_| ())?;
                                if tabs.len() > MAX_TABS || tabs.iter().any(|t| !page_url(&t.url) || t.url.len() > 8192 || t.title.len() > 1024 || (!t.favicon.is_empty() && !valid_favicon(&t.favicon)))
                                    || tabs.iter().map(|t| t.id).collect::<std::collections::HashSet<_>>().len() != tabs.len() { return Err(()); }
                                let was_connected = node.client_connected();
                                node.channels.lock().unwrap().retain(|_, c| tabs.iter().any(|t| t.id == c.tab));
                                if was_connected != node.client_connected() {
                                    node.notify_client_presence();
                                }
                                let mut info = node.info.lock().unwrap();
                                info.tabs = tabs;
                                info.native_control = value["native_control"].as_bool().unwrap_or(false);
                                info.can_open = value["can_open"].as_bool().unwrap_or(false);
                                if let Some(follow) = value["follow"].as_bool() {
                                    info.follow = follow;
                                }
                                info.audio = value["audio"].as_bool().unwrap_or(false);
                                info.files = value["files"].as_bool().unwrap_or(false);
                                let grant = value["public_grant"].as_str().and_then(|s| Uuid::parse_str(s).ok());
                                info.public_share = grant.is_some();
                                *node.public_grant.lock().unwrap() = grant;
                                drop(info);
                                node.bump();
                            }
                            Some("favicons") => {
                                let favicons: Vec<FaviconUpdate> = serde_json::from_value(value["favicons"].clone()).map_err(|_| ())?;
                                if favicons.len() > MAX_FAVICON_BATCH
                                    || favicons.iter().any(|icon| !icon.favicon.starts_with("data:image/") || !valid_favicon(&icon.favicon))
                                    || favicons.iter().map(|icon| icon.id).collect::<std::collections::HashSet<_>>().len() != favicons.len() { return Err(()); }
                                let mut info = node.info.lock().unwrap();
                                let mut changed = false;
                                for icon in favicons {
                                    if let Some(tab) = info.tabs.iter_mut().find(|tab| tab.id == icon.id) {
                                        tab.favicon = icon.favicon;
                                        changed = true;
                                    }
                                }
                                drop(info);
                                if changed { node.bump(); }
                            }
                            Some("cdp_result") => {
                                let channel = value["channel"].as_str().and_then(|s| Uuid::parse_str(s).ok()).ok_or(())?;
                                let reply = match value["error"]["message"].as_str() {
                                    Some(error) => json!({"id": value["id"], "error": {"message": error}}),
                                    None => json!({"id": value["id"], "result": value["result"]}),
                                };
                                deliver(&node, |id, _| *id == channel, &reply.to_string());
                            }
                            Some("cdp_event") => {
                                let tab = value["tab"].as_u64().and_then(|t| u32::try_from(t).ok()).ok_or(())?;
                                let method = value["method"].as_str().ok_or(())?;
                                // Only shared tabs, and only page state for view-only grants.
                                let Some(control) = node.control(tab) else { continue };
                                if !control && !view_event(method) { continue; }
                                let only = value["channel"].as_str().and_then(|s| Uuid::parse_str(s).ok());
                                let event = json!({"method": method, "params": value["params"]}).to_string();
                                // Live views get page state (and the passkey hook's calls); CDP clients everything.
                                let viewed = view_event(method) || method == "Runtime.bindingCalled";
                                deliver(&node, |id, c| c.tab == tab && only.is_none_or(|o| o == *id) && (viewed || c.automation), &event);
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

#[derive(Deserialize)]
pub struct Since {
    since: Option<u64>,
}

/// Long poll: answers at once when the browser's tabs changed since `since`
/// (its revision), else when they change or after 25 s. Live views reload their
/// tab list then, instead of finding out on the next round.
pub async fn changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<Since>,
) -> Result<Response, ApiError> {
    use std::sync::atomic::Ordering;
    state.check_auth(&headers)?;
    let node = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Browser disconnected"))?;
    let current = node.revision.load(Ordering::SeqCst);
    if query.since == Some(current) {
        // Created before checking again: a change in between still wakes it.
        let changed = node.changed.notified();
        if node.revision.load(Ordering::SeqCst) == current {
            let _ = tokio::time::timeout(Duration::from_secs(25), changed).await;
        }
    }
    let revision = node.revision.load(Ordering::SeqCst);
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"revision": revision})),
    )
        .into_response())
}

/// A view that cannot keep up is closed rather than silently missing frames:
/// the screencast waits for acknowledgements and would otherwise stall.
fn deliver(node: &Node, mut to: impl FnMut(&Uuid, &Channel) -> bool, text: &str) {
    node.channels
        .lock()
        .unwrap()
        .retain(|id, c| !to(id, c) || c.tx.try_send(text.to_string()).is_ok());
}

/// The handoff view's protocol socket for one shared tab. It speaks CDP; the
/// extension answers from the tab's debugger (Chromium) or emulates the subset
/// the view needs (Firefox). Methods are checked here and again in the browser.
pub async fn cdp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tab)): Path<(Uuid, u32)>,
    ws: WebSocketUpgrade,
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
    if node.deadline.is_some_and(|d| d <= Instant::now()) {
        return Err(failure(StatusCode::GONE, "Browser grant expired"));
    }
    // Older extensions do not know the relay and would drop the connection.
    if node.info.lock().unwrap().version.is_empty() {
        return Err(failure(
            StatusCode::UPGRADE_REQUIRED,
            "Update the browser extension (restart the browser) for live views",
        ));
    }
    if node.control(tab).is_none() {
        return Err(failure(StatusCode::FORBIDDEN, "Tab is not shared"));
    }
    if node.channels.lock().unwrap().len() >= MAX_CHANNELS {
        return Err(failure(StatusCode::TOO_MANY_REQUESTS, "Too many viewers"));
    }
    Ok(ws
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |mut ws| async move {
            relay(&node, tab, &mut ws).await;
            let _ = ws.close().await;
        }))
}

async fn relay(node: &Arc<Node>, tab: u32, ws: &mut WebSocket) {
    let channel = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<String>(256);
    node.channels.lock().unwrap().insert(
        channel,
        Channel {
            tab,
            tx,
            client: true,
            automation: false,
        },
    );
    node.notify_client_presence();
    struct Open(Arc<Node>, Uuid, u32);
    impl Drop for Open {
        fn drop(&mut self) {
            self.0.channels.lock().unwrap().remove(&self.1);
            self.0.notify_client_presence();
            let _ = self
                .0
                .outbound
                .try_send(json!({"type": "cdp_close", "channel": self.1, "tab": self.2}));
        }
    }
    let _open = Open(node.clone(), channel, tab);
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(text) = out else { break };
                if ws.send(Message::Text(text.into())).await.is_err() { break; }
            }
            incoming = ws.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                let Ok(request) = serde_json::from_str::<Value>(&text) else { break };
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default();
                let params = if request["params"].is_null() { json!({}) } else { request["params"].clone() };
                if !id.is_u64() { break; }
                if node.deadline.is_some_and(|d| d <= Instant::now()) { break; }
                let Some(control) = node.control(tab) else { break };
                if !relay_allowed(method, &params, control) {
                    let reply = json!({"id": id, "error": {"message": "Not permitted for this shared tab"}});
                    if ws.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                    continue;
                }
                let forward = json!({"type": "cdp", "channel": channel, "tab": tab, "id": id, "method": method, "params": params});
                match tokio::time::timeout(Duration::from_secs(5), node.outbound.send(forward)).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
        }
    }
}

/// Record a shared browser like a session: its own relay channel runs a
/// screencast of the shared tab in front (screenshots while the browser paints
/// none, e.g. a minimized window). Stop, pause and resume use the session API.
pub async fn start_recording(state: &AppState, id: Uuid) -> Result<Json<Value>, ApiError> {
    let node = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Session not found"))?;
    if node.info.lock().unwrap().version.is_empty() {
        return Err(failure(
            StatusCode::UPGRADE_REQUIRED,
            "Update the browser extension (restart the browser) to record it",
        ));
    }
    let mut recordings = state.active_recordings.write().await;
    if let Some(running) = recordings.get(&id) {
        return Ok(Json(
            json!({"status": "already_recording", "session_id": id, "recording_id": running.recording_id}),
        ));
    }
    let handle = crate::recorder::start_recording(
        id,
        None,
        None,
        state.node_name.clone(),
        (*state.recorder_config).clone(),
        None,
        None,
    );
    let recording_id = handle.recording_id;
    tokio::spawn(feed(state.clone(), id, node, handle.clone()));
    recordings.insert(id, handle);
    Ok(Json(
        json!({"status": "recording_started", "session_id": id, "recording_id": recording_id}),
    ))
}

const RECORD_SHOT: u64 = 1 << 40;

async fn feed(
    state: AppState,
    id: Uuid,
    node: Arc<Node>,
    handle: crate::recorder::SessionRecorderHandle,
) {
    use std::sync::atomic::Ordering;
    let channel = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let mut tab: Option<u32> = None;
    let mut next = 1u64;
    let mut last_frame = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(750));
    let request = |tab: u32, id: u64, method: &str, params: Value| {
        let _ = node
            .outbound
            .try_send(json!({"type": "cdp", "channel": channel, "tab": tab,
            "id": id, "method": method, "params": params}));
    };
    let screencast = json!({"format": "jpeg", "quality": 60, "maxWidth": 1600, "maxHeight": 1000});
    while !handle.stop_flag.load(Ordering::SeqCst) {
        tokio::select! {
            _ = tick.tick() => {
                let current = state.extensions.nodes.lock().unwrap().get(&id).is_some_and(|n| Arc::ptr_eq(n, &node));
                if !current { break; }
                let want = {
                    let info = node.info.lock().unwrap();
                    info.tabs.iter().find(|t| t.active).or(info.tabs.first()).map(|t| t.id)
                };
                let registered = node.channels.lock().unwrap().contains_key(&channel);
                if want != tab || !registered {
                    if let Some(old) = tab.filter(|_| want != tab) {
                        node.channels.lock().unwrap().remove(&channel);
                        let _ = node.outbound.try_send(json!({"type": "cdp_close", "channel": channel, "tab": old}));
                    }
                    tab = want;
                    if let Some(t) = tab {
                        node.channels.lock().unwrap().insert(channel, Channel { tab: t, tx: tx.clone(), client: false, automation: false });
                        request(t, next, "Page.enable", json!({}));
                        request(t, next + 1, "Page.startScreencast", screencast.clone());
                        next += 2;
                        last_frame = Instant::now();
                    }
                } else if let Some(t) = tab {
                    if last_frame.elapsed() > Duration::from_millis(1500) {
                        request(t, RECORD_SHOT + next, "Page.captureScreenshot", json!({"format": "jpeg", "quality": 60}));
                        next += 1;
                    }
                }
            }
            Some(text) = rx.recv() => {
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                let Some(t) = tab else { continue };
                // An extension's debugger stops sending frames when the page
                // reloads (the screencast stays "active"): restart it.
                if message["method"] == "Page.loadEventFired" {
                    request(t, next, "Page.stopScreencast", json!({}));
                    request(t, next + 1, "Page.startScreencast", screencast.clone());
                    next += 2;
                    continue;
                }
                let data = if message["method"] == "Page.screencastFrame" {
                    request(t, next, "Page.screencastFrameAck", json!({"sessionId": message["params"]["sessionId"]}));
                    next += 1;
                    message["params"]["data"].as_str()
                } else if message["id"].as_u64().is_some_and(|i| i >= RECORD_SHOT) {
                    message["result"]["data"].as_str()
                } else {
                    None
                };
                let Some(data) = data else { continue };
                last_frame = Instant::now();
                let (url, title) = {
                    let info = node.info.lock().unwrap();
                    let shown = info.tabs.iter().find(|x| x.id == t);
                    (shown.map(|x| x.url.clone()), shown.map(|x| x.title.clone()))
                };
                let frame = crate::recorder::ProxyRecordingFrame { url, title, data: data.to_string() };
                if handle.frame_tx.send(frame).is_err() { break; }
            }
        }
    }
    if node.channels.lock().unwrap().remove(&channel).is_some() {
        if let Some(t) = tab {
            let _ = node
                .outbound
                .try_send(json!({"type": "cdp_close", "channel": channel, "tab": t}));
        }
    }
    // The browser went away: the recording ends with it.
    handle.stop();
    let mut recordings = state.active_recordings.write().await;
    if recordings
        .get(&id)
        .is_some_and(|h| h.recording_id == handle.recording_id)
    {
        recordings.remove(&id);
    }
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

pub async fn pairing_javascript() -> impl IntoResponse {
    (
        [
            ("content-type", "text/javascript"),
            ("cache-control", "no-cache"),
        ],
        include_str!("../assets/pairing.js"),
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
                remember: false,
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
                        let peers = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
                        let _ = serve(&broker, &[], &peers, &origin, &mut ws).await;
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
    fn offline_remembered_browsers_are_forgotten_after_the_timeout() {
        let dir = std::env::temp_dir().join(format!("browser-keys-{}", Uuid::new_v4()));
        let path = dir.join("browser-keys.json");
        let now = auth::now();
        let record = |last_seen| Remembered {
            id: Uuid::new_v4(),
            origin: "chrome-extension://abc".into(),
            name: "laptop".into(),
            browser: "chromium".into(),
            key_digest: key_digest("k"),
            last_seen,
        };
        let (fresh, stale, legacy) = (record(now - 3600), record(now - 20 * 86400), record(0));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&[&fresh, &stale, &legacy]).unwrap(),
        )
        .unwrap();
        let broker = Broker::load(&path, Some(Duration::from_secs(14 * 86400)));
        broker.sweep();
        let ids: Vec<Uuid> = broker
            .credentials
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        // Records from before the timestamp count from the daemon's start.
        assert_eq!(ids, vec![fresh.id, legacy.id]);
        let saved: Vec<Remembered> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.len(), 2);
        let never = Broker::load(&path, None);
        never.credentials.lock().unwrap()[0].last_seen = 1;
        never.sweep();
        assert_eq!(never.credentials.lock().unwrap().len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_views_see_frames_and_steer_only_under_control() {
        let none = json!({});
        for method in [
            "Page.startScreencast",
            "Page.screencastFrameAck",
            "Page.getNavigationHistory",
        ] {
            assert!(relay_allowed(method, &none, false), "{method}");
        }
        for method in [
            "Input.dispatchMouseEvent",
            "Runtime.evaluate",
            "Page.reload",
            "Page.handleJavaScriptDialog",
        ] {
            assert!(!relay_allowed(method, &none, false), "{method}");
            assert!(relay_allowed(method, &none, true), "{method}");
        }
        for method in [
            "Target.createTarget",
            "Browser.close",
            "Network.getAllCookies",
            "Storage.getCookies",
            "Emulation.setDeviceMetricsOverride",
            "Page.addScriptToEvaluateOnNewDocument",
            "DOM.setFileInputFiles",
            "Input.dispatchDragEvent",
        ] {
            assert!(!relay_allowed(method, &none, true), "{method}");
        }
        assert!(relay_allowed(
            "Page.navigate",
            &json!({"url": "https://example.org/"}),
            true
        ));
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
        ] {
            assert!(
                !relay_allowed("Page.navigate", &json!({"url": url}), true),
                "{url}"
            );
        }
        assert!(!relay_allowed(
            "Page.navigate",
            &json!({"url": "https://example.org/"}),
            false
        ));
        assert!(!relay_allowed("Page.enable", &json!([]), true));
        assert!(view_event("Page.screencastFrame") && !view_event("Runtime.consoleAPICalled"));
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
