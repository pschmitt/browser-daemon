//! Public links: a recording or a running session shown to anyone who has the link,
//! without signing in.
//!
//! A share is a long random token (64 bytes from the kernel's generator, 86
//! characters of base64url), kept with the node that holds what it shows, in
//! `<state dir>/shares.json`. Its link is `<share url>/share/<node>/<token>`; any
//! node of the mesh serves it and forwards the requests to the node named in it,
//! so one public name (a reverse proxy in front of any node) reaches them all.
//!
//! What a link gives is deliberately narrow: a recording's frames (with the
//! addresses cut down to their site, as an address may carry a login code), its
//! sound and icon; a session's front tab as pictures, watched, never controlled.
//! No cookie and no token comes with it, and nothing else of the daemon is
//! reachable through it. A session can be shared to be controlled too
//! (`control`): then the link also takes clicks, scrolling, a few named keys and
//! typed text for the tab in front (Chromium), sent as input events, never a
//! DevTools connection, an address bar or keyboard shortcuts. A share ends when it is removed ("unshared"), when it
//! expires, or when what it shows is gone (a stopped session, a deleted recording).

use super::*;

use base64::Engine as _;
use std::io::Read as _;

/// The share page, the same for every share: it reads the link from its address.
pub(crate) static SHARE_HTML: &str = include_str!("../share.html");

/// Requests a node forwards to the node of a share carry this, so they are never
/// forwarded again (a node that does not answer to the name in the link).
const FORWARDED: &str = "x-browser-share-forwarded";

/// Random bytes in a token.
const TOKEN_BYTES: usize = 64;

/// How long a live picture of a shared session is shown to every viewer before a
/// new one is taken: many viewers cost one capture.
const LIVE_FRESH: Duration = Duration::from_millis(700);
/// Width of the live pictures.
const LIVE_WIDTH: f64 = 1600.0;
/// A share page that fetched no picture for this long is no longer counted.
const WATCHER_GONE: Duration = Duration::from_secs(6);
/// The header a share page names itself with (a random id per page).
const VIEWER: &str = "x-share-viewer";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    Recording,
    Session,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Share {
    pub id: Uuid,
    pub token: String,
    pub kind: ShareKind,
    /// The recording's or the session's id.
    pub target: Uuid,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// How often the link was opened.
    #[serde(default)]
    pub views: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<u64>,
    /// A session's viewers may also control it (click, scroll, type); view only
    /// otherwise.
    #[serde(default)]
    pub control: bool,
}

impl Share {
    fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

/// A share as the API shows it: with its node and link.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ShareView {
    #[serde(flatten)]
    pub share: Share,
    pub node: String,
    /// `/share/<node>/<token>`.
    pub path: String,
    /// The whole link, on the node's share URL.
    pub url: String,
    /// A session's: how many share pages watch it now.
    #[serde(default)]
    pub watching: usize,
}

/// A new token: 64 random bytes as base64url.
fn new_token() -> std::io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Whether a link's token has the shape of one: nothing else is looked up.
fn token_shape(token: &str) -> bool {
    token.len() == 86
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A live picture of a shared session's front tab.
pub struct LiveFrame {
    jpeg: Vec<u8>,
    url: Option<String>,
    title: Option<String>,
}

type LiveSlot = Arc<Mutex<Option<(Instant, Arc<LiveFrame>)>>>;

pub struct Store {
    path: PathBuf,
    shares: std::sync::RwLock<HashMap<Uuid, Share>>,
    /// One change at a time: read, modify and save are a unit.
    write: Mutex<()>,
    /// The latest live picture per shared session.
    live: std::sync::Mutex<HashMap<Uuid, LiveSlot>>,
    /// Who watches a shared session through its link: each share page's own
    /// random id (`X-Share-Viewer`) and when it last fetched a picture.
    watchers: std::sync::Mutex<HashMap<Uuid, HashMap<String, Instant>>>,
}

impl Store {
    /// The shares in `path`; none when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let shares: Vec<Share> = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Ignoring unreadable shares");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self {
            path: path.to_path_buf(),
            shares: std::sync::RwLock::new(shares.into_iter().map(|s| (s.id, s)).collect()),
            write: Mutex::new(()),
            live: std::sync::Mutex::default(),
            watchers: std::sync::Mutex::default(),
        }
    }

    pub fn all(&self) -> Vec<Share> {
        let mut all: Vec<Share> = self.shares.read().unwrap().values().cloned().collect();
        all.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        all
    }

    /// The share a link names, unless it expired.
    pub fn by_token(&self, token: &str) -> Option<Share> {
        if !token_shape(token) {
            return None;
        }
        let now = auth::now();
        self.shares
            .read()
            .unwrap()
            .values()
            .find(|s| s.token == token && !s.expired(now))
            .cloned()
    }

    fn for_target(&self, kind: ShareKind, target: Uuid) -> Option<Share> {
        self.shares
            .read()
            .unwrap()
            .values()
            .find(|s| s.kind == kind && s.target == target)
            .cloned()
    }

    /// Change the shares and save them; `change` says whether it changed anything.
    async fn update<T>(
        &self,
        change: impl FnOnce(&mut HashMap<Uuid, Share>) -> (bool, T),
    ) -> std::io::Result<T> {
        let _guard = self.write.lock().await;
        let mut shares = self.shares.read().unwrap().clone();
        let (changed, out) = change(&mut shares);
        if changed {
            let list: Vec<&Share> = shares.values().collect();
            persist::write_atomic(&self.path, &serde_json::to_vec_pretty(&list)?).await?;
            *self.shares.write().unwrap() = shares;
        }
        Ok(out)
    }

    /// Share `target`, or give its share a new expiry (and, for `Some`, mode):
    /// one link per target.
    pub async fn share(
        &self,
        kind: ShareKind,
        target: Uuid,
        expires_at: Option<u64>,
        control: Option<bool>,
    ) -> std::io::Result<Share> {
        let control = control.filter(|_| kind == ShareKind::Session);
        let token = new_token()?;
        self.update(|shares| {
            let now = auth::now();
            if let Some(s) = shares
                .values_mut()
                .find(|s| s.kind == kind && s.target == target && !s.expired(now))
            {
                let changed = s.expires_at != expires_at || control.is_some_and(|c| c != s.control);
                s.expires_at = expires_at;
                s.control = control.unwrap_or(s.control);
                return (changed, s.clone());
            }
            shares.retain(|_, s| !(s.kind == kind && s.target == target));
            let share = Share {
                id: Uuid::new_v4(),
                token,
                kind,
                target,
                created_at: now,
                expires_at,
                views: 0,
                last_viewed_at: None,
                control: control.unwrap_or(false),
            };
            shares.insert(share.id, share.clone());
            (true, share)
        })
        .await
    }

    /// Remove a share: the link stops working at once.
    pub async fn remove(&self, id: Uuid) -> std::io::Result<bool> {
        self.update(|shares| {
            let removed = shares.remove(&id).is_some();
            (removed, removed)
        })
        .await
    }

    /// Remove the share of something that is gone.
    pub async fn remove_target(&self, kind: ShareKind, target: Uuid) {
        if self.for_target(kind, target).is_none() {
            return;
        }
        if kind == ShareKind::Session {
            self.live.lock().unwrap().remove(&target);
            self.watchers.lock().unwrap().remove(&target);
        }
        if let Err(e) = self
            .update(|shares| {
                let before = shares.len();
                shares.retain(|_, s| !(s.kind == kind && s.target == target));
                (shares.len() != before, ())
            })
            .await
        {
            warn!(error = %e, "Could not save the shares");
        }
    }

    /// Remove the shares that expired or whose recording or session is gone.
    async fn prune(&self, alive: impl Fn(&Share) -> bool) {
        let now = auth::now();
        let dead: Vec<Uuid> = self
            .all()
            .into_iter()
            .filter(|s| s.expired(now) || !alive(s))
            .map(|s| s.id)
            .collect();
        if dead.is_empty() {
            return;
        }
        if let Err(e) = self
            .update(|shares| {
                shares.retain(|id, _| !dead.contains(id));
                (true, ())
            })
            .await
        {
            warn!(error = %e, "Could not save the shares");
        }
    }

    /// Count a visit (not saved right away: the next change or prune saves it).
    fn note_view(&self, id: Uuid) {
        if let Some(s) = self.shares.write().unwrap().get_mut(&id) {
            s.views += 1;
            s.last_viewed_at = Some(auth::now());
        }
    }

    /// A share page of `session` fetched a picture.
    fn note_watcher(&self, session: Uuid, viewer: &str) {
        let mut all = self.watchers.lock().unwrap();
        let seen = all.entry(session).or_default();
        seen.retain(|_, at| at.elapsed() < WATCHER_GONE);
        // A page sends at most this many ids before the count stops believing it.
        if seen.len() < 1000 || seen.contains_key(viewer) {
            seen.insert(viewer.to_string(), Instant::now());
        }
    }

    /// How many share pages watch `session` right now.
    pub fn watching(&self, session: Uuid) -> usize {
        self.watchers
            .lock()
            .unwrap()
            .get(&session)
            .map_or(0, |seen| {
                seen.values()
                    .filter(|at| at.elapsed() < WATCHER_GONE)
                    .count()
            })
    }

    fn live_slot(&self, session: Uuid) -> LiveSlot {
        self.live
            .lock()
            .unwrap()
            .entry(session)
            .or_default()
            .clone()
    }
}

impl AppState {
    pub(crate) fn share_view(&self, share: Share) -> ShareView {
        let path = format!("/share/{}/{}", self.node_name, share.token);
        ShareView {
            watching: match share.kind {
                ShareKind::Session => self.shares.watching(share.target),
                ShareKind::Recording => 0,
            },
            url: format!("{}{path}", self.share_url.trim_end_matches('/')),
            node: self.node_name.clone(),
            path,
            share,
        }
    }

    /// This node's shares, without the dead ones.
    pub(crate) async fn live_shares(&self) -> Vec<ShareView> {
        let sessions: std::collections::HashSet<Uuid> =
            self.sessions.read().await.keys().copied().collect();
        let mut recordings = std::collections::HashSet::new();
        for s in self.shares.all() {
            if s.kind == ShareKind::Recording
                && fs::try_exists(
                    self.recorder_config
                        .recordings_dir
                        .join(s.target.to_string())
                        .join("meta.json"),
                )
                .await
                .unwrap_or(false)
            {
                recordings.insert(s.target);
            }
        }
        self.shares
            .prune(|s| match s.kind {
                ShareKind::Recording => recordings.contains(&s.target),
                ShareKind::Session => sessions.contains(&s.target),
            })
            .await;
        self.shares
            .all()
            .into_iter()
            .map(|s| self.share_view(s))
            .collect()
    }
}

// ------------------------------------------------------------- management API

pub(crate) async fn api_list_shares(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(state.live_shares().await))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ShareRequest {
    pub(crate) kind: ShareKind,
    /// The recording's or the session's id.
    pub(crate) id: Uuid,
    /// Seconds until the link stops working; never when absent.
    #[serde(default)]
    pub(crate) expires_in_secs: Option<u64>,
    /// Sessions: whoever has the link may also control it (true) or only watch
    /// (false, the default for a new link); absent keeps an existing link's mode.
    #[serde(default)]
    pub(crate) control: Option<bool>,
}

/// Share a recording or a session (`{"kind": "recording", "id": ...}`); a shared
/// recording is also kept, so the cleanup does not break its link.
pub(crate) async fn api_create_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ShareRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    match req.kind {
        ShareKind::Recording => {
            if !recorder::set_keep(&state.recorder_config, req.id, true).await {
                return Err(api_error(StatusCode::NOT_FOUND, "Recording not found"));
            }
        }
        ShareKind::Session => {
            let Some(session) = state.sessions.read().await.get(&req.id).cloned() else {
                return Err(api_error(StatusCode::NOT_FOUND, "Session not found"));
            };
            if req.control == Some(true) && session.bidi.is_some() {
                return Err(api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Control through a public link works for Chromium sessions only",
                ));
            }
        }
    }
    let expires_at = req
        .expires_in_secs
        .filter(|s| *s > 0)
        .map(|s| auth::now().saturating_add(s));
    let share = state
        .shares
        .share(req.kind, req.id, expires_at, req.control)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(kind = ?share.kind, target = %share.target, control = share.control, "Shared publicly");
    Ok(Json(state.share_view(share)))
}

/// Unshare: the link stops working at once.
pub(crate) async fn api_delete_share(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    match state.shares.remove(id).await {
        Ok(true) => {
            info!(share = %id, "Unshared");
            Ok(Json(serde_json::json!({ "status": "removed", "id": id })))
        }
        Ok(false) => Err(api_error(StatusCode::NOT_FOUND, "Share not found")),
        Err(e) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ------------------------------------------------------------- public routes

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": "This link does not exist or no longer works" })),
    )
        .into_response()
}

/// Headers of everything a share serves: nobody indexes it, embeds it or learns
/// its address from a referrer.
fn harden(mut res: Response) -> Response {
    let h = res.headers_mut();
    for (name, value) in [
        ("x-robots-tag", "noindex, nofollow"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    res
}

/// The share page (`/share/<node>/<token>`).
pub(crate) async fn share_page() -> Response {
    harden(
        (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                     img-src 'self' blob: data:; media-src 'self' blob:; connect-src 'self'; \
                     frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
                ),
            ],
            Html(SHARE_HTML),
        )
            .into_response(),
    )
}

/// Largest input request of a controlling viewer (typed text included).
const MAX_INPUT_BYTES: usize = 8 * 1024;

/// What a share page loads (`/share/<node>/<token>/<what>`): here, or from the
/// share's node. `POST .../input` is a controlling viewer's input.
pub(crate) async fn share_resource(
    AxumPath((node, token, what)): AxumPath<(String, String, String)>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let post = req.method() == axum::http::Method::POST;
    if post != (what == "input") {
        return not_found();
    }
    if node != state.node_name {
        if req.headers().contains_key(FORWARDED) {
            return not_found();
        }
        let headers = req.headers().clone();
        let body = if post {
            match axum::body::to_bytes(req.into_body(), MAX_INPUT_BYTES).await {
                Ok(b) => Some(b),
                Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            }
        } else {
            None
        };
        return harden(forward(&state, &node, &token, &what, &headers, body).await);
    }
    if post {
        let Some(share) = state.shares.by_token(&token) else {
            return not_found();
        };
        let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_INPUT_BYTES).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        return harden(input(&state, &share, &body).await);
    }
    let Some(share) = state.shares.by_token(&token) else {
        return not_found();
    };
    let res = match (share.kind, what.as_str()) {
        (_, "info") => info(&state, &share).await,
        (ShareKind::Recording, "frames") => recording_frames(&state, share.target).await,
        (ShareKind::Recording, "audio") => recording_audio(&state, share.target, req).await,
        (ShareKind::Recording, "favicon") => recording_favicon(&state, share.target).await,
        (ShareKind::Session, "frame") => {
            if let Some(viewer) = req
                .headers()
                .get(VIEWER)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 64)
            {
                state.shares.note_watcher(share.target, viewer);
            }
            live_frame(&state, share.target).await
        }
        _ => not_found(),
    };
    harden(res)
}

/// The same request to the share's node.
async fn forward(
    state: &AppState,
    node: &str,
    token: &str,
    what: &str,
    headers: &HeaderMap,
    body: Option<Bytes>,
) -> Response {
    if !token_shape(token) {
        return not_found();
    }
    let Some(peer_url) = get_peer_url(state, node).await else {
        return not_found();
    };
    let Ok(mut url) = url::Url::parse(&peer_url) else {
        return not_found();
    };
    match url.path_segments_mut() {
        Ok(mut path) => {
            path.pop_if_empty().extend(["share", node, token, what]);
        }
        Err(()) => return not_found(),
    }
    let wait = if matches!(what, "frames" | "audio") {
        PeerWait::Long
    } else {
        PeerWait::Short
    };
    let mut req = match body {
        Some(body) => state
            .peer_request(state.peer_client.post(url))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        None => state.peer_request(state.peer_client.get(url)),
    }
    .header(FORWARDED, "1")
    .timeout(wait.timeout());
    for name in [header::RANGE.as_str(), VIEWER] {
        if let Some(v) = headers.get(name) {
            req = req.header(name, v.clone());
        }
    }
    match req.send().await {
        Ok(res) => {
            let status =
                StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
                header::CACHE_CONTROL,
                axum::http::HeaderName::from_static("x-page-url"),
                axum::http::HeaderName::from_static("x-page-title"),
            ] {
                if let Some(v) = res.headers().get(name.as_str()) {
                    if let Ok(v) = HeaderValue::from_bytes(v.as_bytes()) {
                        builder = builder.header(name, v);
                    }
                }
            }
            builder
                .body(axum::body::Body::from_stream(res.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => {
            warn!(node, error = %e, "Could not reach a share's node");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": "The node with this share is not reachable" })),
            )
                .into_response()
        }
    }
}

/// An address cut down to its site: a path or query can carry a login code.
fn site_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.origin().ascii_serialization())
}

async fn info(state: &AppState, share: &Share) -> Response {
    let body = match share.kind {
        ShareKind::Recording => {
            let Some(rec) = recorder::get_recording(&state.recorder_config, share.target).await
            else {
                return not_found();
            };
            serde_json::json!({
                "kind": share.kind,
                "title": rec.title,
                "site": rec.final_url.as_deref().or(rec.initial_url.as_deref()).and_then(site_of),
                "started_at": rec.started_at_secs,
                "duration_ms": rec.length_ms(),
                "frame_count": rec.frame_count,
                "size_bytes": rec.size_bytes,
                "audio": rec.audio,
                "favicon": rec.favicon_type.is_some(),
                "status": rec.status,
                "expires_at": share.expires_at,
            })
        }
        ShareKind::Session => {
            let Some(session) = state.sessions.read().await.get(&share.target).cloned() else {
                return not_found();
            };
            let name = session.name.read().unwrap().clone();
            serde_json::json!({
                "kind": share.kind,
                "name": name,
                "engine": session.engine,
                "control": share.control && session.bidi.is_none(),
                "expires_at": share.expires_at,
            })
        }
    };
    state.shares.note_view(share.id);
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

/// The recording's frames as JSON lines, each address cut down to its site.
async fn recording_frames(state: &AppState, id: Uuid) -> Response {
    use tokio::io::AsyncBufReadExt;
    let path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join("frames.jsonl");
    let Ok(file) = fs::File::open(&path).await else {
        return not_found();
    };
    let lines = tokio::io::BufReader::with_capacity(256 * 1024, file).lines();
    let stream = futures_util::stream::unfold(lines, |mut lines| async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    // A line cut short (a crash while recording) is left out.
                    let Ok(mut frame) = serde_json::from_str::<recorder::RecordingFrame>(&line)
                    else {
                        continue;
                    };
                    frame.url = frame.url.as_deref().and_then(site_of);
                    let mut out = serde_json::to_vec(&frame).unwrap_or_default();
                    out.push(b'\n');
                    return Some((Ok::<_, std::io::Error>(Bytes::from(out)), lines));
                }
                Ok(None) => return None,
                Err(e) => return Some((Err(e), lines)),
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn recording_audio(state: &AppState, id: Uuid, req: axum::extract::Request) -> Response {
    let path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join(rec_audio::FILE);
    if !fs::try_exists(&path).await.unwrap_or(false) {
        return not_found();
    }
    match tower_http::services::ServeFile::new(&path)
        .try_call(req)
        .await
    {
        Ok(res) => {
            let mut res = res.map(axum::body::Body::new);
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/webm"));
            res
        }
        Err(_) => not_found(),
    }
}

async fn recording_favicon(state: &AppState, id: Uuid) -> Response {
    let Some(rec) = recorder::get_recording(&state.recorder_config, id).await else {
        return not_found();
    };
    let dir = state.recorder_config.recordings_dir.join(id.to_string());
    let (Some(mime), Ok(bytes)) = (rec.favicon_type, fs::read(dir.join("favicon.bin")).await)
    else {
        return not_found();
    };
    // Only pictures: the type was recorded from the site.
    if !mime.starts_with("image/") || mime.contains("svg") {
        return not_found();
    }
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "max-age=300".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// A picture of the shared session's front tab, taken at most every
/// `LIVE_FRESH`, with the tab's site and title in `X-Page-Url` / `X-Page-Title`
/// (percent-encoded).
async fn live_frame(state: &AppState, id: Uuid) -> Response {
    let Some(session) = state.sessions.read().await.get(&id).cloned() else {
        return not_found();
    };
    let slot = state.shares.live_slot(id);
    let mut latest = slot.lock().await;
    let frame = match latest.as_ref() {
        Some((at, frame)) if at.elapsed() < LIVE_FRESH => Some(frame.clone()),
        _ => {
            let shot = match session.bidi {
                Some(ref hub) => match hub.active_context().await {
                    Some(ctx) => hub.screenshot_jpeg(&ctx).await.ok().and_then(|b64| {
                        let jpeg = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
                        Some(LiveFrame {
                            jpeg,
                            url: None,
                            title: None,
                        })
                    }),
                    None => None,
                },
                None => thumbnail::capture(&state.http_client, session.port, LIVE_WIDTH, 75)
                    .await
                    .map(|c| LiveFrame {
                        jpeg: c.jpeg,
                        url: c.url,
                        title: c.title,
                    }),
            };
            match shot {
                Some(frame) => {
                    let frame = Arc::new(frame);
                    *latest = Some((Instant::now(), frame.clone()));
                    Some(frame)
                }
                None => latest.as_ref().map(|(_, f)| f.clone()),
            }
        }
    };
    drop(latest);
    let Some(frame) = frame else {
        return (
            StatusCode::NO_CONTENT,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response();
    };
    let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mut res = (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        frame.jpeg.clone(),
    )
        .into_response();
    for (name, value) in [
        ("x-page-url", frame.url.as_deref().and_then(site_of)),
        ("x-page-title", frame.title.clone()),
    ] {
        if let Some(v) = value.and_then(|v| HeaderValue::from_str(&encode(&v)).ok()) {
            res.headers_mut().insert(name, v);
        }
    }
    res
}

/// What a controlling viewer sends: positions are fractions (0..1) of the live
/// picture, which shows the tab's viewport.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Input {
    Click { x: f64, y: f64 },
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
    Key { key: String },
    Text { text: String },
}

/// The keys a controlling viewer may press, with their Windows key codes; no
/// modifiers, so no browser shortcuts.
const KEYS: &[(&str, i64)] = &[
    ("Enter", 13),
    ("Backspace", 8),
    ("Tab", 9),
    ("Escape", 27),
    ("Delete", 46),
    ("ArrowLeft", 37),
    ("ArrowUp", 38),
    ("ArrowRight", 39),
    ("ArrowDown", 40),
    ("PageUp", 33),
    ("PageDown", 34),
    ("Home", 36),
    ("End", 35),
];

/// Most text one input request types.
const MAX_TEXT_CHARS: usize = 2000;

fn refuse(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Input of a viewer of a session shared to be controlled.
async fn input(state: &AppState, share: &Share, body: &[u8]) -> Response {
    if share.kind != ShareKind::Session {
        return not_found();
    }
    if !share.control {
        return refuse(StatusCode::FORBIDDEN, "This link is view only");
    }
    let ev = match serde_json::from_slice::<Input>(body) {
        Ok(Input::Text { ref text }) if text.chars().count() > MAX_TEXT_CHARS => {
            return refuse(StatusCode::PAYLOAD_TOO_LARGE, "Too much text at once");
        }
        Ok(Input::Key { ref key }) if !KEYS.iter().any(|(k, _)| k == key) => {
            return refuse(StatusCode::BAD_REQUEST, "Not a key that can be pressed");
        }
        Ok(ev) => ev,
        Err(_) => return refuse(StatusCode::BAD_REQUEST, "Not an input"),
    };
    let Some(session) = state.sessions.read().await.get(&share.target).cloned() else {
        return not_found();
    };
    if session.bidi.is_some() {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Control through a public link works for Chromium sessions only",
        );
    }
    // One input at a time, between pictures; the next picture shows its effect.
    let slot = state.shares.live_slot(share.target);
    let mut latest = slot.lock().await;
    session.activity.touch();
    let done = chromium_input(&state.http_client, session.port, &ev).await;
    *latest = None;
    drop(latest);
    match done {
        Ok(()) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Err(e) => {
            warn!(session = %share.target, error = %e, "Public input failed");
            refuse(
                StatusCode::BAD_GATEWAY,
                "The browser did not take the input",
            )
        }
    }
}

/// Send one input to the tab in front of the Chromium listening on `port`.
async fn chromium_input(http: &reqwest::Client, port: u16, ev: &Input) -> Result<(), String> {
    let targets: Vec<serde_json::Value> = http
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let page = recorder::pick_visible(recorder::page_candidates(targets))
        .await
        .ok_or("no page")?;
    let ws = page
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .ok_or("no page")?
        .to_string();
    let mut cdp = autofill::Cdp::connect(&ws).await?;
    let result = async {
        let metrics = cdp
            .call("Page.getLayoutMetrics", serde_json::json!({}))
            .await?;
        let vp = &metrics["cssVisualViewport"];
        let (w, h) = (
            vp["clientWidth"].as_f64().unwrap_or(0.0),
            vp["clientHeight"].as_f64().unwrap_or(0.0),
        );
        let at = |x: f64, y: f64| (x.clamp(0.0, 1.0) * w, y.clamp(0.0, 1.0) * h);
        match ev {
            Input::Click { x, y } => {
                let (x, y) = at(*x, *y);
                for (kind, buttons) in
                    [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)]
                {
                    cdp.call(
                        "Input.dispatchMouseEvent",
                        serde_json::json!({
                            "type": kind, "x": x, "y": y, "button": "left",
                            "buttons": buttons, "clickCount": 1,
                        }),
                    )
                    .await?;
                }
            }
            Input::Scroll { x, y, dx, dy } => {
                let (x, y) = at(*x, *y);
                cdp.call(
                    "Input.dispatchMouseEvent",
                    serde_json::json!({
                        "type": "mouseWheel", "x": x, "y": y,
                        "deltaX": dx.clamp(-3000.0, 3000.0), "deltaY": dy.clamp(-3000.0, 3000.0),
                    }),
                )
                .await?;
            }
            Input::Key { key } => {
                let code = KEYS.iter().find(|(k, _)| k == key).map_or(0, |(_, c)| *c);
                // Enter types a newline (it submits forms); the others only press.
                let down = if key == "Enter" {
                    serde_json::json!({ "type": "keyDown", "key": key, "code": key, "text": "\r",
                        "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code })
                } else {
                    serde_json::json!({ "type": "rawKeyDown", "key": key, "code": key,
                        "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code })
                };
                cdp.call("Input.dispatchKeyEvent", down).await?;
                cdp.call(
                    "Input.dispatchKeyEvent",
                    serde_json::json!({ "type": "keyUp", "key": key, "code": key,
                        "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code }),
                )
                .await?;
            }
            Input::Text { text } => {
                cdp.call("Input.insertText", serde_json::json!({ "text": text }))
                    .await?;
            }
        }
        Ok::<_, String>(())
    }
    .await;
    cdp.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(&dir.path().join("shares.json"));
        (dir, s)
    }

    #[test]
    fn tokens_are_long_and_random() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 86);
        assert!(token_shape(&a));
        assert_ne!(a, b);
        assert!(!token_shape("short"));
        assert!(!token_shape(&format!("{}/", &a[..85])));
        assert!(!token_shape(&format!("{a}A")));
    }

    #[tokio::test]
    async fn one_link_per_target_until_unshared() {
        let (dir, s) = store();
        let rec = Uuid::new_v4();
        let first = s
            .share(ShareKind::Recording, rec, None, None)
            .await
            .unwrap();
        let again = s
            .share(ShareKind::Recording, rec, Some(u64::MAX), Some(true))
            .await
            .unwrap();
        assert_eq!(first.token, again.token);
        // Recordings are only ever watched.
        assert!(!again.control);
        assert_eq!(again.expires_at, Some(u64::MAX));
        assert_eq!(s.by_token(&first.token).unwrap().id, first.id);
        // Kept across restarts.
        let reloaded = Store::load(&dir.path().join("shares.json"));
        assert_eq!(reloaded.by_token(&first.token).unwrap().target, rec);
        assert!(s.remove(first.id).await.unwrap());
        assert!(s.by_token(&first.token).is_none());
        assert!(!s.remove(first.id).await.unwrap());
        // A new share is a new link.
        let second = s
            .share(ShareKind::Recording, rec, None, None)
            .await
            .unwrap();
        assert_ne!(second.token, first.token);
    }

    #[tokio::test]
    async fn expired_links_do_not_work() {
        let (_dir, s) = store();
        let share = s
            .share(ShareKind::Session, Uuid::new_v4(), Some(1), None)
            .await
            .unwrap();
        assert!(s.by_token(&share.token).is_none());
        s.prune(|_| true).await;
        assert!(s.all().is_empty());
    }

    #[tokio::test]
    async fn gone_targets_lose_their_share() {
        let (_dir, s) = store();
        let session = Uuid::new_v4();
        let share = s
            .share(ShareKind::Session, session, None, None)
            .await
            .unwrap();
        assert!(!share.control, "view only unless asked");
        let control = s
            .share(ShareKind::Session, session, None, Some(true))
            .await
            .unwrap();
        assert!(control.control);
        assert_eq!(control.token, share.token);
        // Absent: the mode stays.
        assert!(
            s.share(ShareKind::Session, session, None, None)
                .await
                .unwrap()
                .control
        );
        s.remove_target(ShareKind::Recording, session).await;
        assert!(s.by_token(&share.token).is_some());
        s.remove_target(ShareKind::Session, session).await;
        assert!(s.by_token(&share.token).is_none());
    }

    #[test]
    fn inputs_parse_and_keys_are_few() {
        let ev: Input = serde_json::from_str(r#"{"type":"click","x":0.5,"y":0.25}"#).unwrap();
        assert!(matches!(ev, Input::Click { .. }));
        assert!(serde_json::from_str::<Input>(r#"{"type":"eval","js":"1"}"#).is_err());
        assert!(KEYS.iter().any(|(k, _)| *k == "Enter"));
        assert!(!KEYS
            .iter()
            .any(|(k, _)| k.contains("Control") || k.starts_with('F')));
    }

    #[test]
    fn addresses_are_cut_to_their_site() {
        assert_eq!(
            site_of("https://accounts.example.com/o/oauth2?code=secret#x").as_deref(),
            Some("https://accounts.example.com")
        );
        assert_eq!(
            site_of("http://example.com:8080/a").as_deref(),
            Some("http://example.com:8080")
        );
        assert_eq!(site_of("about:blank"), None);
        assert_eq!(site_of("chrome://settings"), None);
        assert_eq!(site_of("data:text/html,hi"), None);
    }
}
