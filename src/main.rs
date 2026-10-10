mod browser;
mod proxy;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::fs;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::browser::{ActiveBrowser, BrowserConfig};
use crate::proxy::bridge_websockets;

static HANDOFF_HTML: &str = include_str!("../handoff.html");
static DASHBOARD_HTML: &str = include_str!("../dashboard.html");

#[derive(Parser, Debug)]
#[command(author, version, about = "Native NixOS remote browser daemon")]
pub struct Args {
    #[arg(short, long, default_value = "127.0.0.1:3001")]
    pub bind: SocketAddr,

    #[arg(long, default_value = "chromium")]
    pub chromium: PathBuf,

    #[arg(long)]
    pub display: Option<String>,

    #[arg(long, default_value = "8")]
    pub max_sessions: usize,

    #[arg(long, default_value = "/var/lib/browser-daemon/profiles")]
    pub profile_dir: PathBuf,

    #[arg(long, default_value_t = 1600)]
    pub default_width: u32,

    #[arg(long, default_value_t = 1000)]
    pub default_height: u32,
}

#[derive(Clone)]
pub struct ManagedSession {
    pub id: Uuid,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    pub persistent: bool,
    pub created_at: Instant,
    pub client_count: Arc<AtomicUsize>,
    pub browser: Arc<Mutex<Option<ActiveBrowser>>>,
}

impl ManagedSession {
    pub async fn to_summary(&self, http_client: &reqwest::Client) -> SessionSummary {
        let tabs_count = {
            let url = format!("http://127.0.0.1:{}/json/list", self.port);
            if let Ok(res) = http_client.get(&url).send().await {
                if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                    targets.iter().filter(|t| t.target_type == "page").count()
                } else {
                    0
                }
            } else {
                0
            }
        };

        SessionSummary {
            id: self.id,
            port: self.port,
            ws_path: self.ws_path.clone(),
            profile: self.profile.clone(),
            persistent: self.persistent,
            uptime_seconds: self.created_at.elapsed().as_secs(),
            client_count: self.client_count.load(Ordering::SeqCst),
            tabs_count,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    pub persistent: bool,
    pub uptime_seconds: u64,
    pub client_count: usize,
    pub tabs_count: usize,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<BrowserConfig>,
    pub max_sessions: usize,
    pub sessions: Arc<RwLock<HashMap<Uuid, ManagedSession>>>,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub async fn spawn_session(
        &self,
        profile: Option<&str>,
        persistent: bool,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<ManagedSession, (StatusCode, &'static str)> {
        // If a profile was given, check if an existing session is already running it
        if let Some(prof_name) = profile {
            let sanitized = sanitize_profile_name(prof_name);
            if !sanitized.is_empty() {
                let sessions = self.sessions.read().await;
                for session in sessions.values() {
                    if let Some(ref p) = session.profile {
                        if p == &sanitized {
                            info!(session_id = %session.id, profile = %sanitized, "Attaching to existing profile session");
                            return Ok(session.clone());
                        }
                    }
                }
            }
        }

        let active_count = self.sessions.read().await.len();
        if active_count >= self.max_sessions {
            warn!(active = active_count, max = self.max_sessions, "Session limit reached");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Session limit reached. Try again later.",
            ));
        }

        let browser = match ActiveBrowser::spawn(&self.config, profile, width, height).await {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "Failed to spawn Chromium instance");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to spawn browser process",
                ));
            }
        };

        let session = ManagedSession {
            id: browser.id,
            port: browser.port,
            ws_path: browser.ws_path.clone(),
            profile: browser.profile_name.clone(),
            persistent,
            created_at: browser.created_at,
            client_count: Arc::new(AtomicUsize::new(0)),
            browser: Arc::new(Mutex::new(Some(browser))),
        };

        {
            let mut guard = self.sessions.write().await;
            guard.insert(session.id, session.clone());
        }

        Ok(session)
    }

    pub async fn terminate_session(&self, id: &Uuid) -> bool {
        let removed = {
            let mut guard = self.sessions.write().await;
            guard.remove(id)
        };

        if let Some(session) = removed {
            let mut b_guard = session.browser.lock().await;
            if let Some(mut b) = b_guard.take() {
                b.terminate().await;
                info!(session_id = %id, "Session terminated");
                return true;
            }
        }
        false
    }
}

#[derive(Deserialize, Debug)]
pub struct LaunchParams {
    pub session_id: Option<Uuid>,
    pub profile: Option<String>,
    #[serde(default)]
    pub persistent: Option<bool>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Deserialize, Debug)]
pub struct CreateSessionRequest {
    pub profile: Option<String>,
    #[serde(default = "default_true")]
    pub persistent: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Debug)]
pub struct ProfileInfo {
    pub name: String,
    pub size_bytes: u64,
    pub is_active: bool,
    pub session_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DevToolsTarget {
    pub id: String,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub target_type: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devtools_frontend_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_socket_debugger_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "browser_daemon=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();
    info!("Starting browser-daemon on {}", args.bind);

    let config = Arc::new(BrowserConfig {
        chromium_bin: args.chromium,
        display: args.display,
        profiles_dir: args.profile_dir,
        default_width: args.default_width,
        default_height: args.default_height,
    });

    let state = AppState {
        config,
        max_sessions: args.max_sessions,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?,
    };

    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/dashboard", get(|| async { Redirect::to("/") }))
        .route("/health", get(health_check))
        .route("/json/version", get(json_version))
        .route("/json", get(json_list))
        .route("/json/list", get(json_list))
        .route("/handoff/", get(handoff_page))
        .route("/handoff", get(|| async { Redirect::to("/handoff/") }))
        .route("/chromium", get(handle_chromium_ws))
        .route("/devtools/page/{page_id}", get(handle_page_ws))
        // REST Management APIs for sessions and profiles
        .route("/api/sessions", get(api_list_sessions).post(api_create_session))
        .route("/api/sessions/{id}", get(api_get_session).delete(api_delete_session))
        .route("/api/profiles", get(api_list_profiles))
        .route("/api/profiles/{name}", delete(api_delete_profile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    Json(serde_json::json!({
        "status": "ok",
        "active_sessions": sessions.len(),
        "max_sessions": state.max_sessions,
    }))
}

async fn json_version() -> impl IntoResponse {
    Json(serde_json::json!({
        "Browser": "Chrome/153.0.0.0",
        "Protocol-Version": "1.3",
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36",
        "V8-Version": "15.3.76.4",
        "WebKit-Version": "537.36",
        "webSocketDebuggerUrl": "ws://localhost:3001/chromium"
    }))
}

async fn json_list(State(state): State<AppState>) -> impl IntoResponse {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    let mut all_targets = Vec::new();

    for session in sessions {
        let url = format!("http://127.0.0.1:{}/json/list", session.port);
        if let Ok(res) = state.http_client.get(&url).send().await {
            if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                for mut target in targets {
                    target.web_socket_debugger_url =
                        Some(format!("/devtools/page/{}", target.id));
                    all_targets.push(target);
                }
            }
        }
    }

    Json(all_targets)
}

async fn dashboard_page() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Html(DASHBOARD_HTML),
    )
}

async fn handoff_page() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Html(HANDOFF_HTML),
    )
}

async fn handle_chromium_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
) -> Response {
    let session = if let Some(sid) = params.session_id {
        let guard = state.sessions.read().await;
        match guard.get(&sid).cloned() {
            Some(s) => s,
            None => {
                return (StatusCode::NOT_FOUND, "Requested session ID not found").into_response();
            }
        }
    } else {
        let persistent = params.persistent.unwrap_or(false);
        match state
            .spawn_session(
                params.profile.as_deref(),
                persistent,
                params.width,
                params.height,
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    ws.on_upgrade(move |socket| async move {
        session.client_count.fetch_add(1, Ordering::SeqCst);

        let ws_path = session.ws_path.trim_start_matches('/');
        let backend_ws_url = format!("ws://127.0.0.1:{}/{}", session.port, ws_path);
        info!(session_id = %session.id, url = %backend_ws_url, "Connecting client to Chromium DevTools");

        match tokio_tungstenite::connect_async(&backend_ws_url).await {
            Ok((server_ws, _)) => {
                bridge_websockets(socket, server_ws).await;
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Failed to connect to Chromium backend");
            }
        }

        let remaining = session.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if !session.persistent && remaining == 0 {
            info!(session_id = %session.id, "Auto-terminating non-persistent session");
            state.terminate_session(&session.id).await;
        }
    })
}

async fn handle_page_ws(
    AxumPath(page_id): AxumPath<String>,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    let mut target_port: Option<u16> = None;
    for session in sessions {
        let url = format!("http://127.0.0.1:{}/json/list", session.port);
        if let Ok(res) = state.http_client.get(&url).send().await {
            if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                if targets.iter().any(|t| t.id == page_id) {
                    target_port = Some(session.port);
                    break;
                }
            }
        }
    }

    let port = match target_port {
        Some(p) => p,
        None => {
            warn!(page_id = %page_id, "Page not found across active sessions");
            return (StatusCode::NOT_FOUND, "Target page not found").into_response();
        }
    };

    let backend_url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, page_id);
    ws.on_upgrade(move |socket| async move {
        match tokio_tungstenite::connect_async(&backend_url).await {
            Ok((server_ws, _)) => {
                bridge_websockets(socket, server_ws).await;
            }
            Err(e) => {
                error!(page_id = %page_id, error = %e, "Failed to bridge to target page");
            }
        }
    })
}

// ---------------------------------------------------------------------------
// REST API Handlers
// ---------------------------------------------------------------------------

async fn api_list_sessions(State(state): State<AppState>) -> impl IntoResponse {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    let mut summaries = Vec::new();
    for session in sessions {
        summaries.push(session.to_summary(&state.http_client).await);
    }

    Json(summaries)
}

async fn api_create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    let session = state
        .spawn_session(
            req.profile.as_deref(),
            req.persistent,
            req.width,
            req.height,
        )
        .await?;

    let summary = session.to_summary(&state.http_client).await;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn api_get_session(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let session = {
        let guard = state.sessions.read().await;
        guard.get(&id).cloned().ok_or(StatusCode::NOT_FOUND)?
    };

    let summary = session.to_summary(&state.http_client).await;
    let url = format!("http://127.0.0.1:{}/json/list", session.port);
    let targets = if let Ok(res) = state.http_client.get(&url).send().await {
        res.json::<Vec<DevToolsTarget>>().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Json(serde_json::json!({
        "session": summary,
        "targets": targets,
    })))
}

async fn api_delete_session(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if state.terminate_session(&id).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "terminated", "id": id })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        )
    }
}

async fn api_list_profiles(State(state): State<AppState>) -> impl IntoResponse {
    let active_profiles: HashMap<String, Uuid> = {
        let guard = state.sessions.read().await;
        guard
            .values()
            .filter_map(|s| s.profile.as_ref().map(|p| (p.clone(), s.id)))
            .collect()
    };

    let mut profiles = Vec::new();
    let dir = &state.config.profiles_dir;
    if let Ok(mut entries) = fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                if meta.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let size_bytes = compute_dir_size(entry.path()).await;
                    let is_active = active_profiles.contains_key(&name);
                    let session_id = active_profiles.get(&name).cloned();

                    profiles.push(ProfileInfo {
                        name,
                        size_bytes,
                        is_active,
                        session_id,
                    });
                }
            }
        }
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Json(profiles)
}

async fn api_delete_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let sanitized = sanitize_profile_name(&name);
    if sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    // Check if profile is active in an ongoing session
    {
        let guard = state.sessions.read().await;
        if let Some(s) = guard.values().find(|s| s.profile.as_deref() == Some(&sanitized)) {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Profile is currently in use by active session {}", s.id)
                })),
            ));
        }
    }

    let dir = state.config.profiles_dir.join(&sanitized);
    if !dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Profile directory not found" })),
        ));
    }

    if let Err(e) = fs::remove_dir_all(&dir).await {
        error!(profile = %sanitized, error = %e, "Failed to remove profile directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to remove profile directory" })),
        ));
    }

    info!(profile = %sanitized, "Profile directory removed");
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "status": "deleted", "profile": sanitized })),
    ))
}

fn sanitize_profile_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

async fn compute_dir_size(path: PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || {
        fn walk(p: &Path) -> u64 {
            let mut s = 0;
            if let Ok(rd) = std::fs::read_dir(p) {
                for entry in rd.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.is_file() {
                            s += meta.len();
                        } else if meta.is_dir() {
                            s += walk(&entry.path());
                        }
                    }
                }
            }
            s
        }
        walk(&path)
    })
    .await
    .unwrap_or(0)
}
