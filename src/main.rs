mod api;
mod audible;
mod audio;
mod auth;
mod autofill;
mod backups;
mod bidi;
mod browser;
mod cdp_ws;
mod clients;
mod ctl;
mod display;
mod favicon;
mod history;
mod idle;
mod json_api;
mod mcp;
mod mesh;
mod names;
mod page_input;
mod pages;
mod passkey_helper;
mod passkeys;
mod persist;
mod proc;
mod profile_transfer;
mod profiles;
mod profiles_api;
mod proxies;
mod proxy;
mod rec_audio;
mod rec_export;
mod recorder;
mod relay;
mod secrets;
mod sessions;
mod settings;
mod state;
mod thumbnail;
mod tui;
mod vault_index;
mod video;
mod viewers;

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::api::*;
use crate::bidi::BidiHub;
use crate::browser::{ActiveBrowser, BrowserConfig, BrowserEngine, SessionState};
use crate::cdp_ws::*;
use crate::json_api::*;
use crate::mesh::*;
use crate::pages::*;
use crate::profile_transfer::*;
use crate::profiles::{ColorScheme, Profile};
use crate::profiles_api::*;
use crate::proxy::bridge_websockets;
use crate::recorder::{RecorderConfig, RecordingMeta, SessionRecorderHandle};
use crate::sessions::*;
use crate::state::*;

/// Git commit this build comes from, passed in by the Nix build.
pub const REVISION: Option<&str> = option_env!("BROWSER_DAEMON_REV");

/// `--version`: the crate's version and, when known, the git commit.
static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| match REVISION {
    Some(rev) => format!("{} ({rev})", env!("CARGO_PKG_VERSION")),
    None => env!("CARGO_PKG_VERSION").to_string(),
});

/// `browser-daemon <command>`: the daemon itself (`run`) and clients of its API.
#[derive(Parser, Debug)]
#[command(
    author,
    version = VERSION.as_str(),
    about = "Native NixOS remote browser daemon, and its command-line client"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Options of `browser-daemon run`.
#[derive(clap::Args, Debug)]
pub struct Args {
    #[arg(short, long, default_value = "127.0.0.1:3001")]
    pub bind: SocketAddr,

    #[arg(long, default_value = "chromium")]
    pub chromium: PathBuf,

    #[arg(long)]
    pub firefox: Option<PathBuf>,

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

    /// A `Host` (HOST[:PORT]) the node answers to besides its public URL's and
    /// loopback's (repeat for several); other hosts get 403.
    #[arg(long = "allowed-host")]
    pub allowed_hosts: Vec<String>,

    #[arg(long, default_value = "/var/lib/browser-daemon/recordings")]
    pub recordings_dir: PathBuf,

    /// Record new sessions unless they opt out (`--record-sessions false` to turn off).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub record_sessions: bool,

    /// Record the session's sound with its recording (needs --pulse-server); a
    /// recording can still be started without (`POST .../record?audio=false`).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub record_audio: bool,

    #[arg(long, default_value_t = 2048)]
    pub recording_max_storage_mb: u64,

    #[arg(long, default_value_t = 168)]
    pub recording_max_age_hours: u64,

    #[arg(long, default_value_t = 100)]
    pub recording_max_count: usize,

    #[arg(
        long,
        help = "Maximum memory per Chromium browser process in MB (sets --js-flags=--max-old-space-size=N)"
    )]
    pub max_session_memory_mb: Option<u64>,

    #[arg(
        long,
        default_value_t = 30,
        help = "Stop sessions nobody used for this many minutes (0 = never)"
    )]
    pub idle_timeout_minutes: u64,

    #[arg(
        long,
        help = "Unpacked ad blocker extension (uBlock Origin Lite) for Chromium"
    )]
    pub chromium_adblock: Option<PathBuf>,

    #[arg(long, help = "Ad blocker add-on (uBlock Origin .xpi) for Firefox")]
    pub firefox_adblock: Option<PathBuf>,

    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        help = "Block ads in new sessions unless they opt out"
    )]
    pub adblock_default: bool,

    #[arg(
        long,
        help = "JSON list of upstream proxies sessions can use: [{\"id\", \"name\", \"url\"}]"
    )]
    pub proxies_file: Option<PathBuf>,

    #[arg(
        long,
        help = "PulseAudio socket for session audio (per-session sinks, streamed on /audio)"
    )]
    pub pulse_server: Option<PathBuf>,

    #[arg(long, default_value = "pactl")]
    pub pactl: PathBuf,

    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = DisplayMode::Shared,
        help = "X display per session (own Xvfb, needed for video streaming) or one shared --display"
    )]
    pub display_mode: DisplayMode,

    #[arg(
        long,
        default_value = "Xvfb",
        help = "Xvfb binary for --display-mode per-session"
    )]
    pub xvfb: PathBuf,

    #[arg(long, help = "xdotool, to park the X pointer on per-session displays")]
    pub xdotool: Option<PathBuf>,

    #[arg(
        long,
        value_enum,
        help = "Offer the live view as H.264 video (needs --display-mode per-session)"
    )]
    pub video_encoder: Option<VideoEncoderArg>,

    #[arg(
        long,
        default_value = "ffmpeg",
        help = "ffmpeg with x11grab (ffmpeg-full) for --video-encoder"
    )]
    pub video_ffmpeg: PathBuf,

    #[arg(long, default_value = "/dev/dri/renderD128")]
    pub vaapi_device: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value = "none",
        help = "Render Chromium on the GPU: vulkan (ANGLE on Vulkan), or none (software)"
    )]
    pub chromium_gpu: ChromiumGpuArg,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromiumGpuArg {
    None,
    Vulkan,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEncoderArg {
    Vaapi,
    X264,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayMode {
    Shared,
    PerSession,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the daemon (what the systemd service does).
    Run(Box<Args>),
    /// Browser sessions: list, start, stop, record, tabs.
    #[command(visible_alias = "sessions")]
    Session(ctl::SessionArgs),
    /// Profiles: list, create, back up, clear, export, import, move between nodes.
    #[command(visible_alias = "profiles")]
    Profile(ctl::ProfileArgs),
    /// Recordings: list, export, delete.
    #[command(visible_alias = "recordings")]
    Recording(ctl::RecordingArgs),
    /// The mesh's nodes: list, show, add, remove.
    #[command(visible_alias = "nodes")]
    Node(ctl::NodeArgs),
    /// Terminal UI for the mesh: sessions, profiles, recordings and nodes.
    Tui(ctl::Conn),
    /// MCP server for AI agents: one browser tool set for every node and profile.
    Mcp(mcp::McpArgs),
    /// Print a shell completion script (bash, zsh, fish, ...).
    Completions { shell: clap_complete::Shell },
    /// Serve passkeys and vault logins approved in live views from the local rbw vault.
    PasskeyHelper(passkey_helper::HelperArgs),
}

/// The latest mesh overview, as the JSON everybody is sent.
pub struct OverviewSnapshot {
    at: Instant,
    json: String,
}

#[derive(Default)]
pub struct OverviewHub {
    latest: tokio::sync::watch::Sender<Option<Arc<OverviewSnapshot>>>,
    /// One computation at a time; the ones waiting reuse its result.
    compute: Mutex<()>,
}

impl AppState {
    /// The mesh overview, at most `max_age` old: dashboards, the CLI and the
    /// TUI share one computation instead of each walking every node.
    pub async fn overview_snapshot(&self, max_age: Duration) -> Arc<OverviewSnapshot> {
        let recent = |hub: &OverviewHub| {
            hub.latest
                .borrow()
                .clone()
                .filter(|s| s.at.elapsed() <= max_age)
        };
        if let Some(s) = recent(&self.overview) {
            return s;
        }
        let _computing = self.overview.compute.lock().await;
        if let Some(s) = recent(&self.overview) {
            return s;
        }
        let at = Instant::now();
        let overview = fetch_mesh_overview(self).await;
        let snapshot = Arc::new(OverviewSnapshot {
            at,
            json: serde_json::to_string(&overview).unwrap_or_default(),
        });
        // Whoever listens (the overview sockets) is woken.
        self.overview.latest.send_replace(Some(snapshot.clone()));
        snapshot
    }
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerState {
    pub name: String,
    pub url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub latency_ms: Option<u64>,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    #[serde(default)]
    pub recordings_count: usize,
    #[serde(default)]
    pub storage_used_bytes: u64,
    pub uptime_seconds: u64,
    #[serde(default)]
    pub supported_engines: Vec<BrowserEngine>,
    #[serde(default)]
    pub proxies: Vec<proxies::ProxyInfo>,
    /// Its settings (absent when unreachable, or from a node without them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<settings::NodeSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            if host.starts_with(&format!("browser.{}.", self_node))
                || host == format!("browser.{}", self_node)
            {
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

/// Report a failed client command on one line and exit non-zero.
fn ctl_exit(
    result: Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(ctl::exit_code(e.as_ref()));
    }
    Ok(())
}

/// A small JPEG of the tab the session is showing (the dashboard's cards).
async fn api_session_screenshot(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(session) = state.sessions.read().await.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let jpeg = match state.thumbnails.fresh(id) {
        Some(jpeg) => Some(jpeg),
        None => {
            let shot = match session.bidi {
                Some(ref hub) => match hub.active_context().await {
                    Some(ctx) => hub.screenshot_jpeg(&ctx).await.ok().and_then(|b64| {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.decode(b64).ok()
                    }),
                    None => None,
                },
                None => thumbnail::chromium(&state.http_client, session.port).await,
            };
            if let Some(ref jpeg) = shot {
                state.thumbnails.store(id, jpeg.clone());
            }
            shot.or_else(|| state.thumbnails.stale(id))
        }
    };
    match jpeg {
        Some(jpeg) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            jpeg,
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Whose history a session's pages belong to: its profile's, else its own.
fn history_key(profile_id: Option<Uuid>, session_id: Uuid) -> String {
    profile_id.map_or_else(
        || format!("{}{session_id}", history::SESSION_PREFIX),
        |p| p.to_string(),
    )
}

#[derive(Deserialize)]
struct HistoryParams {
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Suggestion {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// "tab": one of the profile's default tabs; "history": a page visited before.
    kind: &'static str,
}

/// Addresses to suggest in a session's address bar: the profile's default tabs,
/// then the pages visited in the profile (or the session).
async fn api_session_history(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<HistoryParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile_id = {
        let guard = state.sessions.read().await;
        guard
            .get(&id)
            .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?
            .profile_id
    };
    let query = params.q.unwrap_or_default().trim().to_lowercase();
    let limit = params.limit.unwrap_or(8).clamp(1, 50);
    let mut out: Vec<Suggestion> = Vec::new();
    if let Some(pid) = profile_id {
        if let Some(profile) = profiles::find(&state.config.profiles_dir, &pid.to_string()).await {
            for tab in profile.meta.tabs {
                if query.is_empty() || tab.to_lowercase().contains(&query) {
                    out.push(Suggestion {
                        url: tab,
                        title: None,
                        kind: "tab",
                    });
                }
            }
        }
    }
    for e in state
        .history
        .suggest(&history_key(profile_id, id), &query, limit)
    {
        match out.iter_mut().find(|o| o.url == e.url) {
            Some(known) => known.title = e.title,
            None => out.push(Suggestion {
                url: e.url,
                title: e.title,
                kind: "history",
            }),
        }
    }
    out.truncate(limit);
    Ok(Json(out))
}

/// Half-unpacked imports a crash or a restart left behind.
async fn remove_stale_imports(profiles_dir: &Path) {
    let Ok(mut entries) = fs::read_dir(profiles_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(IMPORT_PREFIX)
        {
            let _ = fs::remove_dir_all(entry.path()).await;
        }
    }
}

fn meta_profile(profile: &Profile, meta: profiles::ProfileMeta) -> Profile {
    Profile {
        meta,
        dir: profile.dir.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = Cli::parse().command;
    if !matches!(command, Command::Run(_)) {
        // `browser-daemon session list | head`: end quietly instead of panicking.
        // SAFETY: restoring the default disposition of SIGPIPE.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let args = match command {
        Command::Run(args) => *args,
        Command::Session(a) => return ctl_exit(ctl::session(a).await),
        Command::Profile(a) => return ctl_exit(ctl::profile(a).await),
        Command::Recording(a) => return ctl_exit(ctl::recording(a).await),
        Command::Node(a) => return ctl_exit(ctl::node(a).await),
        // stdout carries the protocol: the log goes to stderr, unset.
        Command::Mcp(a) => return mcp::run(a).await,
        Command::Completions { shell } => {
            ctl::completions(shell);
            return Ok(());
        }
        // The TUI owns the terminal: no log output.
        Command::Tui(conn) => return ctl_exit(tui::run(conn).await),
        Command::PasskeyHelper(helper_args) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "browser_daemon=info".into()),
                )
                .init();
            return passkey_helper::run(helper_args).await;
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "browser_daemon=info,tower_http=info".into()),
        )
        .init();

    let node_name = args.node_name.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| "node".to_string())
    });

    let public_url = args
        .public_url
        .unwrap_or_else(|| format!("https://browser.{}.ts.brkn.lol", node_name));

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
                eprintln!(
                    "Failed to read cluster secret file {}: {}",
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        }
    } else {
        if args.cluster_secret.is_some() {
            warn!(
                "--cluster-secret shows the secret in the process list: use --cluster-secret-file"
            );
        }
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

    let chromium_adblock = match args.chromium_adblock.clone() {
        Some(src) => {
            let profile_dir = args.profile_dir.clone();
            tokio::task::spawn_blocking(move || writable_extension_copy(&src, &profile_dir))
                .await
                .ok()
                .flatten()
        }
        None => None,
    };
    profiles::migrate(&args.profile_dir).await;
    remove_stale_imports(&args.profile_dir).await;
    let proxies = match args.proxies_file {
        Some(ref path) => match proxies::load(path) {
            Ok(list) => list,
            Err(e) => {
                eprintln!("Invalid proxies file: {e}");
                std::process::exit(1);
            }
        },
        None => Vec::new(),
    };
    // Next to the profiles directory (the state directory).
    let settings_path = args
        .profile_dir
        .parent()
        .unwrap_or(&args.profile_dir)
        .join("node-settings.json");
    let config = Arc::new(BrowserConfig {
        chromium_bin: args.chromium,
        firefox_bin: args.firefox,
        display: args.display,
        profiles_dir: args.profile_dir,
        default_width: args.default_width,
        default_height: args.default_height,
        max_session_memory_mb: args.max_session_memory_mb,
        chromium_adblock,
        firefox_adblock: args.firefox_adblock,
        proxies,
        chromium_gpu: args.chromium_gpu == ChromiumGpuArg::Vulkan,
        video: match (args.video_encoder, args.display_mode) {
            (Some(enc), DisplayMode::PerSession) => Some(video::VideoConfig {
                ffmpeg: args.video_ffmpeg.clone(),
                encoder: match enc {
                    VideoEncoderArg::Vaapi => video::Encoder::Vaapi(args.vaapi_device.clone()),
                    VideoEncoderArg::X264 => video::Encoder::X264,
                },
            }),
            (Some(_), DisplayMode::Shared) => {
                eprintln!("--video-encoder needs --display-mode per-session; video disabled");
                None
            }
            _ => None,
        },
        session_displays: (args.display_mode == DisplayMode::PerSession).then(|| {
            display::DisplayConfig {
                xvfb: args.xvfb.clone(),
                xdotool: args.xdotool.clone(),
            }
        }),
        audio: args.pulse_server.clone().map(|server| audio::AudioConfig {
            server,
            pactl: args.pactl.clone(),
            ffmpeg: args.ffmpeg.clone(),
        }),
    });

    let recorder_config = Arc::new(RecorderConfig {
        enabled: args.record_sessions,
        recordings_dir: args.recordings_dir.clone(),
        max_storage_bytes: args.recording_max_storage_mb * 1024 * 1024,
        max_age_secs: args.recording_max_age_hours * 3600,
        max_count: args.recording_max_count,
        audio: args.record_audio,
        ffmpeg: Some(args.ffmpeg.clone()),
    });
    let active_recordings = Arc::new(RwLock::new(HashMap::new()));
    let orphaned = recorder::mark_orphaned_recordings(&recorder_config).await;
    if orphaned > 0 {
        info!(
            count = orphaned,
            "Marked recordings of a previous run as interrupted"
        );
    }

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

    let settings = Arc::new(settings::Store::load(&settings_path));
    let history = Arc::new(history::History::new(config.state_dir().join("history")));
    let thumbnails = Arc::new(thumbnail::Thumbnails::default());
    let hosts = auth::HostPolicy::new(&public_url, &args.allowed_hosts, args.bind.port());
    // The browsers must not reach the daemon (or its mesh) through their relay.
    relay::deny_hosts(hosts.names());
    for url in peer_map.values() {
        relay::deny_url(url);
    }
    let auth = Arc::new(auth::Auth::new(cluster_secret.clone(), hosts));
    let state = AppState {
        config,
        max_sessions: args.max_sessions,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?,
        peer_client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?,
        node_name,
        public_url,
        peers: Arc::new(RwLock::new(peer_map)),
        start_time: Instant::now(),
        auth,
        cluster_secret,
        recorder_config,
        active_recordings,
        idle_timeout_secs: idle::minutes_to_secs(args.idle_timeout_minutes),
        adblock_default: args.adblock_default,
        profiles_lock: Arc::new(Mutex::new(())),
        tab_order: Arc::new(std::sync::Mutex::new(TabOrder::default())),
        audible_pages: Arc::default(),
        viewers: Arc::default(),
        passkeys: Arc::default(),
        settings,
        history,
        thumbnails,
        starting: Arc::default(),
        profile_locks: Arc::default(),
        dir_sizes: Arc::default(),
        overview: Arc::default(),
    };
    // Sessions of the previous run keep their browsers (the service does not
    // kill them on a restart): adopt them before anything else touches the nodes.
    state.adopt_sessions().await;
    {
        let adopted = snapshot_sessions(&state).await;
        let mut keep = std::collections::HashSet::new();
        for session in &adopted {
            if let Some(b) = session.browser.lock().await.as_ref() {
                keep.insert(b.proc.pid);
                if let Some(d) = b.display.as_ref() {
                    keep.insert(d.identity().0);
                }
            }
        }
        let swept = proc::sweep_orphans(&keep);
        if swept > 0 {
            warn!(
                count = swept,
                "Killed browsers and displays no session record claimed"
            );
        }
        let mut files = std::collections::HashSet::new();
        for session in &adopted {
            if let Some(b) = session.browser.lock().await.as_ref() {
                files.extend(b.files().cloned());
            }
        }
        let removed = proc::sweep_leftover_files(&persist::dir(state.config.state_dir()), &files);
        if removed > 0 {
            info!(
                count = removed,
                "Removed temporary profiles and logs no session claimed"
            );
        }
    }
    if let Some(ref cfg) = state.config.audio {
        let keep: std::collections::HashSet<String> = state
            .sessions
            .read()
            .await
            .values()
            .filter_map(|s| s.audio_sink.clone())
            .collect();
        audio::remove_stale_sinks(cfg, &keep).await;
    }
    let saver = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let sessions: Vec<ManagedSession> =
                saver.sessions.read().await.values().cloned().collect();
            for session in sessions {
                saver.persist_session(&session).await;
            }
            saver.history.flush().await;
        }
    });
    // Note the pages the tabs are on, for the address bar's suggestions.
    let tracker = state.clone();
    tokio::spawn(async move {
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_secs(4));
        loop {
            interval.tick().await;
            let targets = get_local_targets(&tracker).await;
            let sessions = tracker.sessions.read().await;
            let mut live = std::collections::HashSet::new();
            for t in targets.iter().filter(|t| t.target_type == "page") {
                let Some(session) = t.session_id.and_then(|sid| sessions.get(&sid)) else {
                    continue;
                };
                live.insert(t.id.clone());
                let arrived = seen.get(&t.id) != Some(&t.url);
                tracker.history.record(
                    &history_key(session.profile_id, session.id),
                    &t.url,
                    t.title.as_deref(),
                    arrived,
                );
                if arrived {
                    seen.insert(t.id.clone(), t.url.clone());
                }
            }
            seen.retain(|id, _| live.contains(id));
        }
    });
    audible::spawn_monitor(state.clone());

    // The overview every connected dashboard gets, computed once per tick and only
    // while somebody listens.
    let overview_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if overview_state.overview.latest.receiver_count() > 0 {
                overview_state
                    .overview_snapshot(Duration::from_millis(1500))
                    .await;
            }
        }
    });

    // Stop sessions that nobody has used for a while.
    let reaper_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let expired: Vec<(Uuid, u64)> = reaper_state
                .sessions
                .read()
                .await
                .values()
                .filter(|s| s.activity.expired())
                .map(|s| (s.id, s.activity.idle_secs()))
                .collect();
            for (id, idle) in expired {
                info!(session_id = %id, idle_secs = idle, "Stopping idle session");
                reaper_state.terminate_session(&id).await;
            }
        }
    });

    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/dashboard", get(|| async { Redirect::to("/") }))
        .route("/favicon.ico", get(favicon_ico))
        .route("/favicon.svg", get(favicon_svg))
        .route("/handoff/favicon.ico", get(handoff_favicon_ico))
        .route("/handoff/favicon.svg", get(handoff_favicon_svg))
        .route("/manifest.webmanifest", get(manifest_webmanifest))
        .route("/manifest.json", get(manifest_webmanifest))
        .route("/site.webmanifest", get(manifest_webmanifest))
        .route("/sw.js", get(service_worker_js))
        .route("/icon-192.png", get(icon_192_png))
        .route("/icon-512.png", get(icon_512_png))
        .route("/download-worker.js", get(download_worker_js))
        .route("/health", get(health_check))
        .route("/api/auth/token", get(api_auth_token))
        .route("/json/version", get(json_version))
        .route("/json", get(json_list))
        .route("/json/list", get(json_list))
        // PUT only, as Chromium's own: a GET (an <img src>) must not open tabs.
        .route("/json/new", put(json_new))
        .route("/json/close/{id}", put(json_close))
        .route("/json/activate/{id}", put(json_activate))
        .route("/handoff/", get(handoff_page))
        .route("/handoff", get(|| async { Redirect::to("/handoff/") }))
        .route("/chromium", get(handle_chromium_ws))
        .route("/firefox", get(handle_firefox_ws))
        .route("/devtools/page/{page_id}", get(handle_page_ws))
        .route(
            "/devtools/frontend/{session_id}/{*path}",
            get(devtools_frontend),
        )
        .route("/audio", get(handle_audio_ws))
        .route("/video", get(handle_video_ws))
        .route("/viewers", get(handle_viewers_ws))
        // REST Management APIs for sessions and profiles
        .route(
            "/api/sessions",
            get(api_list_sessions).post(api_create_session),
        )
        .route(
            "/api/sessions/{id}",
            get(api_get_session).delete(api_delete_session),
        )
        .route("/api/proxies", get(api_list_proxies))
        .route(
            "/api/settings",
            get(api_get_settings).patch(api_update_settings),
        )
        .route("/api/sessions/{id}/proxy", post(api_session_set_proxy))
        .route("/api/sessions/{id}/passkey", post(api_session_passkey))
        .route(
            "/api/pages/{page_id}/insert-text",
            post(api_page_insert_text),
        )
        .route("/api/sessions/{id}/autofill", post(api_session_autofill))
        .route("/api/pages/{page_id}/autofill", post(api_page_autofill))
        .route("/api/sessions/{id}/name", post(api_session_rename))
        .route("/api/sessions/{id}/history", get(api_session_history))
        .route("/api/sessions/{id}/screenshot", get(api_session_screenshot))
        .route(
            "/api/sessions/{id}/save-as-profile",
            post(api_session_save_as_profile),
        )
        .route("/api/sessions/{id}/popups", get(api_session_popups))
        .route("/api/sessions/{id}/x-input", post(api_session_x_input))
        .route("/api/agent-topic", post(api_agent_topic))
        .route("/api/passkey/{req}", get(api_passkey_status))
        .route("/api/passkey-helper/next", get(api_passkey_helper_next))
        .route("/api/passkey-helper/{req}", post(api_passkey_helper_done))
        .route(
            "/api/passkey-helper/{req}/note",
            post(api_passkey_helper_note),
        )
        .route("/api/sessions/{id}/record", post(api_session_start_record))
        .route(
            "/api/sessions/{id}/files",
            put(api_session_upload_file)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/api/sessions/{id}/stop-record",
            post(api_session_stop_record),
        )
        .route(
            "/api/sessions/{id}/idle-timeout",
            post(api_session_idle_timeout),
        )
        .route(
            "/api/sessions/{id}/pause-record",
            post(api_session_pause_record),
        )
        .route(
            "/api/sessions/{id}/resume-record",
            post(api_session_resume_record),
        )
        .route(
            "/api/profiles",
            get(api_list_profiles).post(api_create_profile),
        )
        .route(
            "/api/profiles/{name}",
            get(api_get_profile)
                .patch(api_update_profile)
                .put(api_update_profile)
                .delete(api_delete_profile),
        )
        .route("/api/profiles/{name}/clone", post(api_clone_profile))
        .route("/api/profiles/{name}/export", get(api_export_profile))
        .route(
            "/api/profiles/{name}/import",
            post(api_import_profile).layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/api/profiles/{name}/clear", post(api_clear_profile))
        .route(
            "/api/profiles/{name}/backups",
            get(api_list_backups).post(api_create_backup),
        )
        .route(
            "/api/profiles/{name}/backups/{backup}",
            delete(api_delete_backup),
        )
        .route(
            "/api/profiles/{name}/backups/{backup}/restore",
            post(api_restore_backup),
        )
        .route("/api/profiles/{name}/transfer", post(api_transfer_profile))
        // Recording APIs
        .route("/api/recordings", get(api_list_recordings))
        .route(
            "/api/recordings/{id}",
            get(api_get_recording).delete(api_delete_recording),
        )
        .route("/api/recordings/{id}/frames", get(api_get_recording_frames))
        .route("/api/recordings/{id}/audio", get(api_get_recording_audio))
        .route(
            "/api/recordings/{id}/export.webm",
            get(api_export_recording),
        )
        .route("/api/recordings/merge", post(api_merge_recordings))
        .route("/api/recordings/{id}/keep", post(api_set_recording_keep))
        .route("/api/favicon", get(api_site_favicon))
        .route(
            "/api/recordings/{id}/favicon",
            get(api_get_recording_favicon),
        )
        .route("/api/recordings/vacuum", post(api_vacuum_recordings))
        // Peer-to-Peer Mesh APIs
        .route("/api/mesh/node", get(api_mesh_node))
        .route("/api/mesh/overview", get(api_mesh_overview))
        .route("/api/ws/overview", get(handle_overview_ws))
        .route("/api/peers", get(api_list_peers).post(api_add_peer))
        .route("/api/peers/{peer}", delete(api_remove_peer))
        // Everything else for a peer is forwarded to it (see `peer_target`).
        .route("/api/peers/{peer}/{*rest}", any(api_peer_forward))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::layer,
        ));

    tokio::spawn(shutdown_on_signal(state));
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let active_sessions = state.sessions.read().await.len();
    let mut supported_engines = vec!["chromium"];
    if state.config.firefox_bin.is_some() {
        supported_engines.push("firefox");
    }
    Json(serde_json::json!({
        "status": "ok",
        "node": state.node_name,
        "active_sessions": active_sessions,
        "max_sessions": state.max_sessions,
        "peers_count": state.peers.read().await.len(),
        "supported_engines": supported_engines,
    }))
}

/// On SIGTERM/SIGINT (a restart or deploy) the browsers keep running for the next
/// daemon to adopt: save every session's record, hand Firefox's single WebDriver
/// session back (it would otherwise stay taken), and let the recordings finish
/// (the next daemon continues them in new ones, as the records say). Then exit:
/// open live-view sockets would stall a graceful server shutdown.
async fn shutdown_on_signal(state: AppState) {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    info!("Shutting down: sessions stay running");
    state.history.flush().await;
    let sessions: Vec<ManagedSession> = state.sessions.read().await.values().cloned().collect();
    // Records first: they say which sessions were recording.
    for session in &sessions {
        state.persist_session(session).await;
    }
    for session in &sessions {
        if let Some(ref hub) = session.bidi {
            hub.end().await;
        }
    }
    let ids: Vec<Uuid> = {
        let recs = state.active_recordings.read().await;
        for handle in recs.values() {
            handle.stop();
        }
        recs.values().map(|h| h.recording_id).collect()
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !ids.is_empty() {
        let pending = recorder::list_recordings(&state.recorder_config)
            .await
            .iter()
            .any(|m| m.status == "recording" && ids.contains(&m.id));
        if !pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    info!(sessions = sessions.len(), recordings = ids.len(), "Exiting");
    std::process::exit(0);
}

async fn handle_overview_ws(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }

    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        // Listen first, then send the current one: nothing falls in between.
        let mut updates = state.overview.latest.subscribe();
        let snapshot = state.overview_snapshot(Duration::from_secs(2)).await;
        updates.borrow_and_update();
        if socket
            .send(Message::Text(snapshot.json.clone().into()))
            .await
            .is_err()
        {
            return;
        }

        loop {
            tokio::select! {
                // The overview ticker publishes a new one every two seconds for as
                // long as somebody listens: one computation for all of them.
                changed = updates.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let Some(snapshot) = updates.borrow_and_update().clone() else {
                        continue;
                    };
                    if socket.send(Message::Text(snapshot.json.clone().into())).await.is_err() {
                        break;
                    }
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Text(txt))) => {
                            if txt.as_str() == "refresh" {
                                let snapshot = state.overview_snapshot(Duration::from_millis(500)).await;
                                updates.borrow_and_update();
                                let _ = socket.send(Message::Text(snapshot.json.clone().into())).await;
                            } else if txt.as_str() == "ping" {
                                let _ = socket.send(Message::Text("pong".into())).await;
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if socket.send(Message::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    })
}

/// The peer a request names: by its name or its URL, exactly (a name is
/// matched without regard to case). A name that fits several peers fits none.
fn find_peer<'a>(peers: &'a HashMap<String, String>, peer: &str) -> Option<&'a String> {
    let peer = peer.trim();
    if let Some(url) = peers.get(peer) {
        return Some(url);
    }
    let wanted_url = peer.trim_end_matches('/');
    let mut hits = peers.iter().filter(|(name, url)| {
        name.eq_ignore_ascii_case(peer) || url.trim_end_matches('/') == wanted_url
    });
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit.1)
}

async fn get_peer_url(state: &AppState, peer: &str) -> Option<String> {
    find_peer(&*state.peers.read().await, peer).cloned()
}

/// How long a peer may take to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerWait {
    /// Anything that answers from memory or does a little work.
    Short,
    /// Copies, archives, recordings: minutes to hours.
    Long,
}

impl PeerWait {
    fn timeout(self) -> Duration {
        match self {
            Self::Short => Duration::from_secs(120),
            Self::Long => Duration::from_secs(3600),
        }
    }
}

/// Where a request for a peer goes on that peer.
#[derive(Debug, PartialEq, Eq)]
struct PeerTarget {
    /// Path segments below `/api`, or the whole path of a DevTools route.
    segments: Vec<String>,
    devtools: bool,
    wait: PeerWait,
}

/// What `/api/peers/{peer}/{rest}` may reach on a peer: the management routes the
/// dashboard, CLI and TUI use on remote nodes, and the DevTools tab routes. Anything
/// else (the peer's own peers included, which would loop) is not forwarded.
fn peer_target(method: &axum::http::Method, rest: &str) -> Option<PeerTarget> {
    use PeerWait::{Long, Short};
    let segments: Vec<&str> = rest.split('/').collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains(['\\', '\0']))
    {
        return None;
    }
    let (devtools, wait) = match (method.as_str(), segments.as_slice()) {
        ("POST", ["sessions"]) => (false, Short),
        ("DELETE", ["sessions", _]) => (false, Short),
        ("GET", ["sessions", _, "screenshot"]) => (false, Short),
        ("POST", ["sessions", _, "save-as-profile"]) => (false, Long),
        (
            "POST",
            ["sessions", _, "record" | "stop-record" | "pause-record" | "resume-record" | "idle-timeout" | "name"
            | "proxy"],
        ) => (false, Short),
        ("PATCH", ["settings"]) => (false, Short),
        ("POST", ["profiles"]) => (false, Long),
        ("GET" | "PATCH" | "PUT", ["profiles", _]) => (false, Short),
        ("DELETE", ["profiles", _]) => (false, Long),
        ("POST", ["profiles", _, "clone" | "clear" | "import" | "transfer"]) => (false, Long),
        ("GET", ["profiles", _, "export"]) => (false, Long),
        ("GET" | "POST", ["profiles", _, "backups"]) => (false, Long),
        ("DELETE", ["profiles", _, "backups", _]) => (false, Short),
        ("POST", ["profiles", _, "backups", _, "restore"]) => (false, Long),
        ("GET", ["recordings"]) => (false, Short),
        ("POST", ["recordings", "vacuum" | "merge"]) => (false, Long),
        ("GET" | "DELETE", ["recordings", _]) => (false, Short),
        ("GET", ["recordings", _, "frames" | "audio" | "export.webm"]) => (false, Long),
        ("GET", ["recordings", _, "favicon"]) => (false, Short),
        ("POST", ["recordings", _, "keep"]) => (false, Short),
        ("PUT", ["json", "new"]) => (true, Short),
        ("PUT", ["json", "close" | "activate", _]) => (true, Short),
        _ => return None,
    };
    Some(PeerTarget {
        segments: segments.into_iter().map(str::to_string).collect(),
        devtools,
        wait,
    })
}

/// A request body's chunks, failing once more than `limit` bytes have come.
fn capped_chunks(
    body: axum::body::Body,
    limit: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    use futures_util::StreamExt;
    body.into_data_stream().scan(0usize, move |seen, chunk| {
        let item = match chunk {
            Ok(bytes) => {
                *seen = seen.saturating_add(bytes.len());
                if *seen > limit {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "The body is too large",
                    ))
                } else {
                    Ok(bytes)
                }
            }
            Err(e) => Err(std::io::Error::other(e)),
        };
        std::future::ready(Some(item))
    })
}

/// Forward a request to the peer it names, streaming the body both ways: an
/// import or an export never sits in memory, and the wait is as long as the
/// kind of request needs, not the overview's few seconds.
async fn api_peer_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: axum::http::Method,
    AxumPath((peer, rest)): AxumPath<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Body,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(target) = peer_target(&method, &rest) else {
        return api_error(StatusCode::NOT_FOUND, "Unknown route for a peer").into_response();
    };
    let Some(peer_url) = get_peer_url(&state, &peer).await else {
        return api_error(
            StatusCode::NOT_FOUND,
            format!("Peer '{peer}' not found in mesh"),
        )
        .into_response();
    };
    let Ok(mut url) = url::Url::parse(&peer_url) else {
        return api_error(StatusCode::BAD_GATEWAY, "The peer's URL is invalid").into_response();
    };
    {
        let Ok(mut path) = url.path_segments_mut() else {
            return api_error(StatusCode::BAD_GATEWAY, "The peer's URL is invalid").into_response();
        };
        path.pop_if_empty();
        if !target.devtools {
            path.push("api");
        }
        path.extend(&target.segments);
    }
    url.set_query(query.as_deref().filter(|q| !q.is_empty()));

    let upstream_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = state
        .peer_request(state.peer_client.request(upstream_method, url))
        .timeout(target.wait.timeout());
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range.clone());
    }
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if length.map_or(headers.contains_key(header::TRANSFER_ENCODING), |n| n > 0) {
        if let Some(ct) = headers.get(header::CONTENT_TYPE) {
            req = req.header(header::CONTENT_TYPE, ct.clone());
        }
        if let Some(n) = length {
            req = req.header(header::CONTENT_LENGTH, n);
        }
        req = req.body(reqwest::Body::wrap_stream(capped_chunks(
            body,
            MAX_UPLOAD_BYTES,
        )));
    }

    match req.send().await {
        Ok(res) => {
            let status =
                StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_DISPOSITION,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
                header::CACHE_CONTROL,
                header::ETAG,
                header::LAST_MODIFIED,
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
        Err(e) => api_error(
            StatusCode::BAD_GATEWAY,
            format!("Peer communication failed: {e}"),
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
            "revision": REVISION,
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
    // Every request to a peer carries the cluster secret.
    if !relay::peer_url_ok(&trimmed) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Peer URL must be http(s) and not a loopback address" })),
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
        relay::deny_url(&trimmed);
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

#[derive(Deserialize)]
struct UpdateSettingsRequest {
    /// Proxy id; "none" or "" for direct. Absent: unchanged.
    #[serde(default)]
    default_proxy: Option<String>,
    /// Engine defaults, each engine replaced as a whole. Absent: unchanged.
    #[serde(default)]
    engines: Option<EnginesPatch>,
}

/// The engines of an `UpdateSettingsRequest` that change; one left out stays.
#[derive(Deserialize)]
struct EnginesPatch {
    #[serde(default)]
    chromium: Option<settings::EngineDefaults>,
    #[serde(default)]
    firefox: Option<settings::EngineDefaults>,
}

async fn api_get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(state.settings.get()))
}

async fn api_update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let settings = state
        .settings
        .update(|settings| {
            if let Some(ref key) = req.default_proxy {
                settings.default_proxy = if key.trim().is_empty() {
                    None
                } else {
                    state.check_proxy(key)?
                };
            }
            if let Some(engines) = req.engines {
                if let Some(chromium) = engines.chromium {
                    settings.engines.chromium = chromium;
                }
                if let Some(firefox) = engines.firefox {
                    settings.engines.firefox = firefox;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            settings::UpdateError::Rejected(e) => e,
            settings::UpdateError::Io(e) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not save the settings: {e}"),
            ),
        })?;
    info!(settings = ?settings, "Node settings changed");
    Ok(Json(settings))
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

/// The site's own icon, as grabbed during the recording.
async fn api_get_recording_favicon(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let dir = state.recorder_config.recordings_dir.join(id.to_string());
    let meta = tokio::fs::read(dir.join("meta.json"))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<recorder::RecordingMeta>(&b).ok());
    let (Some(mime), Ok(bytes)) = (
        meta.and_then(|m| m.favicon_type),
        tokio::fs::read(dir.join("favicon.bin")).await,
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Deserialize)]
struct SiteFaviconParams {
    url: String,
}

/// The icon of the site a page belongs to, fetched by this node (see `favicon.rs`).
async fn api_site_favicon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SiteFaviconParams>,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    match favicon::lookup(&params.url).await {
        Some(icon) => (
            [
                (header::CONTENT_TYPE, icon.mime.to_string()),
                (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; sandbox".to_string(),
                ),
            ],
            icon.bytes,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "private, max-age=600")],
        )
            .into_response(),
    }
}

async fn api_get_recording_frames(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    if let Err(e) = state.check_auth(req.headers()) {
        return e.into_response();
    }
    let frames_path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join("frames.jsonl");
    if !fs::try_exists(&frames_path).await.unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Frames not found for recording" })),
        )
            .into_response();
    }
    // Streamed from disk with Range support: recordings reach hundreds of MB,
    // and clients resume interrupted downloads with `Range: bytes=N-`.
    match tower_http::services::ServeFile::new(&frames_path)
        .try_call(req)
        .await
    {
        Ok(res) => {
            let mut res = res.map(axum::body::Body::new);
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            res
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to read frames: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct KeepRequest {
    keep: bool,
}

/// Mark a recording to be kept (`{"keep": true}`), or not: the cleanup never removes
/// a kept one. Nothing is kept unless asked.
async fn api_set_recording_keep(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<KeepRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if recorder::set_keep(&state.recorder_config, id, req.keep).await {
        Ok(Json(serde_json::json!({ "id": id, "keep": req.keep })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))
    }
}

async fn api_delete_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if recorder::is_active(id) || recorder::is_busy(id) {
        return Err((
            StatusCode::CONFLICT,
            Json(
                serde_json::json!({ "error": "The recording is in use (being recorded, exported or merged)" }),
            ),
        ));
    }
    if recorder::delete_recording(&state.recorder_config, id).await {
        Ok(Json(serde_json::json!({ "status": "deleted", "id": id })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))
    }
}

/// A recording's sound (`audio.webm`, Opus), with Range support for seeking.
async fn api_get_recording_audio(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    if let Err(e) = state.check_auth(req.headers()) {
        return e.into_response();
    }
    let path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join(rec_audio::FILE);
    if !fs::try_exists(&path).await.unwrap_or(false) {
        return api_error(StatusCode::NOT_FOUND, "This recording has no sound").into_response();
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
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The recording as a WebM video with its sound, encoded on request (see
/// rec_export.rs); this takes a while for a long one.
async fn api_export_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let (file, size, name) = match rec_export::export_webm(&state.recorder_config, id).await {
        Ok(x) => x,
        Err(e) => return api_error(StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    };
    let chunks = futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "video/webm")
        .header(header::CONTENT_LENGTH, size)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .body(axum::body::Body::from_stream(chunks))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Deserialize, Serialize)]
struct MergeRequest {
    /// The recordings to merge, in order.
    ids: Vec<Uuid>,
    /// Delete them once the merge is written.
    #[serde(default)]
    delete: bool,
}

/// Merges recordings of this node into a new one (see recorder::merge_recordings).
async fn api_merge_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MergeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let meta = recorder::merge_recordings(&state.recorder_config, &req.ids, req.delete)
        .await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(meta))
}

async fn api_vacuum_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let stats = recorder::vacuum_recordings(&state.recorder_config).await;
    Ok(Json(stats))
}

#[derive(Deserialize)]
struct RecordParams {
    /// Take the session's sound too (default: the node's --record-audio).
    #[serde(default)]
    audio: Option<bool>,
}

async fn api_session_start_record(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<RecordParams>,
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

    let Some((rec_id, started)) = state.start_recording(&session, params.audio).await else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ));
    };
    Ok(Json(if started {
        serde_json::json!({
            "status": "recording_started",
            "session_id": id,
            "recording_id": rec_id
        })
    } else {
        serde_json::json!({ "status": "already_recording", "session_id": id })
    }))
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

#[derive(Deserialize)]
struct IdleTimeoutParams {
    minutes: u64,
}

/// Changes how long a session may sit unused (0 = never stop it) and
/// restarts its idle clock.
async fn api_session_idle_timeout(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<IdleTimeoutParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let guard = state.sessions.read().await;
    let session = guard.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Session not found" })),
    ))?;
    let secs = idle::minutes_to_secs(params.minutes);
    session.activity.set_timeout_secs(secs);
    Ok(Json(serde_json::json!({
        "session_id": id,
        "idle_timeout_seconds": secs
    })))
}

async fn api_list_proxies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(state.proxy_infos()))
}

#[derive(Deserialize)]
struct VideoParams {
    session_id: Uuid,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Stream a region of a session's own display as H.264, one access unit per
/// binary message: a flag byte (1 = keyframe) followed by the Annex B data. One
/// encoder per viewer; reconnect to change the region. Access control as for the
/// page WebSocket (unguessable session id, mesh or Authelia in front).
async fn handle_video_ws(
    ws: WebSocketUpgrade,
    Query(p): Query<VideoParams>,
    State(state): State<AppState>,
) -> Response {
    let Some(cfg) = state.config.video.clone() else {
        return (StatusCode::NOT_FOUND, "This node does not stream video").into_response();
    };
    let found = state
        .sessions
        .read()
        .await
        .get(&p.session_id)
        .and_then(|s| Some((s.display.clone()?, s.display_size?)));
    let Some((xdisplay, (dw, dh))) = found else {
        return (
            StatusCode::NOT_FOUND,
            "No display of its own for this session",
        )
            .into_response();
    };
    let region = video::Region {
        x: p.x,
        y: p.y,
        w: p.w,
        h: p.h,
    };
    let Some(region) = region.clamp(dw, dh) else {
        return (StatusCode::BAD_REQUEST, "Region outside the display").into_response();
    };
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let mut child = match video::encoder(&cfg, &xdisplay, region) {
            Ok(c) => c,
            Err(e) => {
                warn!(xdisplay = %xdisplay, error = %e, "Could not start the video encoder");
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        if let Some(err) = child.stderr.take() {
            let xdisplay = xdisplay.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(xdisplay = %xdisplay, "ffmpeg: {line}");
                }
            });
        }
        info!(session_id = %p.session_id, region = ?region, "Video viewer attached");
        let mut units = video::AccessUnits::default();
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending = false;
        'stream: loop {
            tokio::select! {
                // Encoder quiet for a moment: the buffered frame is complete. ffmpeg
                // writes a frame in back-to-back chunks, so a few ms is plenty.
                _ = tokio::time::sleep(std::time::Duration::from_millis(4)), if pending => {
                    pending = false;
                    if let Some((key, au)) = units.flush() {
                        let mut msg = Vec::with_capacity(au.len() + 1);
                        msg.push(u8::from(key));
                        msg.extend_from_slice(&au);
                        if socket.send(Message::Binary(msg.into())).await.is_err() {
                            break 'stream;
                        }
                    }
                }
                n = out.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending = true;
                        for (key, au) in units.push(&buf[..n]) {
                            let mut msg = Vec::with_capacity(au.len() + 1);
                            msg.push(u8::from(key));
                            msg.extend_from_slice(&au);
                            if socket.send(Message::Binary(msg.into())).await.is_err() {
                                break 'stream;
                            }
                        }
                    }
                },
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        let _ = child.kill().await;
        info!(session_id = %p.session_id, "Video viewer left");
    })
}

#[derive(Deserialize)]
struct AudioParams {
    session_id: Uuid,
}

/// Stream a session's audio as Opus in WebM, one encoder per listener. Like the
/// page WebSocket, the unguessable session id plus the mesh or Authelia in
/// front are the access control.
async fn handle_audio_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<AudioParams>,
    State(state): State<AppState>,
) -> Response {
    let Some(cfg) = state.config.audio.clone() else {
        return (StatusCode::NOT_FOUND, "This node has no audio").into_response();
    };
    let sink = state
        .sessions
        .read()
        .await
        .get(&params.session_id)
        .and_then(|s| s.audio_sink.clone());
    let Some(sink) = sink else {
        return (StatusCode::NOT_FOUND, "No audio for this session").into_response();
    };
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::io::AsyncReadExt;
        let mut child = match audio::encoder(&cfg, &sink) {
            Ok(c) => c,
            Err(e) => {
                warn!(sink = %sink, error = %e, "Could not start the audio encoder");
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        info!(session_id = %params.session_id, "Audio listener attached");
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            tokio::select! {
                n = out.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if socket.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                },
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        let _ = child.kill().await;
        info!(session_id = %params.session_id, "Audio listener left");
    })
}

#[derive(Deserialize)]
struct ViewerReport {
    /// [width, height] while in Native/Phone layout, null otherwise.
    native: Option<(u32, u32)>,
    /// This viewer's size wins over the others' (and over automation clients).
    #[serde(default)]
    force: bool,
}

/// One live view of a session: it reports its size in Native layout and gets
/// the size all Native viewers of the session share.
async fn handle_viewers_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<AudioParams>,
    State(state): State<AppState>,
) -> Response {
    if !state.sessions.read().await.contains_key(&params.session_id) {
        return (StatusCode::NOT_FOUND, "No such session").into_response();
    }
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let session = params.session_id;
        let (id, mut rx) = state.viewers.join(session);
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(r) = serde_json::from_str::<ViewerReport>(&t) {
                            state.viewers.set(session, id, r.native, r.force);
                        }
                    }
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        state.viewers.leave(session, id);
    })
}

#[derive(Deserialize)]
struct AgentTopic {
    conversation: String,
    topic: String,
}

/// An agent labels its browser connections with what it is doing (its
/// `browser-agent-topic` command). No cluster secret: only a conversation that is
/// connected can be labelled, and the label is all that changes.
async fn api_agent_topic(
    State(state): State<AppState>,
    Json(req): Json<AgentTopic>,
) -> Result<impl IntoResponse, ApiError> {
    let topic = clients::clean(Some(&req.topic));
    let conversation = req.conversation.trim();
    if conversation.len() < 8 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Missing conversation id",
        ));
    }
    let sessions: Vec<ManagedSession> = state.sessions.read().await.values().cloned().collect();
    let labelled: usize = sessions
        .iter()
        .map(|s| s.clients.set_topic(conversation, topic.as_deref()))
        .sum();
    Ok(Json(serde_json::json!({ "clients": labelled })))
}

#[derive(Deserialize)]
struct RenameRequest {
    /// Empty: back to the generated name.
    name: String,
}

async fn api_session_rename(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RenameRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    let name = names::clean(&req.name).unwrap_or_else(|| names::generate(id));
    *session.name.write().unwrap() = name.clone();
    Ok(Json(serde_json::json!({ "name": name })))
}

/// The session's display and its size, with the xdotool to drive it.
async fn session_x(state: &AppState, id: Uuid) -> Result<(PathBuf, String, (u32, u32)), ApiError> {
    let xdotool = state
        .config
        .session_displays
        .as_ref()
        .and_then(|d| d.xdotool.clone())
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "This node has no xdotool"))?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    match (session.display.clone(), session.display_size) {
        (Some(display), Some(size)) => Ok((xdotool, display, size)),
        _ => Err(api_error(
            StatusCode::NOT_FOUND,
            "No display of its own for this session",
        )),
    }
}

/// Native popups open on the session's display (context menu, `<select>` list).
async fn api_session_popups(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let (xdotool, display, size) = session_x(&state, id).await?;
    let popups = display::popups(&xdotool, &display, size).await;
    Ok(Json(serde_json::json!({ "popups": popups })))
}

/// Mouse or key input for the session's display, for a view while a popup is open.
async fn api_session_x_input(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<display::XInput>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let (xdotool, display, size) = session_x(&state, id).await?;
    if !display::send_input(&xdotool, &display, size, &input).await {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input refused"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct InsertTextRequest {
    text: String,
    /// The sites the text may go to (the vault entry's URIs): the page, as the
    /// browser's own page list has it, and the focused frame must be on one,
    /// so a navigation in between cannot send the text elsewhere.
    #[serde(default)]
    uris: Vec<secrets::Uri>,
    /// Press Enter afterwards.
    #[serde(default)]
    enter: bool,
    /// The session the page must belong to (the caller's own).
    #[serde(default)]
    session_id: Option<Uuid>,
}

/// Types text into the focused element of a Chromium page (see page_input.rs).
/// The text is never logged.
async fn api_page_insert_text(
    AxumPath(page_id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<InsertTextRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page = resolve_page(&targets, &page_id, req.session_id)
        .map_err(|e| api_error(StatusCode::NOT_FOUND, e))?;
    let session = match page.session_id {
        Some(sid) => state.sessions.read().await.get(&sid).cloned(),
        None => None,
    }
    .filter(|s| s.bidi.is_none())
    .ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "Page not found (Chromium sessions only)",
        )
    })?;
    let page_url = page.url.clone();
    if !secrets::url_matches(&page_url, &req.uris) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "The page is not on a site the entry is saved for",
        ));
    }
    session.activity.touch();
    let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, page_id);
    let uris = req.uris;
    page_input::insert_text(&ws_url, &req.text, req.enter, move |u| {
        secrets::url_matches(u, &uris)
    })
    .await
    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PasskeyRequest {
    page_id: String,
    rp_id: String,
    /// true: only look up which vault entry would be used (or that it's locked).
    #[serde(default)]
    find: bool,
    /// Vault entry id, when several entries hold a passkey for the site.
    #[serde(default)]
    entry: Option<String>,
    /// Sign the request's challenge (base64url) instead of injecting the passkey
    /// (Firefox: the page answers navigator.credentials.get() with the result).
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
}

/// A passkey approved in the live view: the helper puts it into the page.
/// Answers right away with the request's id; its progress is at /api/passkey/{id}.
async fn api_session_passkey(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PasskeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    let kind = match (req.find, &req.challenge) {
        (true, _) => "find",
        (false, Some(_)) => "assert",
        (false, None) => "inject",
    };
    // The page comes from the browser's own page list, never from what the caller
    // says, and must be one of this session's. The origin to sign for and the rp_id
    // follow from it: a page must not obtain an assertion for another site.
    let targets = get_local_targets(&state).await;
    let page = targets
        .iter()
        .find(|t| t.id == req.page_id && t.session_id == Some(id))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found in this session"))?;
    let page_url = url::Url::parse(&page.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page has no web origin"))?;
    let rp_id = req.rp_id.trim().trim_end_matches('.').to_ascii_lowercase();
    if !secrets::rp_id_valid_for(page_url.host_str().unwrap_or_default(), &rp_id) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The page is not on {rp_id} or a subdomain of it"),
        ));
    }
    let origin = (kind == "assert").then(|| page_url.origin().ascii_serialization());
    if !req.find {
        info!(session_id = %id, rp_id = %rp_id, kind, "Passkey approved in the live view");
    }
    let signing = passkeys::Signing {
        origin,
        challenge: req.challenge,
        allow: req.allow,
    };
    let request = state
        .passkeys
        .start(req.page_id, rp_id, kind, req.entry, signing)
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(serde_json::json!({ "request": request })))
}

#[derive(Deserialize)]
struct AutofillRequest {
    page_id: String,
    /// Vault entry id to fill; without one, the entries for the page's site are listed.
    #[serde(default)]
    entry: Option<String>,
    /// Fields of the entry to fill (default: username and password).
    #[serde(default)]
    fields: Vec<String>,
    /// Listing: unlock the vault (from the phone) if it's locked.
    #[serde(default)]
    unlock: bool,
    /// Listing: search the whole vault for this instead.
    #[serde(default)]
    query: Option<String>,
    /// Filling: the user confirmed an entry that isn't saved for the site.
    #[serde(default)]
    any_site: bool,
}

/// The vault button of the live view: list the logins for the page's site, or
/// fill one. The site is the page's host as the browser has it, never what the
/// view says. Answers with the request's id, polled at /api/passkey/{id}.
async fn api_session_autofill(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AutofillRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page = targets
        .iter()
        .find(|t| t.id == req.page_id && t.session_id == Some(id))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found in this session"))?;
    let host = url::Url::parse(&page.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
    if let Some(session) = state.sessions.read().await.get(&id) {
        session.activity.touch();
    }
    if req.entry.is_some() {
        info!(
            session_id = %id,
            host = %host,
            any_site = req.any_site,
            "Login fill approved in the live view"
        );
    }
    let query = req
        .query
        .map(|q| q.trim().chars().take(100).collect::<String>())
        .filter(|q| !q.is_empty());
    let fields = if req.fields.is_empty() {
        vec!["username".into(), "password".into()]
    } else {
        req.fields
    };
    let request = state
        .passkeys
        .start_autofill(
            req.page_id,
            host.clone(),
            passkeys::Autofill {
                session: Some(id),
                entry: req.entry,
                fields,
                unlock: req.unlock,
                query,
                any_site: req.any_site,
            },
        )
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(
        serde_json::json!({ "request": request, "host": host }),
    ))
}

#[derive(Deserialize)]
struct PageAutofill {
    /// The "fill" request this answers: only one the helper took, for this page and
    /// host, and only once.
    request: Uuid,
    /// The page must still be on this host (the one the entry was checked for).
    host: String,
    /// The entry's URIs: only frames on these sites are filled.
    uris: Vec<secrets::Uri>,
    /// The entry is not saved for the site and was picked anyway (the helper
    /// told the user): only the page's own site is filled, then.
    #[serde(default)]
    any_site: bool,
    #[serde(flatten)]
    creds: autofill::Creds,
}

/// The passkey helper fills a vault entry into a page (see autofill.rs). The
/// values are never logged.
async fn api_page_autofill(
    AxumPath(page_id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PageAutofill>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page =
        resolve_page(&targets, &page_id, None).map_err(|e| api_error(StatusCode::NOT_FOUND, e))?;
    let actual = url::Url::parse(&page.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    if actual.as_deref() != Some(req.host.to_ascii_lowercase().as_str()) {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("The page is not on {} anymore", req.host),
        ));
    }
    let uris = if req.any_site {
        let origin = url::Url::parse(&page.url)
            .ok()
            .map(|u| u.origin().ascii_serialization())
            .filter(|o| o.starts_with("http"))
            .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
        vec![secrets::Uri {
            uri: origin,
            match_type: Some(1),
        }]
    } else {
        req.uris
    };
    if !secrets::url_matches(&page.url, &uris) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The entry is not for {}", req.host),
        ));
    }
    if !state
        .passkeys
        .authorize_fill(req.request, &page_id, &req.host, page.session_id)
    {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "No approved fill request for this page",
        ));
    }
    let sid = page
        .session_id
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found"))?;
    let session = state
        .sessions
        .read()
        .await
        .get(&sid)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    let frame_ok = move |u: &str| secrets::url_matches(u, &uris);
    let filled = match session.bidi {
        Some(ref hub) => autofill::fill_firefox(hub, &page_id, frame_ok, &req.creds).await,
        None => {
            let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, page_id);
            autofill::fill_chromium(&ws_url, frame_ok, &req.creds).await
        }
    }
    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
    if filled.is_empty() {
        return Err(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "No login form found on this page",
        ));
    }
    Ok(Json(serde_json::json!({ "filled": filled })))
}

async fn api_passkey_status(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    state
        .passkeys
        .status(req)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such passkey request"))
}

#[derive(Deserialize)]
struct PasskeyNote {
    note: String,
}

async fn api_passkey_helper_note(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(n): Json<PasskeyNote>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(if state.passkeys.note(req, n.note) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::GONE
    })
}

#[derive(Deserialize)]
struct HelperPoll {
    /// The helper handles "find" requests (newer helpers say so).
    #[serde(default)]
    finds: bool,
    /// The helper handles "logins" and "fill" requests.
    #[serde(default)]
    autofill: bool,
}

async fn api_passkey_helper_next(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(poll): Query<HelperPoll>,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    Ok(
        match state
            .passkeys
            .next(Duration::from_secs(25), poll.finds, poll.autofill)
            .await
        {
            Some(req) => Json(req).into_response(),
            None => StatusCode::NO_CONTENT.into_response(),
        },
    )
}

async fn api_passkey_helper_done(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(outcome): Json<passkeys::Outcome>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(if state.passkeys.complete(req, outcome) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::GONE
    })
}

#[derive(Deserialize)]
struct SetProxyRequest {
    /// Proxy id, or "none" for direct connections.
    proxy: String,
}

/// Switch a running session's proxy. Open connections are dropped, so pages
/// reconnect through the new route; nothing is restarted.
async fn api_session_set_proxy(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetProxyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let proxy = state
        .check_proxy(&req.proxy)?
        .and_then(|id| state.config.proxies.iter().find(|p| p.id == id).cloned());
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    session.relay.set(proxy);
    let rec = state.active_recordings.read().await.get(&id).cloned();
    Ok(Json(
        session.to_summary(&state.http_client, rec.as_ref()).await,
    ))
}

/// Largest file handoff can drop into a page.
const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

/// Files dropped into a session's pages, removed when the session stops.
fn upload_dir(session: &Uuid) -> PathBuf {
    std::env::temp_dir()
        .join("browser-daemon-uploads")
        .join(session.to_string())
}

/// A file name that is safe as a single path component, keeping it readable.
fn upload_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_string();
    if clean.is_empty() {
        "upload".to_string()
    } else {
        clean.chars().take(200).collect()
    }
}

#[derive(Deserialize)]
struct UploadParams {
    name: String,
}

/// Store a file on this node so a page of the session can receive it (handoff drops it
/// with CDP drag events, which take local paths). Returns the file's path.
async fn api_session_upload_file(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<UploadParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<impl IntoResponse, ApiError> {
    use futures_util::StreamExt;
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    // One directory per upload: the same name can be dropped twice.
    let dir = upload_dir(&id).join(Uuid::new_v4().to_string());
    let path = dir.join(upload_file_name(&params.name));
    let io = |e: std::io::Error| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    fs::create_dir_all(&dir).await.map_err(io)?;
    // Dropped files can be private: keep them away from other users of the host.
    let root = upload_dir(&id);
    let root = root.parent().unwrap_or(&root);
    fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(io)?;
    // Written as it arrives, not held in memory.
    let mut file = fs::File::create(&path).await.map_err(io)?;
    let mut chunks = Box::pin(capped_chunks(body, MAX_UPLOAD_BYTES));
    let mut size = 0u64;
    let stored: Result<(), ApiError> = async {
        while let Some(chunk) = chunks.next().await {
            let chunk =
                chunk.map_err(|e| api_error(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()))?;
            size += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(io)?;
        }
        file.flush().await.map_err(io)
    }
    .await;
    if let Err(e) = stored {
        let _ = fs::remove_dir_all(&dir).await;
        return Err(e);
    }
    info!(session_id = %id, path = %path.display(), bytes = size, "Stored dropped file");
    Ok(Json(serde_json::json!({
        "path": path,
        "size_bytes": size,
    })))
}

async fn api_session_pause_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    set_recording_paused(&state, &headers, id, true).await
}

async fn api_session_resume_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    set_recording_paused(&state, &headers, id, false).await
}

async fn set_recording_paused(
    state: &AppState,
    headers: &HeaderMap,
    id: Uuid,
    paused: bool,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(headers)?;
    let recs = state.active_recordings.read().await;
    let handle = recs.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Session is not actively recording" })),
    ))?;
    handle.clock.set_paused(paused);
    Ok(Json(serde_json::json!({
        "status": if paused { "recording_paused" } else { "recording_resumed" },
        "session_id": id,
        "recording_id": handle.recording_id
    })))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_names_stay_in_their_directory() {
        assert_eq!(upload_file_name("report.pdf"), "report.pdf");
        assert_eq!(upload_file_name("../../etc/passwd"), "passwd");
        assert_eq!(upload_file_name("C:\\Users\\me\\a b.txt"), "a b.txt");
        assert_eq!(upload_file_name(".."), "upload");
        assert_eq!(upload_file_name(".bashrc"), "bashrc");
        assert_eq!(upload_file_name("a\nb"), "ab");
    }

    #[test]
    fn peers_are_matched_exactly() {
        let peers: HashMap<String, String> = [
            ("rofl-13", "https://browser.rofl-13.example"),
            ("rofl-14", "https://browser.rofl-14.example/"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let url = |p: &str| find_peer(&peers, p).map(String::as_str);
        assert_eq!(url("rofl-13"), Some("https://browser.rofl-13.example"));
        assert_eq!(url("ROFL-14"), Some("https://browser.rofl-14.example/"));
        assert_eq!(
            url("https://browser.rofl-14.example"),
            Some("https://browser.rofl-14.example/")
        );
        // Not a part of a name or an address, which used to pick one at random.
        assert_eq!(url("rofl-1"), None);
        assert_eq!(url("rofl"), None);
        assert_eq!(url("browser.rofl-13"), None);
        assert_eq!(url(""), None);
    }

    #[test]
    fn only_known_routes_reach_a_peer() {
        use axum::http::Method;
        let target = |m: Method, p: &str| peer_target(&m, p);
        let t = target(Method::GET, "profiles/Work").unwrap();
        assert_eq!(t.segments, ["profiles", "Work"]);
        assert!(!t.devtools);
        assert_eq!(t.wait, PeerWait::Short);
        assert_eq!(
            target(Method::POST, "profiles/Work/import").unwrap().wait,
            PeerWait::Long
        );
        assert_eq!(
            target(Method::GET, "recordings/abc/export.webm")
                .unwrap()
                .wait,
            PeerWait::Long
        );
        assert!(target(Method::PUT, "json/new").unwrap().devtools);
        assert!(target(Method::PUT, "json/close/ABC").unwrap().devtools);
        // The method matters.
        assert!(target(Method::GET, "json/new").is_none());
        assert!(target(Method::DELETE, "settings").is_none());
        // Nothing else of the peer is reachable, nor a way out of the route.
        assert!(target(Method::POST, "peers/rofl-14/sessions").is_none());
        assert!(target(Method::GET, "mesh/overview").is_none());
        assert!(target(Method::GET, "profiles/../settings").is_none());
        assert!(target(Method::GET, "profiles/./x").is_none());
        assert!(target(Method::GET, "profiles//x").is_none());
        assert!(target(Method::GET, "profiles/").is_none());
        assert!(target(Method::GET, "profiles/a\\b").is_none());
        assert!(target(Method::POST, "sessions/abc/passkey").is_none());
    }

    #[tokio::test]
    async fn forwarded_bodies_are_capped() {
        use futures_util::StreamExt;
        let body = |chunks: Vec<&'static [u8]>| {
            axum::body::Body::from_stream(futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|c| Ok::<_, std::io::Error>(Bytes::from_static(c))),
            ))
        };
        let ok: Vec<_> = capped_chunks(body(vec![b"abc", b"de"]), 5).collect().await;
        assert!(ok.iter().all(Result::is_ok));
        let over: Vec<_> = capped_chunks(body(vec![b"abc", b"def"]), 5).collect().await;
        assert!(over[0].is_ok());
        assert!(over[1].is_err());
    }
}
