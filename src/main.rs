mod browser;
mod proxy;
mod recorder;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::browser::{ActiveBrowser, BrowserConfig};
use crate::proxy::bridge_websockets;
use crate::recorder::{RecorderConfig, RecordingMeta, SessionRecorderHandle};

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

    #[arg(long)]
    pub node_name: Option<String>,

    #[arg(long)]
    pub public_url: Option<String>,

    #[arg(long = "peer")]
    pub peers: Vec<String>,

    #[arg(long, help = "Shared cluster secret for peer-to-peer authentication")]
    pub cluster_secret: Option<String>,

    #[arg(long, help = "Path to file containing shared cluster secret")]
    pub cluster_secret_file: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/browser-daemon/recordings")]
    pub recordings_dir: PathBuf,

    #[arg(long, default_value_t = true)]
    pub record_sessions: bool,

    #[arg(long, default_value_t = 2048)]
    pub recording_max_storage_mb: u64,

    #[arg(long, default_value_t = 168)]
    pub recording_max_age_hours: u64,

    #[arg(long, default_value_t = 100)]
    pub recording_max_count: usize,
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
    pub async fn to_summary(&self, http_client: &reqwest::Client, is_recording: bool) -> SessionSummary {
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
            is_recording,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    pub persistent: bool,
    pub uptime_seconds: u64,
    pub client_count: usize,
    pub tabs_count: usize,
    pub is_recording: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<BrowserConfig>,
    pub max_sessions: usize,
    pub sessions: Arc<RwLock<HashMap<Uuid, ManagedSession>>>,
    pub http_client: reqwest::Client,
    pub node_name: String,
    pub public_url: String,
    pub peers: Arc<RwLock<HashMap<String, String>>>,
    pub start_time: Instant,
    pub cluster_secret: Option<String>,
    pub recorder_config: Arc<RecorderConfig>,
    pub active_recordings: Arc<RwLock<HashMap<Uuid, SessionRecorderHandle>>>,
}

impl AppState {
    pub fn peer_request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref sec) = self.cluster_secret {
            builder
                .bearer_auth(sec)
                .header("X-Browser-Cluster-Secret", sec)
        } else {
            builder
        }
    }

    pub fn check_auth(
        &self,
        headers: &HeaderMap,
    ) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        if let Some(ref expected) = self.cluster_secret {
            let auth_header = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok());
            let custom_header = headers.get("x-browser-cluster-secret").and_then(|h| h.to_str().ok());

            let token = auth_header
                .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
                .or(custom_header);

            if token != Some(expected.as_str()) {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": "Unauthorized: invalid or missing cluster secret"
                    })),
                ));
            }
        }
        Ok(())
    }
    pub async fn spawn_session(
        &self,
        profile: Option<&str>,
        persistent: bool,
        width: Option<u32>,
        height: Option<u32>,
        record: Option<bool>,
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

        if record.unwrap_or(self.recorder_config.enabled) {
            let handle = recorder::start_recording(
                session.id,
                session.port,
                session.profile.clone(),
                self.node_name.clone(),
                (*self.recorder_config).clone(),
            );
            self.active_recordings.write().await.insert(session.id, handle);
        }

        Ok(session)
    }

    pub async fn terminate_session(&self, id: &Uuid) -> bool {
        if let Some(handle) = self.active_recordings.write().await.remove(id) {
            handle.stop();
        }

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
    pub record: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateSessionRequest {
    pub profile: Option<String>,
    #[serde(default = "default_true")]
    pub persistent: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub record: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub size_bytes: u64,
    pub is_active: bool,
    pub session_id: Option<Uuid>,
    pub created_at_secs: Option<u64>,
    pub modified_at_secs: Option<u64>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ProfileDetails {
    pub name: String,
    pub size_bytes: u64,
    pub is_active: bool,
    pub session_id: Option<Uuid>,
    pub created_at_secs: Option<u64>,
    pub modified_at_secs: Option<u64>,
    pub session: Option<SessionSummary>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateProfileRequest {
    pub name: String,
    pub clone_from: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateProfileRequest {
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CloneProfileRequest {
    pub target_name: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct DeleteProfileParams {
    pub force: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeInfo {
    pub name: String,
    pub url: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    pub recordings_count: usize,
    pub storage_used_bytes: u64,
    pub max_storage_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerState {
    pub name: String,
    pub url: String,
    pub status: String,
    pub latency_ms: Option<u64>,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    #[serde(default)]
    pub recordings_count: usize,
    #[serde(default)]
    pub storage_used_bytes: u64,
    pub uptime_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshSession {
    #[serde(flatten)]
    pub session: SessionSummary,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshProfile {
    #[serde(flatten)]
    pub profile: ProfileInfo,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshTarget {
    #[serde(flatten)]
    pub target: DevToolsTarget,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshRecording {
    #[serde(flatten)]
    pub recording: RecordingMeta,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshOverview {
    pub local_node: NodeInfo,
    pub peers: Vec<PeerState>,
    pub sessions: Vec<MeshSession>,
    pub profiles: Vec<MeshProfile>,
    pub targets: Vec<MeshTarget>,
    pub recordings: Vec<MeshRecording>,
    pub total_sessions: usize,
    pub total_capacity: usize,
    pub total_profiles: usize,
    pub total_recordings: usize,
    pub total_nodes: usize,
    pub online_nodes: usize,
    pub total_storage_bytes: u64,
    pub max_storage_bytes: u64,
}

#[derive(Deserialize, Debug)]
pub struct AddPeerRequest {
    pub url: String,
    pub name: Option<String>,
}

fn infer_peer_name(url_str: &str, self_node: &str) -> Option<String> {
    let trimmed = url_str.trim().trim_end_matches('/');
    if let Ok(parsed) = url::Url::parse(trimmed) {
        if let Some(host) = parsed.host_str() {
            if host.starts_with(&format!("browser.{}.", self_node)) || host == format!("browser.{}", self_node) {
                return None;
            }
            if let Some(rest) = host.strip_prefix("browser.") {
                let name = rest.split('.').next().unwrap_or(host);
                if name == self_node {
                    return None;
                }
                return Some(name.to_string());
            }
            let name = host.split('.').next().unwrap_or(host);
            if name == self_node {
                return None;
            }
            return Some(name.to_string());
        }
    }
    None
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

    let node_name = args.node_name.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| "node".to_string())
    });

    let public_url = args.public_url.unwrap_or_else(|| {
        format!("https://browser.{}.ts.brkn.lol", node_name)
    });

    let mut peer_map = HashMap::new();
    for p in args.peers {
        let trimmed = p.trim().trim_end_matches('/').to_string();
        if trimmed.is_empty() || trimmed == public_url.trim_end_matches('/') {
            continue;
        }
        if let Some(inferred) = infer_peer_name(&trimmed, &node_name) {
            if inferred != node_name {
                peer_map.insert(inferred, trimmed);
            }
        } else {
            peer_map.insert(trimmed.clone(), trimmed);
        }
    }

    let cluster_secret = if let Some(ref path) = args.cluster_secret_file {
        match fs::read_to_string(path).await {
            Ok(content) => {
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            Err(e) => {
                eprintln!("Failed to read cluster secret file {}: {}", path.display(), e);
                std::process::exit(1);
            }
        }
    } else {
        args.cluster_secret.or_else(|| {
            std::env::var("CLUSTER_SECRET")
                .or_else(|_| std::env::var("BROWSER_CLUSTER_SECRET"))
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
    };

    info!(
        node = %node_name,
        public_url = %public_url,
        peers = ?peer_map.keys().collect::<Vec<_>>(),
        has_cluster_secret = cluster_secret.is_some(),
        "Starting browser-daemon on {}",
        args.bind
    );

    let config = Arc::new(BrowserConfig {
        chromium_bin: args.chromium,
        display: args.display,
        profiles_dir: args.profile_dir,
        default_width: args.default_width,
        default_height: args.default_height,
    });

    let recorder_config = Arc::new(RecorderConfig {
        enabled: args.record_sessions,
        recordings_dir: args.recordings_dir.clone(),
        max_storage_bytes: args.recording_max_storage_mb * 1024 * 1024,
        max_age_secs: args.recording_max_age_hours * 3600,
        max_count: args.recording_max_count,
    });
    let active_recordings = Arc::new(RwLock::new(HashMap::new()));

    // Run initial auto-vacuum and background periodic vacuum every 10 mins
    let vac_cfg = (*recorder_config).clone();
    tokio::spawn(async move {
        recorder::vacuum_recordings(&vac_cfg).await;
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        loop {
            interval.tick().await;
            recorder::vacuum_recordings(&vac_cfg).await;
        }
    });

    let state = AppState {
        config,
        max_sessions: args.max_sessions,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?,
        node_name,
        public_url,
        peers: Arc::new(RwLock::new(peer_map)),
        start_time: Instant::now(),
        cluster_secret,
        recorder_config,
        active_recordings,
    };

    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/dashboard", get(|| async { Redirect::to("/") }))
        .route("/health", get(health_check))
        .route("/json/version", get(json_version))
        .route("/json", get(json_list))
        .route("/json/list", get(json_list))
        .route("/json/new", put(json_new).post(json_new).get(json_new))
        .route(
            "/json/close/{id}",
            put(json_close)
                .post(json_close)
                .delete(json_close)
                .get(json_close),
        )
        .route(
            "/json/activate/{id}",
            put(json_activate)
                .post(json_activate)
                .get(json_activate),
        )
        .route("/handoff/", get(handoff_page))
        .route("/handoff", get(|| async { Redirect::to("/handoff/") }))
        .route("/chromium", get(handle_chromium_ws))
        .route("/devtools/page/{page_id}", get(handle_page_ws))
        // REST Management APIs for sessions and profiles
        .route("/api/sessions", get(api_list_sessions).post(api_create_session))
        .route("/api/sessions/{id}", get(api_get_session).delete(api_delete_session))
        .route("/api/sessions/{id}/record", post(api_session_start_record))
        .route("/api/sessions/{id}/stop-record", post(api_session_stop_record))
        .route("/api/profiles", get(api_list_profiles).post(api_create_profile))
        .route(
            "/api/profiles/{name}",
            get(api_get_profile)
                .patch(api_update_profile)
                .put(api_update_profile)
                .delete(api_delete_profile),
        )
        .route("/api/profiles/{name}/clone", post(api_clone_profile))
        // Recording APIs
        .route("/api/recordings", get(api_list_recordings))
        .route("/api/recordings/{id}", get(api_get_recording).delete(api_delete_recording))
        .route("/api/recordings/{id}/frames", get(api_get_recording_frames))
        .route("/api/recordings/vacuum", post(api_vacuum_recordings))
        // Peer-to-Peer Mesh APIs
        .route("/api/mesh/node", get(api_mesh_node))
        .route("/api/mesh/overview", get(api_mesh_overview))
        .route("/api/peers", get(api_list_peers).post(api_add_peer))
        .route("/api/peers/{peer}", delete(api_remove_peer))
        .route("/api/peers/{peer}/sessions", post(api_peer_create_session))
        .route("/api/peers/{peer}/sessions/{id}", delete(api_peer_delete_session))
        .route("/api/peers/{peer}/profiles", post(api_peer_create_profile))
        .route(
            "/api/peers/{peer}/profiles/{name}",
            delete(api_peer_delete_profile)
                .patch(api_peer_update_profile)
                .put(api_peer_update_profile),
        )
        .route("/api/peers/{peer}/profiles/{name}/clone", post(api_peer_clone_profile))
        .route("/api/peers/{peer}/recordings", get(api_peer_list_recordings))
        .route("/api/peers/{peer}/recordings/{id}", get(api_peer_get_recording).delete(api_peer_delete_recording))
        .route("/api/peers/{peer}/recordings/{id}/frames", get(api_peer_get_recording_frames))
        .route("/api/peers/{peer}/recordings/vacuum", post(api_peer_vacuum_recordings))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    Json(serde_json::json!({
        "status": "ok",
        "node": state.node_name,
        "active_sessions": sessions.len(),
        "max_sessions": state.max_sessions,
        "peers_count": state.peers.read().await.len(),
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

async fn get_local_targets(state: &AppState) -> Vec<DevToolsTarget> {
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
                    target.session_id = Some(session.id);
                    target.profile = session.profile.clone();
                    all_targets.push(target);
                }
            }
        }
    }

    all_targets
}

async fn json_list(State(state): State<AppState>) -> impl IntoResponse {
    Json(get_local_targets(&state).await)
}

async fn dashboard_page(State(state): State<AppState>) -> impl IntoResponse {
    let secret_injection = match state.cluster_secret {
        Some(ref sec) => format!("window.__CLUSTER_SECRET__ = {:?};", sec),
        None => "window.__CLUSTER_SECRET__ = null;".to_string(),
    };
    let html = DASHBOARD_HTML.replace("/* __CLUSTER_SECRET_PLACEHOLDER__ */", &secret_injection);
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Html(html),
    )
}

async fn handoff_page(State(state): State<AppState>) -> impl IntoResponse {
    let secret_injection = match state.cluster_secret {
        Some(ref sec) => format!("window.__CLUSTER_SECRET__ = {:?};", sec),
        None => "window.__CLUSTER_SECRET__ = null;".to_string(),
    };
    let html = HANDOFF_HTML.replace("/* __CLUSTER_SECRET_PLACEHOLDER__ */", &secret_injection);
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Html(html),
    )
}

async fn json_new(
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let raw = raw_query.unwrap_or_default();
    let mut target_url: Option<String> = None;
    let mut session_id: Option<Uuid> = None;
    let mut page: Option<String> = None;

    if !raw.is_empty() {
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            match k.as_ref() {
                "url" => target_url = Some(v.into_owned()),
                "session_id" => session_id = Uuid::parse_str(&v).ok(),
                "page" => page = Some(v.into_owned()),
                _ => {}
            }
        }
        if target_url.is_none() && !raw.starts_with("session_id=") && !raw.starts_with("page=") {
            let decoded = url::form_urlencoded::parse(raw.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_else(|| raw.clone());
            target_url = Some(decoded);
        }
    }
    let target_url = target_url.unwrap_or_else(|| "about:blank".to_string());

    let session = {
        let guard = state.sessions.read().await;
        if let Some(sid) = session_id {
            guard.get(&sid).cloned()
        } else if let Some(ref pid) = page {
            let mut found = None;
            for s in guard.values() {
                let list_url = format!("http://127.0.0.1:{}/json/list", s.port);
                if let Ok(res) = state.http_client.get(&list_url).send().await {
                    if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                        if targets.iter().any(|t| &t.id == pid) {
                            found = Some(s.clone());
                            break;
                        }
                    }
                }
            }
            found.or_else(|| guard.values().next().cloned())
        } else {
            guard.values().next().cloned()
        }
    };

    let session = match session {
        Some(s) => s,
        None => {
            state
                .spawn_session(None, false, None, None, Some(state.recorder_config.enabled))
                .await
                .map_err(|(st, err)| (st, Json(serde_json::json!({ "error": err }))))?
        }
    };

    let mut dest_url = url::Url::parse(&format!("http://127.0.0.1:{}/json/new", session.port))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;
    dest_url.set_query(Some(&target_url));

    let res = state
        .http_client
        .put(dest_url)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let mut target = res.json::<DevToolsTarget>().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
    target.session_id = Some(session.id);
    target.profile = session.profile.clone();

    Ok(Json(target))
}

async fn json_close(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    for session in sessions {
        let req_url = format!("http://127.0.0.1:{}/json/close/{}", session.port, id);
        if let Ok(res) = state.http_client.put(&req_url).send().await {
            if res.status().is_success() {
                return (StatusCode::OK, "Target is closing").into_response();
            }
        }
    }

    (StatusCode::NOT_FOUND, "Target not found").into_response()
}

async fn json_activate(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    for session in sessions {
        let req_url = format!("http://127.0.0.1:{}/json/activate/{}", session.port, id);
        if let Ok(res) = state.http_client.put(&req_url).send().await {
            if res.status().is_success() {
                return (StatusCode::OK, "Target activated").into_response();
            }
        }
    }

    (StatusCode::NOT_FOUND, "Target not found").into_response()
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
                params.record,
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

async fn get_local_sessions(state: &AppState) -> Vec<SessionSummary> {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };
    let rec_guard = state.active_recordings.read().await;

    let mut summaries = Vec::new();
    for session in sessions {
        let is_rec = rec_guard.contains_key(&session.id);
        summaries.push(session.to_summary(&state.http_client, is_rec).await);
    }
    summaries
}

async fn get_local_profiles(state: &AppState) -> Vec<ProfileInfo> {
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
                    let created_at_secs = meta
                        .created()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    let modified_at_secs = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());

                    profiles.push(ProfileInfo {
                        name,
                        size_bytes,
                        is_active,
                        session_id,
                        created_at_secs,
                        modified_at_secs,
                    });
                }
            }
        }
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

async fn api_list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    Ok(Json(get_local_sessions(&state).await))
}

async fn api_create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let session = state
        .spawn_session(
            req.profile.as_deref(),
            req.persistent,
            req.width,
            req.height,
            req.record,
        )
        .await
        .map_err(|(status, msg)| (status, Json(serde_json::json!({ "error": msg }))))?;

    let is_rec = state.active_recordings.read().await.contains_key(&session.id);
    let summary = session.to_summary(&state.http_client, is_rec).await;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn api_get_session(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let session = {
        let guard = state.sessions.read().await;
        guard
            .get(&id)
            .cloned()
            .ok_or((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Session not found" }))))?
    };

    let is_rec = state.active_recordings.read().await.contains_key(&session.id);
    let summary = session.to_summary(&state.http_client, is_rec).await;
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
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if state.terminate_session(&id).await {
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "status": "terminated", "id": id })),
        ))
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ))
    }
}

async fn api_list_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    Ok(Json(get_local_profiles(&state).await))
}

async fn api_create_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProfileRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let sanitized = sanitize_profile_name(&req.name);
    if sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    let target_dir = state.config.profiles_dir.join(&sanitized);
    if target_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("Profile '{}' already exists", sanitized) })),
        ));
    }

    if let Some(ref source_name) = req.clone_from {
        let source_sanitized = sanitize_profile_name(source_name);
        let source_dir = state.config.profiles_dir.join(&source_sanitized);
        if !source_dir.exists() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Source profile '{}' to clone not found", source_sanitized) })),
            ));
        }

        if let Err(e) = copy_dir_all(source_dir, target_dir.clone()).await {
            error!(profile = %sanitized, error = %e, "Failed to clone profile directory");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to copy profile directory" })),
            ));
        }
    } else if let Err(e) = fs::create_dir_all(&target_dir).await {
        error!(profile = %sanitized, error = %e, "Failed to create profile directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to create profile directory" })),
        ));
    }

    let meta = target_dir.metadata().ok();
    let size_bytes = compute_dir_size(target_dir).await;
    let created_at_secs = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let modified_at_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    info!(profile = %sanitized, "Profile created");
    Ok((
        StatusCode::CREATED,
        Json(ProfileInfo {
            name: sanitized,
            size_bytes,
            is_active: false,
            session_id: None,
            created_at_secs,
            modified_at_secs,
        }),
    ))
}

async fn api_get_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let sanitized = sanitize_profile_name(&name);
    if sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    let dir = state.config.profiles_dir.join(&sanitized);
    if !dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Profile '{}' not found", sanitized) })),
        ));
    }

    let active_session = {
        let guard = state.sessions.read().await;
        guard
            .values()
            .find(|s| s.profile.as_deref() == Some(&sanitized))
            .cloned()
    };

    let session_id = active_session.as_ref().map(|s| s.id);
    let session_summary = match active_session {
        Some(s) => {
            let is_rec = state.active_recordings.read().await.contains_key(&s.id);
            Some(s.to_summary(&state.http_client, is_rec).await)
        }
        None => None,
    };

    let meta = dir.metadata().ok();
    let size_bytes = compute_dir_size(dir).await;
    let created_at_secs = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let modified_at_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    Ok(Json(ProfileDetails {
        name: sanitized,
        size_bytes,
        is_active: session_id.is_some(),
        session_id,
        created_at_secs,
        modified_at_secs,
        session: session_summary,
    }))
}

async fn api_update_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let old_sanitized = sanitize_profile_name(&name);
    if old_sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    let old_dir = state.config.profiles_dir.join(&old_sanitized);
    if !old_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Profile '{}' not found", old_sanitized) })),
        ));
    }

    let new_name = match req.name {
        Some(n) => sanitize_profile_name(&n),
        None => old_sanitized.clone(),
    };

    if new_name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "New profile name cannot be empty" })),
        ));
    }

    if new_name == old_sanitized {
        let meta = old_dir.metadata().ok();
        let size_bytes = compute_dir_size(old_dir).await;
        let created_at_secs = meta
            .as_ref()
            .and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let modified_at_secs = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let is_active = {
            let guard = state.sessions.read().await;
            guard.values().any(|s| s.profile.as_deref() == Some(&old_sanitized))
        };
        return Ok(Json(ProfileInfo {
            name: old_sanitized,
            size_bytes,
            is_active,
            session_id: None,
            created_at_secs,
            modified_at_secs,
        }));
    }

    // Check if active
    {
        let guard = state.sessions.read().await;
        if let Some(s) = guard.values().find(|s| s.profile.as_deref() == Some(&old_sanitized)) {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Cannot rename active profile (in use by session {})", s.id)
                })),
            ));
        }
    }

    let new_dir = state.config.profiles_dir.join(&new_name);
    if new_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("Profile '{}' already exists", new_name) })),
        ));
    }

    if let Err(e) = fs::rename(&old_dir, &new_dir).await {
        error!(from = %old_sanitized, to = %new_name, error = %e, "Failed to rename profile directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to rename profile directory" })),
        ));
    }

    let meta = new_dir.metadata().ok();
    let size_bytes = compute_dir_size(new_dir).await;
    let created_at_secs = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let modified_at_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    info!(from = %old_sanitized, to = %new_name, "Profile renamed");
    Ok(Json(ProfileInfo {
        name: new_name,
        size_bytes,
        is_active: false,
        session_id: None,
        created_at_secs,
        modified_at_secs,
    }))
}

async fn api_clone_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CloneProfileRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let source_sanitized = sanitize_profile_name(&name);
    let target_sanitized = sanitize_profile_name(&req.target_name);

    if source_sanitized.is_empty() || target_sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    let source_dir = state.config.profiles_dir.join(&source_sanitized);
    if !source_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Source profile '{}' not found", source_sanitized) })),
        ));
    }

    let target_dir = state.config.profiles_dir.join(&target_sanitized);
    if target_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("Target profile '{}' already exists", target_sanitized) })),
        ));
    }

    if let Err(e) = copy_dir_all(source_dir, target_dir.clone()).await {
        error!(from = %source_sanitized, to = %target_sanitized, error = %e, "Failed to clone profile directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to clone profile directory" })),
        ));
    }

    let meta = target_dir.metadata().ok();
    let size_bytes = compute_dir_size(target_dir).await;
    let created_at_secs = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let modified_at_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    info!(from = %source_sanitized, to = %target_sanitized, "Profile cloned");
    Ok((
        StatusCode::CREATED,
        Json(ProfileInfo {
            name: target_sanitized,
            size_bytes,
            is_active: false,
            session_id: None,
            created_at_secs,
            modified_at_secs,
        }),
    ))
}

async fn api_delete_profile(
    AxumPath(name): AxumPath<String>,
    Query(params): Query<DeleteProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let sanitized = sanitize_profile_name(&name);
    if sanitized.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid profile name" })),
        ));
    }

    // Check if profile is active in an ongoing session
    let active_session_id = {
        let guard = state.sessions.read().await;
        guard
            .values()
            .find(|s| s.profile.as_deref() == Some(&sanitized))
            .map(|s| s.id)
    };

    if let Some(sid) = active_session_id {
        if params.force.unwrap_or(false) {
            info!(profile = %sanitized, session_id = %sid, "Force terminating session before deleting profile");
            state.terminate_session(&sid).await;
        } else {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Profile is currently in use by active session {}. Pass ?force=true to terminate session and delete.", sid)
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

// ---------------------------------------------------------------------------
// Peer-to-Peer Mesh Handlers & Proxy APIs
// ---------------------------------------------------------------------------

async fn api_mesh_node(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let sessions_count = state.sessions.read().await.len();
    let profiles_count = match fs::read_dir(&state.config.profiles_dir).await {
        Ok(mut dir) => {
            let mut c = 0;
            while let Ok(Some(e)) = dir.next_entry().await {
                if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    c += 1;
                }
            }
            c
        }
        Err(_) => 0,
    };
    let local_recordings = recorder::list_recordings(&state.recorder_config).await;
    let recordings_count = local_recordings.len();
    let storage_used_bytes: u64 = local_recordings.iter().map(|r| r.size_bytes).sum();
    let max_storage_bytes = state.recorder_config.max_storage_bytes;

    Ok(Json(NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: sessions_count,
        max_sessions: state.max_sessions,
        profiles_count,
        recordings_count,
        storage_used_bytes,
        max_storage_bytes,
    }))
}

async fn api_mesh_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let local_sessions = get_local_sessions(&state).await;
    let local_profiles = get_local_profiles(&state).await;
    let local_targets = get_local_targets(&state).await;
    let local_recordings = recorder::list_recordings(&state.recorder_config).await;
    let local_storage_used_bytes: u64 = local_recordings.iter().map(|r| r.size_bytes).sum();

    let local_node = NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: local_sessions.len(),
        max_sessions: state.max_sessions,
        profiles_count: local_profiles.len(),
        recordings_count: local_recordings.len(),
        storage_used_bytes: local_storage_used_bytes,
        max_storage_bytes: state.recorder_config.max_storage_bytes,
    };

    let peers: Vec<(String, String)> = {
        let guard = state.peers.read().await;
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };

    let client = state.http_client.clone();
    let cluster_secret = state.cluster_secret.clone();
    let mut peer_futures = Vec::new();

    for (p_name, p_url) in peers {
        let c = client.clone();
        let sec = cluster_secret.clone();
        peer_futures.push(tokio::spawn(async move {
            let add_auth = |mut builder: reqwest::RequestBuilder| {
                if let Some(ref s) = sec {
                    builder = builder
                        .bearer_auth(s)
                        .header("X-Browser-Cluster-Secret", s);
                }
                builder
            };
            let t0 = Instant::now();
            let node_url = format!("{}/api/mesh/node", p_url);
            let sessions_url = format!("{}/api/sessions", p_url);
            let profiles_url = format!("{}/api/profiles", p_url);
            let targets_url = format!("{}/json/list", p_url);
            let recordings_url = format!("{}/api/recordings", p_url);

            let node_res = add_auth(c.get(&node_url))
                .timeout(std::time::Duration::from_millis(1800))
                .send()
                .await;
            let latency_ms = t0.elapsed().as_millis() as u64;

            match node_res {
                Ok(res) if res.status().is_success() => {
                    let node_info: Result<NodeInfo, _> = res.json().await;
                    let (node_name, active_sessions, max_sessions, profiles_count, uptime_seconds, recordings_count, storage_used_bytes, max_storage_bytes) =
                        match node_info {
                            Ok(info) => (
                                info.name,
                                info.active_sessions,
                                info.max_sessions,
                                info.profiles_count,
                                info.uptime_seconds,
                                info.recordings_count,
                                info.storage_used_bytes,
                                info.max_storage_bytes,
                            ),
                            Err(_) => (p_name.clone(), 0, 8, 0, 0, 0, 0, 0),
                        };

                    let p_sessions: Vec<SessionSummary> = match add_auth(c.get(&sessions_url))
                        .timeout(std::time::Duration::from_millis(1800))
                        .send()
                        .await
                    {
                        Ok(r) => r.json().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    let p_profiles: Vec<ProfileInfo> = match add_auth(c.get(&profiles_url))
                        .timeout(std::time::Duration::from_millis(1800))
                        .send()
                        .await
                    {
                        Ok(r) => r.json().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    let p_targets: Vec<DevToolsTarget> = match add_auth(c.get(&targets_url))
                        .timeout(std::time::Duration::from_millis(1800))
                        .send()
                        .await
                    {
                        Ok(r) => r.json().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    let p_recordings: Vec<RecordingMeta> = match add_auth(c.get(&recordings_url))
                        .timeout(std::time::Duration::from_millis(1800))
                        .send()
                        .await
                    {
                        Ok(r) => r.json().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };

                    (
                        PeerState {
                            name: node_name.clone(),
                            url: p_url.clone(),
                            status: "online".to_string(),
                            latency_ms: Some(latency_ms),
                            active_sessions,
                            max_sessions,
                            profiles_count,
                            recordings_count,
                            storage_used_bytes,
                            uptime_seconds,
                            error: None,
                        },
                        node_name,
                        p_url,
                        p_sessions,
                        p_profiles,
                        p_targets,
                        p_recordings,
                        max_storage_bytes,
                    )
                }
                Ok(res) => (
                    PeerState {
                        name: p_name.clone(),
                        url: p_url.clone(),
                        status: "unreachable".to_string(),
                        latency_ms: Some(latency_ms),
                        active_sessions: 0,
                        max_sessions: 8,
                        profiles_count: 0,
                        recordings_count: 0,
                        storage_used_bytes: 0,
                        uptime_seconds: 0,
                        error: Some(format!("HTTP {}", res.status())),
                    },
                    p_name,
                    p_url,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    0,
                ),
                Err(err) => (
                    PeerState {
                        name: p_name.clone(),
                        url: p_url.clone(),
                        status: "unreachable".to_string(),
                        latency_ms: None,
                        active_sessions: 0,
                        max_sessions: 8,
                        profiles_count: 0,
                        recordings_count: 0,
                        storage_used_bytes: 0,
                        uptime_seconds: 0,
                        error: Some(err.to_string()),
                    },
                    p_name,
                    p_url,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    0,
                ),
            }
        }));
    }

    let mut peer_states = Vec::new();
    let mut all_sessions = Vec::new();
    let mut all_profiles = Vec::new();
    let mut all_targets = Vec::new();
    let mut all_recordings = Vec::new();

    // Add local items
    for s in local_sessions {
        all_sessions.push(MeshSession {
            session: s,
            node_name: local_node.name.clone(),
            node_url: local_node.url.clone(),
            is_local: true,
        });
    }

    for p in local_profiles {
        all_profiles.push(MeshProfile {
            profile: p,
            node_name: local_node.name.clone(),
            node_url: local_node.url.clone(),
            is_local: true,
        });
    }

    for t in local_targets {
        all_targets.push(MeshTarget {
            target: t,
            node_name: local_node.name.clone(),
            node_url: local_node.url.clone(),
            is_local: true,
        });
    }

    for r in local_recordings {
        all_recordings.push(MeshRecording {
            recording: r,
            node_name: local_node.name.clone(),
            node_url: local_node.url.clone(),
            is_local: true,
        });
    }

    let mut total_capacity = local_node.max_sessions;
    let mut online_nodes = 1;
    let mut total_storage_bytes = local_node.storage_used_bytes;
    let mut max_storage_bytes = local_node.max_storage_bytes;

    for task in peer_futures {
        if let Ok((p_state, node_name, node_url, p_sessions, p_profiles, p_targets, p_recordings, p_max_storage)) = task.await {
            if p_state.status == "online" {
                online_nodes += 1;
                total_capacity += p_state.max_sessions;
                total_storage_bytes += p_state.storage_used_bytes;
                max_storage_bytes += p_max_storage;
            }
            peer_states.push(p_state);

            for s in p_sessions {
                all_sessions.push(MeshSession {
                    session: s,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                });
            }
            for p in p_profiles {
                all_profiles.push(MeshProfile {
                    profile: p,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                });
            }
            for t in p_targets {
                all_targets.push(MeshTarget {
                    target: t,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                });
            }
            for r in p_recordings {
                all_recordings.push(MeshRecording {
                    recording: r,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                });
            }
        }
    }

    all_recordings.sort_by(|a, b| b.recording.started_at_secs.cmp(&a.recording.started_at_secs));

    let total_nodes = 1 + peer_states.len();
    let total_sessions = all_sessions.len();
    let total_profiles = all_profiles.len();
    let total_recordings = all_recordings.len();

    Ok(Json(MeshOverview {
        local_node,
        peers: peer_states,
        sessions: all_sessions,
        profiles: all_profiles,
        targets: all_targets,
        recordings: all_recordings,
        total_sessions,
        total_capacity,
        total_profiles,
        total_recordings,
        total_nodes,
        online_nodes,
        total_storage_bytes,
        max_storage_bytes,
    }))
}

async fn get_peer_url(state: &AppState, peer: &str) -> Option<String> {
    let guard = state.peers.read().await;
    if let Some(u) = guard.get(peer) {
        return Some(u.clone());
    }
    guard.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case(peer) || v.contains(peer) {
            Some(v.clone())
        } else {
            None
        }
    })
}

async fn proxy_response(res_result: Result<reqwest::Response, reqwest::Error>) -> Response {
    match res_result {
        Ok(res) => {
            let status =
                StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = res.bytes().await.unwrap_or_default();
            (
                status,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("Peer communication failed: {}", e) })),
        )
            .into_response(),
    }
}

async fn api_list_peers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let peers_guard = state.peers.read().await;
    let peer_list: Vec<serde_json::Value> = peers_guard
        .iter()
        .map(|(k, v)| {
            serde_json::json!({
                "name": k,
                "url": v,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "local": {
            "name": state.node_name,
            "url": state.public_url,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": state.start_time.elapsed().as_secs(),
        },
        "peers": peer_list,
    })))
}

async fn api_add_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AddPeerRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let trimmed = req.url.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Peer URL cannot be empty" })),
        )
            .into_response();
    }

    if trimmed == state.public_url.trim_end_matches('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Cannot peer with self" })),
        )
            .into_response();
    }

    let ping_url = format!("{}/api/mesh/node", trimmed);
    let detected_name = match state
        .peer_request(state.http_client.get(&ping_url))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res
            .json::<NodeInfo>()
            .await
            .map(|info| info.name)
            .unwrap_or_else(|_| {
                infer_peer_name(&trimmed, &state.node_name)
                    .unwrap_or_else(|| req.name.clone().unwrap_or_else(|| "peer".to_string()))
            }),
        _ => infer_peer_name(&trimmed, &state.node_name)
            .unwrap_or_else(|| req.name.unwrap_or_else(|| "peer".to_string())),
    };

    if detected_name == state.node_name {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Cannot peer with self" })),
        )
            .into_response();
    }

    {
        let mut guard = state.peers.write().await;
        guard.insert(detected_name.clone(), trimmed.clone());
    }

    let my_url = state.public_url.clone();
    let my_name = state.node_name.clone();
    let peer_announce_url = format!("{}/api/peers", trimmed);
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _ = state_clone
            .peer_request(state_clone.http_client.post(&peer_announce_url))
            .json(&serde_json::json!({ "url": my_url, "name": my_name }))
            .send()
            .await;
    });

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "status": "ok", "name": detected_name, "url": trimmed })),
    )
        .into_response()
}

async fn api_remove_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let mut guard = state.peers.write().await;
    let removed = guard.remove(&peer).is_some() || {
        let key_to_remove = guard.iter().find_map(|(k, v)| {
            if v == &peer || k.eq_ignore_ascii_case(&peer) {
                Some(k.clone())
            } else {
                None
            }
        });
        if let Some(k) = key_to_remove {
            guard.remove(&k).is_some()
        } else {
            false
        }
    };

    if removed {
        Json(serde_json::json!({ "status": "removed" })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Peer not found" })),
        )
            .into_response()
    }
}

async fn api_peer_create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
    Json(req): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/sessions", peer_url);
    let res = state
        .peer_request(state.http_client.post(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_delete_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, id)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/sessions/{}", peer_url, id);
    let res = state
        .peer_request(state.http_client.delete(&url))
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_create_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
    Json(req): Json<CreateProfileRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/profiles", peer_url);
    let res = state
        .peer_request(state.http_client.post(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_delete_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    Query(params): Query<DeleteProfileParams>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let mut url = format!("{}/api/profiles/{}", peer_url, name);
    if let Some(f) = params.force {
        url.push_str(&format!("?force={}", f));
    }
    let res = state
        .peer_request(state.http_client.delete(&url))
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_clone_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    Json(req): Json<CloneProfileRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/profiles/{}/clone", peer_url, name);
    let res = state
        .peer_request(state.http_client.post(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_update_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    Json(req): Json<UpdateProfileRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/profiles/{}", peer_url, name);
    let res = state
        .peer_request(state.http_client.patch(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

// ---------------------------------------------------------------------------
// Recording & Auto-Vacuum APIs
// ---------------------------------------------------------------------------

async fn api_list_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let list = recorder::list_recordings(&state.recorder_config).await;
    Ok(Json(list))
}

async fn api_get_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let rec = recorder::get_recording(&state.recorder_config, id)
        .await
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))?;
    Ok(Json(rec))
}

async fn api_get_recording_frames(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let frames_path = state.recorder_config.recordings_dir.join(id.to_string()).join("frames.jsonl");
    if !frames_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Frames not found for recording" })),
        ));
    }
    match fs::read(&frames_path).await {
        Ok(data) => Ok((
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            data,
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to read frames: {}", e) })),
        )),
    }
}

async fn api_delete_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if recorder::delete_recording(&state.recorder_config, id).await {
        Ok(Json(serde_json::json!({ "status": "deleted", "id": id })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))
    }
}

async fn api_vacuum_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let stats = recorder::vacuum_recordings(&state.recorder_config).await;
    Ok(Json(stats))
}

async fn api_session_start_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let session = {
        let guard = state.sessions.read().await;
        guard.get(&id).cloned().ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ))?
    };

    if state.active_recordings.read().await.contains_key(&id) {
        return Ok(Json(serde_json::json!({ "status": "already_recording", "session_id": id })));
    }

    let handle = recorder::start_recording(
        session.id,
        session.port,
        session.profile.clone(),
        state.node_name.clone(),
        (*state.recorder_config).clone(),
    );
    let rec_id = handle.recording_id;
    state.active_recordings.write().await.insert(session.id, handle);
    Ok(Json(serde_json::json!({
        "status": "recording_started",
        "session_id": id,
        "recording_id": rec_id
    })))
}

async fn api_session_stop_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if let Some(handle) = state.active_recordings.write().await.remove(&id) {
        handle.stop();
        Ok(Json(serde_json::json!({
            "status": "recording_stopped",
            "session_id": id,
            "recording_id": handle.recording_id
        })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session is not actively recording" })),
        ))
    }
}

async fn api_peer_list_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/recordings", peer_url);
    let res = state.peer_request(state.http_client.get(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_get_recording(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, id)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/recordings/{}", peer_url, id);
    let res = state.peer_request(state.http_client.get(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_get_recording_frames(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, id)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/recordings/{}/frames", peer_url, id);
    let res = state.peer_request(state.http_client.get(&url)).send().await;
    match res {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = r.bytes().await.unwrap_or_default();
            (
                status,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                )],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("Peer communication failed: {}", e) })),
        )
            .into_response(),
    }
}

async fn api_peer_delete_recording(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, id)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/recordings/{}", peer_url, id);
    let res = state.peer_request(state.http_client.delete(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_vacuum_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let peer_url = match get_peer_url(&state, &peer).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
            )
                .into_response();
        }
    };
    let url = format!("{}/api/recordings/vacuum", peer_url);
    let res = state.peer_request(state.http_client.post(&url)).send().await;
    proxy_response(res).await
}

async fn copy_dir_all(src: PathBuf, dst: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        fn copy_all(src: &Path, dst: &Path) -> std::io::Result<()> {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                let dest_path = dst.join(entry.file_name());
                if file_type.is_dir() {
                    copy_all(&entry.path(), &dest_path)?;
                } else if file_type.is_file() {
                    std::fs::copy(entry.path(), dest_path)?;
                }
            }
            Ok(())
        }
        copy_all(&src, &dst)
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
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
