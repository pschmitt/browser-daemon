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
mod live_ws;
mod mcp;
mod mesh;
mod names;
mod overview;
mod page_input;
mod pages;
mod passkey_helper;
mod passkeys;
mod passkeys_api;
mod peerproc;
mod peers;
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
mod recordings_api;
mod relay;
mod secrets;
mod session_actions;
mod sessions;
mod settings;
mod settings_api;
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
use crate::live_ws::*;
use crate::mesh::*;
use crate::overview::*;
use crate::pages::*;
use crate::passkeys_api::*;
use crate::peers::*;
use crate::profile_transfer::*;
use crate::profiles::{ColorScheme, Profile};
use crate::profiles_api::*;
use crate::proxy::bridge_websockets;
use crate::recorder::{RecorderConfig, RecordingMeta, SessionRecorderHandle};
use crate::recordings_api::*;
use crate::session_actions::*;
use crate::sessions::*;
use crate::settings_api::*;
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

fn default_true() -> bool {
    true
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
