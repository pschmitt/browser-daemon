mod browser;
mod proxy;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::browser::{ActiveBrowser, BrowserConfig};
use crate::proxy::bridge_websockets;

static HANDOFF_HTML: &str = include_str!("../handoff.html");

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

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    #[serde(skip)]
    pub created_at: Instant,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<BrowserConfig>,
    pub max_sessions: usize,
    pub sessions: Arc<RwLock<HashMap<Uuid, SessionInfo>>>,
    pub http_client: reqwest::Client,
}

#[derive(Deserialize, Debug)]
pub struct LaunchParams {
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
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
        .route("/", get(|| async { Redirect::to("/handoff/") }))
        .route("/health", get(health_check))
        .route("/json/version", get(json_version))
        .route("/json", get(json_list))
        .route("/json/list", get(json_list))
        .route("/handoff/", get(handoff_page))
        .route("/handoff", get(|| async { Redirect::to("/handoff/") }))
        .route("/chromium", get(handle_chromium_ws))
        .route("/devtools/page/{page_id}", get(handle_page_ws))
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
    let sessions = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect::<Vec<_>>()
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
    let active_count = state.sessions.read().await.len();
    if active_count >= state.max_sessions {
        warn!(active = active_count, max = state.max_sessions, "Session limit reached");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Session limit reached. Try again later.",
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| async move {
        let mut browser = match ActiveBrowser::spawn(
            &state.config,
            params.profile.as_deref(),
            params.width,
            params.height,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "Failed to spawn browser instance");
                return;
            }
        };

        let session_info = SessionInfo {
            id: browser.id,
            port: browser.port,
            ws_path: browser.ws_path.clone(),
            profile: browser.profile_name.clone(),
            created_at: browser.created_at,
        };

        {
            let mut guard = state.sessions.write().await;
            guard.insert(browser.id, session_info);
        }

        let ws_path = browser.ws_path.trim_start_matches('/');
        let backend_ws_url = format!("ws://127.0.0.1:{}/{}", browser.port, ws_path);
        info!(session_id = %browser.id, url = %backend_ws_url, "Connecting to Chromium DevTools");

        match tokio_tungstenite::connect_async(&backend_ws_url).await {
            Ok((server_ws, _)) => {
                bridge_websockets(socket, server_ws).await;
            }
            Err(e) => {
                error!(session_id = %browser.id, error = %e, "Failed to connect to Chromium backend");
            }
        }

        {
            let mut guard = state.sessions.write().await;
            guard.remove(&browser.id);
        }

        browser.terminate().await;
        info!(session_id = %browser.id, "Session cleaned up");
    })
}

async fn handle_page_ws(
    AxumPath(page_id): AxumPath<String>,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    let sessions = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect::<Vec<_>>()
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
