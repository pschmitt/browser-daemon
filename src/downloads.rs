//! Files the pages of a session download: the browser saves them on its node
//! (`<state dir>/downloads/<session>/`), the daemon keeps a list of them, tells the
//! session's live views as they go, and serves them (`/api/sessions/{id}/downloads`),
//! so a download started in the handoff view ends up on the viewer's device.
//!
//! Chromium: the daemon holds a browser-level DevTools connection per session and
//! sets the default context's download behaviour (`Browser.setDownloadBehavior`,
//! files named by their guid, events on). Chromium keeps sending that connection
//! the download events whoever set the behaviour last, `filePath` included. A
//! CDP client that sets its own (Playwright does) keeps it while it is connected;
//! when it leaves, Chromium falls back to its default and the daemon sets its own
//! again. Firefox: BiDi `browser.setDownloadBehavior` and the
//! `browsingContext.downloadWillBegin` / `downloadEnd` events, through the
//! session's BiDi hub (no progress events there).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::fs;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::api::{api_error, ApiError};
use crate::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    InProgress,
    Completed,
    Canceled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    /// Chromium's guid, Firefox's navigation id.
    pub id: String,
    /// The name the page suggested, safe as a file name.
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub received_bytes: u64,
    pub state: DownloadState,
    pub started_at_secs: u64,
    #[serde(default)]
    pub finished_at_secs: Option<u64>,
    /// Where the browser put it; not shown to clients.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

impl Download {
    /// What clients see.
    pub fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("path");
        }
        v
    }
}

/// One session's downloads.
pub struct SessionDownloads {
    pub session: Uuid,
    pub dir: PathBuf,
    items: StdMutex<Vec<Download>>,
    /// When each running download last told the views, to thin out progress.
    told: StdMutex<HashMap<String, Instant>>,
    /// Automation clients connected now that set their own download behaviour.
    claims: AtomicUsize,
    /// A client that had set its own left: the daemon sets its behaviour again.
    reapply: tokio::sync::Notify,
}

impl SessionDownloads {
    pub fn list(&self) -> Vec<Download> {
        let mut v = self.items.lock().unwrap().clone();
        v.sort_by_key(|d| std::cmp::Reverse(d.started_at_secs));
        v
    }

    pub fn get(&self, id: &str) -> Option<Download> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    fn claimed(&self) -> bool {
        self.claims.load(Ordering::SeqCst) > 0
    }

    /// A download began; returns it as the views should see it.
    fn begin(&self, id: &str, name: &str, url: &str) -> Download {
        let dl = Download {
            id: id.to_string(),
            name: safe_name(name),
            url: url.to_string(),
            total_bytes: None,
            received_bytes: 0,
            state: DownloadState::InProgress,
            started_at_secs: now_secs(),
            finished_at_secs: None,
            path: None,
        };
        let mut items = self.items.lock().unwrap();
        items.retain(|d| d.id != id);
        items.push(dl.clone());
        dl
    }

    /// Progress of a download; the updated download when the views should hear of
    /// it (state changes always, running progress at most twice a second).
    fn progress(&self, p: Progress) -> Option<Download> {
        let mut items = self.items.lock().unwrap();
        let dl = items.iter_mut().find(|d| d.id == p.id)?;
        let changed = dl.state != p.state;
        dl.received_bytes = p.received.unwrap_or(dl.received_bytes);
        if p.total.is_some_and(|t| t > 0) {
            dl.total_bytes = p.total;
        }
        dl.state = p.state;
        if p.state != DownloadState::InProgress {
            dl.finished_at_secs = Some(now_secs());
            dl.path = p.path.or_else(|| {
                let guess = self.dir.join(&dl.id);
                guess.is_file().then_some(guess)
            });
            if p.state == DownloadState::Completed && p.received.is_none() {
                // Firefox says nothing about sizes: the file does.
                if let Some(len) = dl.path.as_ref().and_then(|f| std::fs::metadata(f).ok()) {
                    dl.received_bytes = len.len();
                    dl.total_bytes = Some(len.len());
                }
            }
        }
        let dl = dl.clone();
        drop(items);
        let mut told = self.told.lock().unwrap();
        let due = told
            .get(&dl.id)
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
        if changed || due {
            told.insert(dl.id.clone(), Instant::now());
            if dl.state != DownloadState::InProgress {
                told.remove(&dl.id);
            }
            Some(dl)
        } else {
            None
        }
    }

    /// Forget a download; its file goes too when it is in the session's directory.
    async fn remove(&self, id: &str) -> Option<Download> {
        let dl = {
            let mut items = self.items.lock().unwrap();
            let i = items.iter().position(|d| d.id == id)?;
            items.remove(i)
        };
        if let Some(ref p) = dl.path {
            if p.starts_with(&self.dir) {
                let _ = fs::remove_file(p).await;
            }
        }
        self.save().await;
        Some(dl)
    }

    /// The list on disk: a restarted daemon still knows the downloads of the
    /// sessions it adopts.
    async fn save(&self) {
        let json = serde_json::to_vec(&*self.items.lock().unwrap()).unwrap_or_default();
        let tmp = self.dir.join(format!(".index.{}.tmp", Uuid::new_v4()));
        if fs::write(&tmp, json).await.is_ok() {
            let _ = fs::rename(&tmp, self.dir.join("index.json")).await;
        }
    }
}

/// What a progress event says.
#[derive(Debug, PartialEq)]
struct Progress {
    id: String,
    state: DownloadState,
    received: Option<u64>,
    total: Option<u64>,
    path: Option<PathBuf>,
}

/// Every session's downloads.
#[derive(Default)]
pub struct Downloads {
    sessions: StdMutex<HashMap<Uuid, Arc<SessionDownloads>>>,
}

/// `<state dir>/downloads`: on disk rather than in /tmp (a tmpfs on these hosts,
/// i.e. memory), and the same path for browsers adopted across a restart.
pub fn root(state_dir: &Path) -> PathBuf {
    state_dir.join("downloads")
}

impl Downloads {
    pub fn get(&self, session: &Uuid) -> Option<Arc<SessionDownloads>> {
        self.sessions.lock().unwrap().get(session).cloned()
    }

    /// The session's downloads, set up (directory, the list a previous run left)
    /// on first use.
    pub async fn session(&self, state_dir: &Path, session: Uuid) -> Arc<SessionDownloads> {
        if let Some(sd) = self.get(&session) {
            return sd;
        }
        let root = root(state_dir);
        let dir = root.join(session.to_string());
        let _ = fs::create_dir_all(&dir).await;
        for d in [&root, &dir] {
            let _ = fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).await;
        }
        let items: Vec<Download> = fs::read(dir.join("index.json"))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let fresh = Arc::new(SessionDownloads {
            session,
            dir,
            items: StdMutex::new(items),
            told: StdMutex::default(),
            claims: AtomicUsize::new(0),
            reapply: tokio::sync::Notify::new(),
        });
        self.sessions
            .lock()
            .unwrap()
            .entry(session)
            .or_insert(fresh)
            .clone()
    }

    /// The session ended: its downloads go with it.
    pub async fn forget(&self, state_dir: &Path, session: &Uuid) {
        self.sessions.lock().unwrap().remove(session);
        let _ = fs::remove_dir_all(root(state_dir).join(session.to_string())).await;
    }
}

/// Download directories of sessions that no longer exist (a crash, a session of
/// a run that was not adopted): removed at startup.
pub async fn sweep(state_dir: &Path, live: &std::collections::HashSet<Uuid>) -> usize {
    let mut removed = 0;
    let Ok(mut entries) = fs::read_dir(root(state_dir)).await else {
        return 0;
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let stale = e
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<Uuid>().ok())
            .is_some_and(|id| !live.contains(&id));
        if stale && fs::remove_dir_all(e.path()).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Held by a connected automation client: once it sets its own download
/// behaviour, the daemon leaves it alone until the client goes.
pub struct Claim {
    sd: Arc<SessionDownloads>,
    claimed: bool,
}

impl Claim {
    pub fn new(sd: Arc<SessionDownloads>) -> Self {
        Self { sd, claimed: false }
    }

    /// Look at a command the client sends.
    pub fn note(&mut self, text: &str) {
        if !self.claimed && sets_download_behavior(text) {
            self.claimed = true;
            self.sd.claims.fetch_add(1, Ordering::SeqCst);
            debug!(session_id = %self.sd.session, "A client set its own download behaviour");
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.claimed {
            self.sd.claims.fetch_sub(1, Ordering::SeqCst);
            // Chromium went back to its default when the client left.
            self.sd.reapply.notify_one();
        }
    }
}

/// A CDP or BiDi command that sets where downloads go.
fn sets_download_behavior(text: &str) -> bool {
    if !text.contains("etDownloadBehavior") {
        return false;
    }
    serde_json::from_str::<Value>(text).is_ok_and(|v| {
        matches!(
            v["method"].as_str(),
            Some(
                "Browser.setDownloadBehavior"
                    | "Page.setDownloadBehavior"
                    | "browser.setDownloadBehavior"
            )
        )
    })
}

/// A file name that is safe as one path component and readable.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .chars()
        .take(200)
        .collect();
    if clean.is_empty() {
        "download".to_string()
    } else {
        clean
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `Content-Disposition` for a download: a plain ASCII name plus the real one.
fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Tell the session's live views about a download.
fn tell(state: &AppState, sd: &SessionDownloads, dl: &Download, removed: bool) {
    let mut v = dl.public();
    if removed {
        v["removed"] = json!(true);
    }
    state
        .viewers
        .send(sd.session, json!({ "download": v }).to_string());
}

// ---------------------------------------------------------------- watching

/// Follow the downloads of a session's browser for as long as the session runs.
pub fn watch(state: AppState, session: &crate::ManagedSession) {
    let (id, port, hub) = (session.id, session.port, session.bidi.clone());
    tokio::spawn(async move {
        let sd = state.downloads.session(state.config.state_dir(), id).await;
        match hub {
            Some(hub) => watch_firefox(&state, &sd, hub).await,
            None => watch_chromium(&state, &sd, port).await,
        }
        // Gone meanwhile: nothing of it may stay behind (the session's end may
        // have come before this set its directory up).
        if !session_alive(&state, &id).await {
            state.downloads.forget(state.config.state_dir(), &id).await;
        }
        debug!(session_id = %id, "Stopped following downloads");
    });
}

async fn session_alive(state: &AppState, id: &Uuid) -> bool {
    state.sessions.read().await.contains_key(id)
}

async fn watch_chromium(state: &AppState, sd: &Arc<SessionDownloads>, port: u16) {
    let mut pause = Duration::from_millis(500);
    while session_alive(state, &sd.session).await {
        if let Some(ws) = browser_socket(port).await {
            pause = Duration::from_millis(500);
            follow_chromium(state, sd, ws).await;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(10));
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The browser's own DevTools socket (not a page's).
async fn browser_socket(port: u16) -> Option<Socket> {
    let v: Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let url = v["webSocketDebuggerUrl"].as_str()?;
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .ok()?
    .ok()?;
    Some(ws)
}

fn chromium_behavior(dir: &Path) -> String {
    json!({
        "id": 1,
        "method": "Browser.setDownloadBehavior",
        "params": {
            "behavior": "allowAndName",
            "downloadPath": dir,
            "eventsEnabled": true,
        },
    })
    .to_string()
}

async fn follow_chromium(state: &AppState, sd: &Arc<SessionDownloads>, ws: Socket) {
    let (mut tx, mut rx) = ws.split();
    // Not over a client's own: without ours the events stay off until it leaves.
    if !sd.claimed() {
        let _ = tx
            .send(Message::Text(chromium_behavior(&sd.dir).into()))
            .await;
    }
    loop {
        tokio::select! {
            () = sd.reapply.notified() => {
                if !sd.claimed() {
                    debug!(session_id = %sd.session, "Download behaviour back to the daemon's");
                    if tx.send(Message::Text(chromium_behavior(&sd.dir).into())).await.is_err() {
                        return;
                    }
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { return };
                let Message::Text(text) = msg else { continue };
                if text.contains("Browser.download") {
                    on_chromium_event(state, sd, text.as_str()).await;
                }
            }
        }
    }
}

/// A `Browser.downloadWillBegin` / `downloadProgress` event.
enum Event {
    Begin {
        id: String,
        name: String,
        url: String,
    },
    Progress(Progress),
}

fn parse_chromium(text: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(text).ok()?;
    let p = &v["params"];
    let id = p["guid"].as_str()?.to_string();
    match v["method"].as_str()? {
        "Browser.downloadWillBegin" => Some(Event::Begin {
            id,
            name: p["suggestedFilename"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            url: p["url"].as_str().unwrap_or_default().to_string(),
        }),
        "Browser.downloadProgress" => Some(Event::Progress(Progress {
            id,
            state: match p["state"].as_str()? {
                "completed" => DownloadState::Completed,
                "canceled" => DownloadState::Canceled,
                _ => DownloadState::InProgress,
            },
            received: p["receivedBytes"].as_f64().map(|n| n as u64),
            total: p["totalBytes"].as_f64().map(|n| n as u64),
            path: p["filePath"].as_str().map(PathBuf::from),
        })),
        _ => None,
    }
}

async fn on_chromium_event(state: &AppState, sd: &Arc<SessionDownloads>, text: &str) {
    match parse_chromium(text) {
        Some(Event::Begin { id, name, url }) => began(state, sd, &id, &name, &url).await,
        Some(Event::Progress(p)) => progressed(state, sd, p).await,
        None => {}
    }
}

async fn began(state: &AppState, sd: &Arc<SessionDownloads>, id: &str, name: &str, url: &str) {
    let dl = sd.begin(id, name, url);
    info!(session_id = %sd.session, download = %dl.name, "Download started");
    sd.save().await;
    tell(state, sd, &dl, false);
}

async fn progressed(state: &AppState, sd: &Arc<SessionDownloads>, p: Progress) {
    let finished = p.state != DownloadState::InProgress;
    if let Some(dl) = sd.progress(p) {
        if finished {
            info!(session_id = %sd.session, download = %dl.name, state = ?dl.state, "Download finished");
            sd.save().await;
        }
        tell(state, sd, &dl, false);
    }
}

const FIREFOX_EVENTS: [&str; 2] = [
    "browsingContext.downloadWillBegin",
    "browsingContext.downloadEnd",
];

async fn firefox_behavior(hub: &crate::bidi::BidiHub, dir: &Path) {
    let params = json!({
        "downloadBehavior": { "type": "allowed", "destinationFolder": dir },
    });
    if let Err(e) = hub.call("browser.setDownloadBehavior", params).await {
        warn!(error = %e, "Firefox did not take the download behaviour");
    }
}

async fn watch_firefox(
    state: &AppState,
    sd: &Arc<SessionDownloads>,
    hub: Arc<crate::bidi::BidiHub>,
) {
    let mut events = hub.listen(&FIREFOX_EVENTS).await;
    if !sd.claimed() {
        firefox_behavior(&hub, &sd.dir).await;
    }
    loop {
        tokio::select! {
            () = sd.reapply.notified() => {
                if !sd.claimed() {
                    firefox_behavior(&hub, &sd.dir).await;
                }
            }
            msg = events.recv() => {
                let Some(text) = msg else { return };
                match parse_firefox(&text) {
                    Some(Event::Begin { id, name, url }) => began(state, sd, &id, &name, &url).await,
                    Some(Event::Progress(p)) => progressed(state, sd, p).await,
                    None => {}
                }
            }
        }
        if hub.is_closed() {
            return;
        }
    }
}

fn parse_firefox(text: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(text).ok()?;
    let p = &v["params"];
    let id = p["navigation"].as_str()?.to_string();
    match v["method"].as_str()? {
        "browsingContext.downloadWillBegin" => Some(Event::Begin {
            id,
            name: p["suggestedFilename"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            url: p["url"].as_str().unwrap_or_default().to_string(),
        }),
        "browsingContext.downloadEnd" => Some(Event::Progress(Progress {
            id,
            state: if p["status"].as_str() == Some("complete") {
                DownloadState::Completed
            } else {
                DownloadState::Canceled
            },
            received: None,
            total: None,
            path: p["filepath"].as_str().map(PathBuf::from),
        })),
        _ => None,
    }
}

// ---------------------------------------------------------------- API

async fn session_downloads(state: &AppState, id: &Uuid) -> Result<Arc<SessionDownloads>, ApiError> {
    if !session_alive(state, id).await {
        return Err(api_error(StatusCode::NOT_FOUND, "Session not found"));
    }
    Ok(state.downloads.session(state.config.state_dir(), *id).await)
}

/// `GET /api/sessions/{id}/downloads`: newest first.
pub(crate) async fn api_list(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let sd = session_downloads(&state, &id).await?;
    Ok(Json(sd.list().iter().map(Download::public).collect()))
}

/// `GET /api/sessions/{id}/downloads/{dl}`: the file, as an attachment.
pub(crate) async fn api_get(
    AxumPath((id, dl)): AxumPath<(Uuid, String)>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let sd = match session_downloads(&state, &id).await {
        Ok(sd) => sd,
        Err(e) => return e.into_response(),
    };
    let Some(d) = sd.get(&dl) else {
        return api_error(StatusCode::NOT_FOUND, "No such download").into_response();
    };
    if d.state != DownloadState::Completed {
        return api_error(StatusCode::CONFLICT, "The download has not finished").into_response();
    }
    let Some(path) = d.path.filter(|p| p.is_file()) else {
        return api_error(StatusCode::GONE, "The file is gone").into_response();
    };
    match tower_http::services::ServeFile::new(&path)
        .try_call(req)
        .await
    {
        Ok(res) => {
            let mut res = res.map(axum::body::Body::new);
            let h = res.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(header::CONTENT_DISPOSITION, content_disposition(&d.name));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `DELETE /api/sessions/{id}/downloads/{dl}`.
pub(crate) async fn api_delete(
    AxumPath((id, dl)): AxumPath<(Uuid, String)>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let sd = session_downloads(&state, &id).await?;
    let d = sd
        .remove(&dl)
        .await
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such download"))?;
    tell(&state, &sd, &d, true);
    Ok(Json(json!({ "deleted": d.id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sd(dir: &Path) -> SessionDownloads {
        SessionDownloads {
            session: Uuid::new_v4(),
            dir: dir.to_path_buf(),
            items: StdMutex::default(),
            told: StdMutex::default(),
            claims: AtomicUsize::new(0),
            reapply: tokio::sync::Notify::new(),
        }
    }

    #[test]
    fn chromium_events_become_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let s = sd(dir.path());
        let Some(Event::Begin { id, name, url }) = parse_chromium(
            r#"{"method":"Browser.downloadWillBegin","params":{"frameId":"f","guid":"g1","url":"https://a.test/x.pdf","suggestedFilename":"../x.pdf"}}"#,
        ) else {
            panic!("no begin");
        };
        let dl = s.begin(&id, &name, &url);
        assert_eq!(dl.name, "x.pdf", "only the last path component");
        let progress = |state: &str, recv: u64| {
            let Some(Event::Progress(p)) = parse_chromium(&format!(
                r#"{{"method":"Browser.downloadProgress","params":{{"guid":"g1","totalBytes":10,"receivedBytes":{recv},"state":"{state}"{}}}}}"#,
                if state == "completed" {
                    r#","filePath":"/elsewhere/g1""#
                } else {
                    ""
                }
            )) else {
                panic!("no progress");
            };
            p
        };
        assert!(
            s.progress(progress("inProgress", 3)).is_some(),
            "first one is told"
        );
        assert!(
            s.progress(progress("inProgress", 5)).is_none(),
            "thinned out"
        );
        let done = s
            .progress(progress("completed", 10))
            .expect("state changes are told");
        assert_eq!(done.state, DownloadState::Completed);
        assert_eq!(done.received_bytes, 10);
        assert_eq!(done.path.as_deref(), Some(Path::new("/elsewhere/g1")));
        assert!(done.public().get("path").is_none(), "no paths for clients");
    }

    #[test]
    fn firefox_events_become_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let s = sd(dir.path());
        let Some(Event::Begin { id, name, url }) = parse_firefox(
            r#"{"type":"event","method":"browsingContext.downloadWillBegin","params":{"context":"c","navigation":"n1","timestamp":1,"url":"https://a.test/f","suggestedFilename":"f.txt"}}"#,
        ) else {
            panic!("no begin");
        };
        s.begin(&id, &name, &url);
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"hello").unwrap();
        let Some(Event::Progress(p)) = parse_firefox(&format!(
            r#"{{"type":"event","method":"browsingContext.downloadEnd","params":{{"context":"c","navigation":"n1","status":"complete","filepath":"{}"}}}}"#,
            file.display()
        )) else {
            panic!("no end");
        };
        let done = s.progress(p).unwrap();
        assert_eq!(done.received_bytes, 5, "size from the file");
        assert_eq!(done.state, DownloadState::Completed);
    }

    #[test]
    fn a_client_that_sets_its_own_behaviour_holds_it_until_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(sd(dir.path()));
        let mut c = Claim::new(s.clone());
        c.note(r#"{"id":1,"method":"Page.navigate","params":{"url":"https://setDownloadBehavior.test"}}"#);
        assert!(!s.claimed(), "only the command itself counts");
        c.note(r#"{"id":2,"method":"Browser.setDownloadBehavior","params":{"behavior":"allow"}}"#);
        c.note(r#"{"id":3,"method":"Browser.setDownloadBehavior","params":{"behavior":"deny"}}"#);
        assert_eq!(s.claims.load(Ordering::SeqCst), 1, "one claim per client");
        drop(c);
        assert!(!s.claimed());
        assert!(sets_download_behavior(
            r#"{"id":4,"method":"browser.setDownloadBehavior","params":{}}"#
        ));
    }

    #[test]
    fn content_disposition_carries_any_name_safely() {
        let h = content_disposition("Résumé \"final\".pdf");
        let s = h.to_str().unwrap();
        assert!(
            s.starts_with("attachment; filename=\"R_sum_ _final_.pdf\""),
            "{s}"
        );
        assert!(
            s.contains("filename*=UTF-8''R%C3%A9sum%C3%A9%20%22final%22.pdf"),
            "{s}"
        );
        assert_eq!(safe_name("..."), "download");
    }

    #[tokio::test]
    async fn the_list_survives_a_restart_and_the_sweep_spares_live_sessions() {
        let state_dir = tempfile::tempdir().unwrap();
        let (live, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let all = Downloads::default();
        let s = all.session(state_dir.path(), live).await;
        s.begin("g", "a.bin", "https://a.test/a.bin");
        s.save().await;
        all.session(state_dir.path(), gone).await;
        let mode = std::fs::metadata(&s.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let after_restart = Downloads::default();
        let again = after_restart.session(state_dir.path(), live).await;
        assert_eq!(again.list().len(), 1);

        let keep: std::collections::HashSet<Uuid> = [live].into();
        assert_eq!(sweep(state_dir.path(), &keep).await, 1);
        assert!(again.dir.is_dir());
    }
}
