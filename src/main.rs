mod audible;
mod audio;
mod backups;
mod bidi;
mod browser;
mod clients;
mod display;
mod idle;
mod names;
mod passkey_helper;
mod passkeys;
mod profiles;
mod proxies;
mod proxy;
mod recorder;
mod relay;
mod settings;
mod tui;
mod video;
mod viewers;

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
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

use crate::bidi::BidiHub;
use crate::browser::{ActiveBrowser, BrowserConfig, BrowserEngine};
use crate::profiles::{ColorScheme, Profile};
use crate::proxy::bridge_websockets;
use crate::recorder::{RecorderConfig, RecordingMeta, SessionRecorderHandle};

static HANDOFF_HTML: &str = include_str!("../handoff.html");
static DASHBOARD_HTML: &str = include_str!("../dashboard.html");
static FAVICON_ICO: &[u8] = include_bytes!("../favicon.ico");
static FAVICON_SVG: &str = include_str!("../favicon.svg");
static HANDOFF_ICO: &[u8] = include_bytes!("../handoff.ico");
static HANDOFF_SVG: &str = include_str!("../handoff.svg");
static MANIFEST_JSON: &str = include_str!("../manifest.webmanifest");
static SW_JS: &str = include_str!("../sw.js");
static ICON_192: &[u8] = include_bytes!("../icon-192.png");
static ICON_512: &[u8] = include_bytes!("../icon-512.png");
static DOWNLOAD_WORKER_JS: &str = include_str!("../download-worker.js");

/// Git commit this build comes from, passed in by the Nix build.
pub const REVISION: Option<&str> = option_env!("BROWSER_DAEMON_REV");

#[derive(Parser, Debug)]
#[command(author, version, about = "Native NixOS remote browser daemon")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

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
    /// Terminal UI for the mesh: sessions, profiles, recordings and nodes.
    Tui(tui::TuiArgs),
    /// Serve passkeys approved in live views from the local rbw vault.
    PasskeyHelper(passkey_helper::HelperArgs),
}

#[derive(Clone)]
pub struct ManagedSession {
    pub id: Uuid,
    /// Given at creation or generated ("quiet-otter"); can be changed later.
    pub name: Arc<std::sync::RwLock<String>>,
    /// The automation clients connected right now (not live views).
    pub clients: Arc<clients::Clients>,
    pub engine: BrowserEngine,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    pub profile_id: Option<Uuid>,
    pub persistent: bool,
    pub created_at: Instant,
    pub client_count: Arc<AtomicUsize>,
    pub activity: Arc<idle::Activity>,
    pub adblock: bool,
    pub relay: Arc<relay::Relay>,
    /// PulseAudio sink the browser plays into.
    pub audio_sink: Option<String>,
    /// The X display the browser runs on: its own (per-session mode) or the shared one.
    pub display: Option<String>,
    /// Size of the session's own display (per-session mode only).
    pub display_size: Option<(u32, u32)>,
    /// The browser window's size at launch, which it returns to when no live
    /// view sizes it.
    pub window_size: (u32, u32),
    pub video: bool,
    pub color_scheme: ColorScheme,
    /// How live views size the page when they open (each view may switch).
    pub screen: Arc<std::sync::RwLock<ScreenMode>>,
    /// Whether live views mark taps and clicks when they open; None: the engine's default.
    pub show_touches: Arc<std::sync::RwLock<Option<bool>>>,
    /// Sound is playing (kept current by the audible monitor).
    pub audible: Arc<AtomicBool>,
    pub browser: Arc<Mutex<Option<ActiveBrowser>>>,
    pub bidi: Option<Arc<BidiHub>>,
}

impl ManagedSession {
    pub async fn to_summary(
        &self,
        http_client: &reqwest::Client,
        recording: Option<&SessionRecorderHandle>,
    ) -> SessionSummary {
        let tabs_count = if let Some(ref hub) = self.bidi {
            hub.call(
                "browsingContext.getTree",
                serde_json::json!({ "maxDepth": 0 }),
            )
            .await
            .ok()
            .and_then(|t| t["contexts"].as_array().map(Vec::len))
            .unwrap_or(0)
        } else {
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
            name: self.name.read().unwrap().clone(),
            clients: self.clients.list(),
            window_size: Some(self.window_size),
            engine: self.engine,
            port: self.port,
            ws_path: self.ws_path.clone(),
            profile: self.profile.clone(),
            profile_id: self.profile_id,
            persistent: self.persistent,
            uptime_seconds: self.created_at.elapsed().as_secs(),
            client_count: self.client_count.load(Ordering::SeqCst),
            tabs_count,
            adblock: self.adblock,
            proxy: self.relay.current(),
            audio: self.audio_sink.is_some(),
            display: self.display.clone(),
            display_server: self.display.as_ref().map(|_| "xvfb".to_string()),
            display_shared: self.display.is_some() && self.display_size.is_none(),
            video: self.video,
            color_scheme: self.color_scheme,
            screen: *self.screen.read().unwrap(),
            show_touches: *self.show_touches.read().unwrap(),
            audible: self.audible.load(Ordering::Relaxed),
            idle_seconds: self.activity.idle_secs(),
            idle_timeout_seconds: self.activity.timeout_secs(),
            is_recording: recording.is_some(),
            recording_paused: recording.is_some_and(SessionRecorderHandle::is_paused),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    #[serde(default)]
    pub name: String,
    /// Connected automation clients (agents), see clients.rs.
    #[serde(default)]
    pub clients: Vec<clients::ClientInfo>,
    /// The browser window's size at launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_size: Option<(u32, u32)>,
    /// How live views size the page when they open.
    #[serde(default)]
    pub screen: ScreenMode,
    /// Live views mark taps and clicks when they open (absent: the engine's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_touches: Option<bool>,
    #[serde(default)]
    pub engine: BrowserEngine,
    pub port: u16,
    pub ws_path: String,
    pub profile: Option<String>,
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    pub persistent: bool,
    pub uptime_seconds: u64,
    pub client_count: usize,
    pub tabs_count: usize,
    #[serde(default)]
    pub adblock: bool,
    /// Id of the proxy the session's traffic goes through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Its audio can be listened to on /audio.
    #[serde(default)]
    pub audio: bool,
    /// The display the browser runs on (`:100`); absent for headless browsers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Which display server provides it ("xvfb").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_server: Option<String>,
    /// The display is shared with other sessions (not one of its own).
    #[serde(default)]
    pub display_shared: bool,
    /// Its live view is available as H.264 video on /video.
    #[serde(default)]
    pub video: bool,
    /// Appearance the browser was launched with.
    #[serde(default)]
    pub color_scheme: ColorScheme,
    /// Sound is playing.
    #[serde(default)]
    pub audible: bool,
    #[serde(default)]
    pub idle_seconds: u64,
    /// 0: never stopped for inactivity.
    #[serde(default)]
    pub idle_timeout_seconds: u64,
    pub is_recording: bool,
    #[serde(default)]
    pub recording_paused: bool,
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
    pub idle_timeout_secs: u64,
    pub adblock_default: bool,
    /// Serializes profile creation and renames so names stay unique.
    pub profiles_lock: Arc<Mutex<()>>,
    /// First-seen order of tabs: Chromium lists them most recently used first,
    /// which reshuffles the tab bar whenever another tab is brought to the front.
    pub tab_order: Arc<std::sync::Mutex<TabOrder>>,
    /// Ids of the tabs playing sound.
    pub audible_pages: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Live views per session, for the size Native viewers share.
    pub viewers: Arc<viewers::Viewers>,
    /// Passkeys approved in live views, on their way to the passkey helper.
    pub passkeys: Arc<passkeys::Broker>,
    /// This node's settings (Nodes tab of the dashboard).
    pub settings: Arc<settings::Store>,
}

#[derive(Default)]
pub struct TabOrder {
    next: u64,
    seen: HashMap<String, u64>,
}

impl TabOrder {
    fn rank(&mut self, id: &str) -> u64 {
        if let Some(n) = self.seen.get(id) {
            return *n;
        }
        self.next += 1;
        self.seen.insert(id.to_string(), self.next);
        self.next
    }

    /// Sort tabs into a stable order and forget tabs that are gone.
    fn sort(&mut self, targets: &mut [DevToolsTarget]) {
        targets.sort_by_cached_key(|t| self.rank(&t.id));
        let live: std::collections::HashSet<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        self.seen.retain(|id, _| live.contains(id.as_str()));
    }
}

impl AppState {
    pub fn proxy_infos(&self) -> Vec<proxies::ProxyInfo> {
        self.config
            .proxies
            .iter()
            .map(proxies::Proxy::info)
            .collect()
    }

    /// A proxy id this node knows, `None` for a direct connection.
    pub fn check_proxy(&self, key: &str) -> Result<Option<String>, ApiError> {
        let key = key.trim();
        if proxies::DIRECT.contains(&key) {
            return Ok(None);
        }
        if self.config.proxies.iter().any(|p| p.id == key) {
            Ok(Some(key.to_string()))
        } else {
            Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("Unknown proxy '{key}' on this node"),
            ))
        }
    }

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
        self.check_auth_opt(headers, None)
    }

    pub fn check_auth_opt(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        if let Some(ref expected) = self.cluster_secret {
            let auth_header = headers
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok());
            let custom_header = headers
                .get("x-browser-cluster-secret")
                .and_then(|h| h.to_str().ok());

            let token = auth_header
                .and_then(|h| {
                    h.strip_prefix("Bearer ")
                        .or_else(|| h.strip_prefix("bearer "))
                })
                .or(custom_header)
                .or(query_token);

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
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_session(
        &self,
        engine: BrowserEngine,
        profile: Option<&str>,
        persistent: bool,
        width: Option<u32>,
        height: Option<u32>,
        record: Option<bool>,
        max_memory_mb: Option<u64>,
        idle_timeout_minutes: Option<u64>,
        adblock: Option<bool>,
        proxy: Option<&str>,
        color_scheme: Option<ColorScheme>,
        start_tab: bool,
    ) -> Result<ManagedSession, (StatusCode, &'static str)> {
        if engine == BrowserEngine::Firefox && self.config.firefox_bin.is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                "Firefox engine is not configured on this host",
            ));
        }

        let profile = match profile.map(str::trim).filter(|p| !p.is_empty()) {
            Some(key) => Some(self.resolve_or_create_profile(key, engine).await?),
            None => None,
        };

        // A profile runs in at most one browser: attach to its session if it has one.
        if let Some(ref profile) = profile {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions
                .values()
                .find(|s| s.profile_id == Some(profile.meta.id))
            {
                info!(session_id = %session.id, engine = %engine, profile = %profile.meta.name, "Attaching to existing profile session");
                session.activity.touch();
                return Ok(session.clone());
            }
        }

        let active_count = self.sessions.read().await.len();
        if active_count >= self.max_sessions {
            warn!(
                active = active_count,
                max = self.max_sessions,
                "Session limit reached"
            );
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Session limit reached. Try again later.",
            ));
        }

        // An explicit choice wins ("none" for a direct connection), then the profile's,
        // then the node's default (skipped when the node no longer has that proxy).
        let node_default = self
            .settings
            .get()
            .default_proxy
            .filter(|k| self.config.proxies.iter().any(|p| &p.id == k));
        let proxy_key = proxy
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| profile.as_ref().and_then(|p| p.meta.proxy.clone()))
            .or(node_default);
        let proxy = match proxy_key.as_deref() {
            None => None,
            Some(k) if proxies::DIRECT.contains(&k) => None,
            Some(k) => Some(
                self.config
                    .proxies
                    .iter()
                    .find(|p| p.id == k)
                    .ok_or((StatusCode::BAD_REQUEST, "Unknown proxy on this node"))?,
            ),
        };

        // An explicit choice wins, then the engine's default on this node.
        let engine_defaults = self.settings.get().engines.of(engine).clone();
        let adblock = adblock
            .or(engine_defaults.adblock)
            .unwrap_or(self.adblock_default);
        // An explicit choice wins, then the profile's, then the engine's; dark otherwise.
        let color_scheme = color_scheme
            .or_else(|| profile.as_ref().and_then(|p| p.meta.color_scheme))
            .or(engine_defaults.color_scheme)
            .unwrap_or_default();
        let browser = match ActiveBrowser::spawn(
            &self.config,
            engine,
            profile.as_ref(),
            width,
            height,
            max_memory_mb,
            adblock,
            proxy,
            color_scheme,
            start_tab,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, engine = %engine, "Failed to spawn browser process");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to spawn browser process",
                ));
            }
        };

        let session = ManagedSession {
            id: browser.id,
            name: Arc::new(std::sync::RwLock::new(names::generate(browser.id))),
            clients: Arc::default(),
            engine: browser.engine,
            port: browser.port,
            ws_path: browser.ws_path.clone(),
            profile: browser.profile_name.clone(),
            profile_id: browser.profile_id,
            persistent,
            created_at: browser.created_at,
            client_count: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(idle::Activity::new(
                idle_timeout_minutes.map_or(self.idle_timeout_secs, |m| m * 60),
            )),
            adblock: browser.adblock,
            relay: browser.relay.clone(),
            audio_sink: browser.audio.as_ref().map(|s| s.name.clone()),
            display: browser
                .display
                .as_ref()
                .map(|d| d.name.clone())
                .or_else(|| self.config.display.clone()),
            display_size: browser.display.as_ref().map(|d| (d.width, d.height)),
            window_size: browser.window_size,
            video: browser.display.is_some() && self.config.video.is_some(),
            color_scheme,
            screen: Arc::default(),
            show_touches: Arc::default(),
            audible: Arc::new(AtomicBool::new(false)),
            bidi: browser.bidi.clone(),
            browser: Arc::new(Mutex::new(Some(browser))),
        };

        {
            let mut guard = self.sessions.write().await;
            guard.insert(session.id, session.clone());
        }

        if record.unwrap_or(self.recorder_config.enabled) {
            self.start_recording(&session).await;
        }

        Ok(session)
    }

    /// A profile by id or name; an unknown name creates a profile for `engine`.
    async fn resolve_or_create_profile(
        &self,
        key: &str,
        engine: BrowserEngine,
    ) -> Result<Profile, (StatusCode, &'static str)> {
        let _guard = self.profiles_lock.lock().await;
        if let Some(profile) = profiles::find(&self.config.profiles_dir, key).await {
            if profile.meta.engine != engine {
                return Err((
                    StatusCode::CONFLICT,
                    "Profile belongs to a different browser engine",
                ));
            }
            return Ok(profile);
        }
        if Uuid::parse_str(key).is_ok() {
            return Err((StatusCode::NOT_FOUND, "Profile not found"));
        }
        let name =
            profiles::clean_name(key).ok_or((StatusCode::BAD_REQUEST, "Invalid profile name"))?;
        profiles::create(&self.config.profiles_dir, &name, engine)
            .await
            .map_err(|e| {
                error!(profile = %name, error = %e, "Failed to create profile");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create profile",
                )
            })
    }

    pub async fn start_recording(&self, session: &ManagedSession) -> Uuid {
        // Chromium with a display of its own: frames grabbed from the display, since
        // screenshots repaint the window the live video shows (flicker).
        let grab = match (&session.display, session.display_size, &self.config.video) {
            (Some(display), Some(size), Some(video))
                if session.engine == BrowserEngine::Chromium =>
            {
                Some(recorder::DisplayGrab {
                    ffmpeg: video.ffmpeg.clone(),
                    display: display.clone(),
                    size,
                })
            }
            _ => None,
        };
        let handle = recorder::start_recording(
            session.id,
            session.port,
            session.profile.clone(),
            self.node_name.clone(),
            (*self.recorder_config).clone(),
            grab,
        );
        // Firefox has no screencast: feed the recorder with BiDi screenshots instead.
        if let Some(ref hub) = session.bidi {
            spawn_bidi_frame_feeder(hub.clone(), handle.clone());
        }
        let rec_id = handle.recording_id;
        self.active_recordings
            .write()
            .await
            .insert(session.id, handle);
        rec_id
    }

    pub async fn terminate_session(&self, id: &Uuid) -> bool {
        if let Some(handle) = self.active_recordings.write().await.remove(id) {
            handle.stop();
        }

        let removed = {
            let mut guard = self.sessions.write().await;
            guard.remove(id)
        };

        let _ = fs::remove_dir_all(upload_dir(id)).await;
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
    pub engine: Option<BrowserEngine>,
    pub session_id: Option<Uuid>,
    pub profile: Option<String>,
    #[serde(default)]
    pub persistent: Option<bool>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub record: Option<bool>,
    pub max_memory_mb: Option<u64>,
    /// Minutes of inactivity before the session is stopped (0 = never).
    pub idle_timeout_minutes: Option<u64>,
    /// Load the ad blocker (defaults to the node's --adblock-default).
    pub adblock: Option<bool>,
    /// Proxy id, or "none"; defaults to the profile's proxy.
    pub proxy: Option<String>,
    /// "dark" or "light"; defaults to the profile's (dark).
    pub color_scheme: Option<ColorScheme>,
    /// Attach without counting as a client (handoff page).
    pub observer: Option<bool>,
    /// Who connects, as the agent's browser MCP reports it (shown on the dashboard):
    /// the agent ("claude-code 2.1.296"), its conversation id, its working directory
    /// and the machine it runs on.
    pub agent: Option<String>,
    pub conversation: Option<String>,
    pub cwd: Option<String>,
    pub machine: Option<String>,
}

/// How a session's live views size the page, as the view's Screen menu: the
/// browser's own window size, the viewer's screen, or a phone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenMode {
    #[default]
    Window,
    Native,
    Phone,
}

/// A phone's size (a common Android one), as the live view's Phone uses.
const PHONE_SIZE: (u32, u32) = (412, 915);

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateSessionRequest {
    /// Optional; generated when absent or empty.
    pub name: Option<String>,
    #[serde(default)]
    pub engine: BrowserEngine,
    pub profile: Option<String>,
    #[serde(default = "default_true")]
    pub persistent: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub record: Option<bool>,
    pub max_memory_mb: Option<u64>,
    /// Minutes of inactivity before the session is stopped (0 = never).
    pub idle_timeout_minutes: Option<u64>,
    /// Load the ad blocker (defaults to the node's --adblock-default).
    pub adblock: Option<bool>,
    /// Proxy id, or "none"; defaults to the profile's proxy.
    pub proxy: Option<String>,
    /// "dark" or "light"; defaults to the profile's (dark).
    pub color_scheme: Option<ColorScheme>,
    /// How live views size the page; Phone also launches at a phone's size.
    #[serde(default)]
    pub screen: ScreenMode,
    /// Live views mark taps and clicks when they open; defaults to the engine's setting.
    pub show_touches: Option<bool>,
}

/// Chromium writes into unpacked extensions and shows a blocking "Error
/// Loading Extension" dialog when it cannot (e.g. in the Nix store), so it
/// gets a writable copy next to the profiles directory, refreshed whenever
/// the source changes.
fn writable_extension_copy(src: &Path, profile_dir: &Path) -> Option<PathBuf> {
    fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let target = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_dir(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
                let mut perms = std::fs::metadata(&target)?.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                std::fs::set_permissions(&target, perms)?;
            }
        }
        Ok(())
    }

    let root = profile_dir
        .parent()
        .unwrap_or(profile_dir)
        .join("extensions");
    let dst = root.join("chromium-adblock");
    let marker = dst.join(".source");
    let source = src.display().to_string();
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(source.as_str()) {
        return Some(dst);
    }
    let _ = std::fs::remove_dir_all(&dst);
    match copy_dir(src, &dst).and_then(|_| std::fs::write(&marker, &source)) {
        Ok(()) => {
            info!(src = %source, dst = %dst.display(), "Prepared Chromium ad blocker");
            Some(dst)
        }
        Err(e) => {
            error!(src = %source, error = %e, "Could not prepare the Chromium ad blocker; ads stay unblocked");
            None
        }
    }
}

fn spawn_bidi_frame_feeder(hub: Arc<BidiHub>, handle: SessionRecorderHandle) {
    tokio::spawn(async move {
        let mut last = String::new();
        while !handle.stop_flag.load(Ordering::SeqCst) && !hub.is_closed() {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let Some(ctx) = hub.active_context().await else {
                continue;
            };
            let Ok(data) = hub.screenshot_jpeg(&ctx).await else {
                continue;
            };
            if data == last {
                continue;
            }
            let info = hub
                .call(
                    "script.evaluate",
                    serde_json::json!({
                        "expression": "JSON.stringify([location.href, document.title])",
                        "target": { "context": ctx },
                        "awaitPromise": false
                    }),
                )
                .await
                .ok()
                .and_then(|r| {
                    r.pointer("/result/value")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .and_then(|s| serde_json::from_str::<(String, String)>(&s).ok());
            let (url, title) = match info {
                Some((u, t)) => (Some(u), Some(t)),
                None => (None, None),
            };
            last = data.clone();
            if handle
                .frame_tx
                .send(recorder::ProxyRecordingFrame { url, title, data })
                .is_err()
            {
                break;
            }
        }
    });
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProfileInfo {
    /// Stable id; also the name of the profile's directory.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Display name.
    pub name: String,
    #[serde(default)]
    pub engine: BrowserEngine,
    /// Where the browser keeps this profile's data on its node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_dir: Option<String>,
    pub size_bytes: u64,
    pub is_active: bool,
    pub session_id: Option<Uuid>,
    pub created_at_secs: Option<u64>,
    pub modified_at_secs: Option<u64>,
    /// Proxy id new sessions of the profile use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Appearance of the profile's sessions.
    #[serde(default)]
    pub color_scheme: ColorScheme,
    /// Page a session's start tab opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ProfileDetails {
    #[serde(flatten)]
    pub profile: ProfileInfo,
    pub session: Option<SessionSummary>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateProfileRequest {
    pub name: String,
    pub clone_from: Option<String>,
    /// Engine of a new empty profile (a clone keeps its source's).
    #[serde(default)]
    pub engine: BrowserEngine,
    /// Proxy id new sessions of the profile use.
    pub proxy: Option<String>,
    /// Appearance of the profile's sessions (dark when unset).
    pub color_scheme: Option<ColorScheme>,
    /// Page a session's start tab opens.
    pub homepage: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateProfileRequest {
    pub name: Option<String>,
    /// Proxy id new sessions use; "none" (or "") clears it.
    pub proxy: Option<String>,
    /// Appearance of new sessions; a running one keeps its own until restarted.
    pub color_scheme: Option<ColorScheme>,
    /// Page new sessions' start tab opens; "" clears it.
    pub homepage: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CloneProfileRequest {
    pub target_name: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct ImportProfileParams {
    pub overwrite: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TransferProfileRequest {
    pub target_node: String,
    #[serde(default)]
    pub target_name: Option<String>,
    #[serde(default)]
    pub delete_source: bool,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Deserialize, Debug, Default)]
pub struct ClearProfileParams {
    pub force: Option<bool>,
    /// Take a safety backup first (the default).
    pub backup: Option<bool>,
}

#[derive(Deserialize, Debug, Default)]
pub struct CreateBackupRequest {
    pub label: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<BrowserEngine>,
    /// Playing sound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audible: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeInfo {
    pub name: String,
    pub url: String,
    pub version: String,
    /// Git commit it was built from (absent in builds without one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub uptime_seconds: u64,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    pub recordings_count: usize,
    pub storage_used_bytes: u64,
    pub max_storage_bytes: u64,
    #[serde(default)]
    pub supported_engines: Vec<BrowserEngine>,
    #[serde(default)]
    pub proxies: Vec<proxies::ProxyInfo>,
    #[serde(default)]
    pub settings: settings::NodeSettings,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    // The TUI owns the terminal: no log output.
    match args.command.take() {
        Some(Command::Tui(tui_args)) => return tui::run(tui_args).await,
        Some(Command::PasskeyHelper(helper_args)) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "browser_daemon=info".into()),
                )
                .init();
            return passkey_helper::run(helper_args).await;
        }
        None => {}
    }

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

    let chromium_adblock = args
        .chromium_adblock
        .as_deref()
        .and_then(|src| writable_extension_copy(src, &args.profile_dir));
    profiles::migrate(&args.profile_dir).await;
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

    if let Some(ref cfg) = config.audio {
        audio::remove_stale_sinks(cfg).await;
    }

    let settings = Arc::new(settings::Store::load(&settings_path));
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
        idle_timeout_secs: args.idle_timeout_minutes * 60,
        adblock_default: args.adblock_default,
        profiles_lock: Arc::new(Mutex::new(())),
        tab_order: Arc::new(std::sync::Mutex::new(TabOrder::default())),
        audible_pages: Arc::default(),
        viewers: Arc::default(),
        passkeys: Arc::default(),
        settings,
    };
    audible::spawn_monitor(state.clone());

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
            put(json_activate).post(json_activate).get(json_activate),
        )
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
        .route("/api/sessions/{id}/name", post(api_session_rename))
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
        .route("/api/recordings/vacuum", post(api_vacuum_recordings))
        // Peer-to-Peer Mesh APIs
        .route("/api/mesh/node", get(api_mesh_node))
        .route("/api/mesh/overview", get(api_mesh_overview))
        .route("/api/ws/overview", get(handle_overview_ws))
        .route("/api/peers", get(api_list_peers).post(api_add_peer))
        .route("/api/peers/{peer}", delete(api_remove_peer))
        .route("/api/peers/{peer}/sessions", post(api_peer_create_session))
        .route(
            "/api/peers/{peer}/sessions/{id}",
            delete(api_peer_delete_session),
        )
        .route(
            "/api/peers/{peer}/sessions/{id}/{action}",
            post(api_peer_session_action),
        )
        .route(
            "/api/peers/{peer}/settings",
            patch(api_peer_update_settings),
        )
        .route("/api/peers/{peer}/profiles", post(api_peer_create_profile))
        .route(
            "/api/peers/{peer}/profiles/{name}",
            delete(api_peer_delete_profile)
                .patch(api_peer_update_profile)
                .put(api_peer_update_profile),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/clone",
            post(api_peer_clone_profile),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/export",
            get(api_peer_export_profile),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/import",
            post(api_peer_import_profile)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/clear",
            post(api_peer_clear_profile),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/backups",
            get(api_peer_backups).post(api_peer_backups),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/backups/{backup}",
            delete(api_peer_backups),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/backups/{backup}/restore",
            post(api_peer_backups),
        )
        .route(
            "/api/peers/{peer}/profiles/{name}/transfer",
            post(api_peer_transfer_profile),
        )
        .route(
            "/api/peers/{peer}/recordings",
            get(api_peer_list_recordings),
        )
        .route(
            "/api/peers/{peer}/recordings/{id}",
            get(api_peer_get_recording).delete(api_peer_delete_recording),
        )
        .route(
            "/api/peers/{peer}/recordings/{id}/frames",
            get(api_peer_get_recording_frames),
        )
        .route(
            "/api/peers/{peer}/recordings/vacuum",
            post(api_peer_vacuum_recordings),
        )
        .route(
            "/api/peers/{peer}/json/new",
            put(api_peer_json_new)
                .post(api_peer_json_new)
                .get(api_peer_json_new),
        )
        .route(
            "/api/peers/{peer}/json/close/{id}",
            put(api_peer_json_close)
                .post(api_peer_json_close)
                .delete(api_peer_json_close)
                .get(api_peer_json_close),
        )
        .route(
            "/api/peers/{peer}/json/activate/{id}",
            put(api_peer_json_activate)
                .post(api_peer_json_activate)
                .get(api_peer_json_activate),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let mut supported_engines = vec!["chromium"];
    if state.config.firefox_bin.is_some() {
        supported_engines.push("firefox");
    }
    Json(serde_json::json!({
        "status": "ok",
        "node": state.node_name,
        "active_sessions": sessions.len(),
        "max_sessions": state.max_sessions,
        "peers_count": state.peers.read().await.len(),
        "supported_engines": supported_engines,
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
        if let Some(ref hub) = session.bidi {
            for ctx in hub.contexts().await {
                all_targets.push(firefox_target(&session, ctx));
            }
            continue;
        }
        let url = format!("http://127.0.0.1:{}/json/list", session.port);
        if let Ok(res) = state.http_client.get(&url).send().await {
            if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                for mut target in targets {
                    target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
                    target.session_id = Some(session.id);
                    target.profile = session.profile.clone();
                    target.engine = Some(BrowserEngine::Chromium);
                    all_targets.push(target);
                }
            }
        }
    }

    if let Ok(mut order) = state.tab_order.lock() {
        order.sort(&mut all_targets);
    }
    if let Ok(audible) = state.audible_pages.read() {
        for t in &mut all_targets {
            t.audible = audible.contains(&t.id);
        }
    }
    all_targets
}

fn firefox_target(session: &ManagedSession, ctx: bidi::BidiContext) -> DevToolsTarget {
    DevToolsTarget {
        id: ctx.id,
        title: ctx.title,
        target_type: "page".to_string(),
        url: ctx.url,
        devtools_frontend_url: None,
        web_socket_debugger_url: Some(format!("/firefox?session_id={}", session.id)),
        session_id: Some(session.id),
        profile: session.profile.clone(),
        engine: Some(BrowserEngine::Firefox),
        audible: false,
    }
}

/// Find the Firefox session owning a BiDi browsing context.
async fn find_firefox_context(state: &AppState, context: &str) -> Option<ManagedSession> {
    let sessions: Vec<ManagedSession> = state.sessions.read().await.values().cloned().collect();
    for session in sessions {
        if let Some(ref hub) = session.bidi {
            if hub.contexts().await.iter().any(|c| c.id == context) {
                return Some(session);
            }
        }
    }
    None
}

async fn json_list(State(state): State<AppState>) -> impl IntoResponse {
    Json(get_local_targets(&state).await)
}

async fn favicon_ico() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/x-icon"),
        )],
        FAVICON_ICO,
    )
}

async fn favicon_svg() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/svg+xml"),
        )],
        FAVICON_SVG,
    )
}

async fn handoff_favicon_ico() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/x-icon"),
        )],
        HANDOFF_ICO,
    )
}

async fn handoff_favicon_svg() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/svg+xml"),
        )],
        HANDOFF_SVG,
    )
}

async fn manifest_webmanifest() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/manifest+json"),
        )],
        MANIFEST_JSON,
    )
}

async fn service_worker_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        )],
        SW_JS,
    )
}

async fn icon_192_png() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        ICON_192,
    )
}

async fn icon_512_png() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        ICON_512,
    )
}

async fn download_worker_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        )],
        DOWNLOAD_WORKER_JS,
    )
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
                if let Some(ref hub) = s.bidi {
                    if hub.contexts().await.iter().any(|c| &c.id == pid) {
                        found = Some(s.clone());
                        break;
                    }
                    continue;
                }
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
        None => state
            .spawn_session(
                BrowserEngine::Chromium,
                None,
                false,
                None,
                None,
                Some(state.recorder_config.enabled),
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .await
            .map_err(|(st, err)| (st, Json(serde_json::json!({ "error": err }))))?,
    };

    session.activity.touch();
    if let Some(ref hub) = session.bidi {
        // Like Chromium below: an untouched about:blank start tab gives way to the new one.
        let start_tab =
            if target_url != "about:blank" && session.client_count.load(Ordering::SeqCst) == 0 {
                match hub.contexts().await.as_slice() {
                    [only] if only.url == "about:blank" => Some(only.id.clone()),
                    _ => None,
                }
            } else {
                None
            };
        let ctx = hub.create_tab(&target_url).await.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e })),
            )
        })?;
        if let Some(blank) = start_tab.filter(|id| *id != ctx.id) {
            if let Err(e) = hub
                .call(
                    "browsingContext.close",
                    serde_json::json!({ "context": blank }),
                )
                .await
            {
                warn!(session_id = %session.id, error = %e, "Could not close the blank start tab");
            }
        }
        if let Ok(mut order) = state.tab_order.lock() {
            order.rank(&ctx.id);
        }
        return Ok(Json(firefox_target(&session, ctx)));
    }

    // Chromium starts every session on an about:blank tab. When that is still the only
    // page and nothing is attached that might be about to use it, it is replaced by the
    // new tab instead of lingering as an empty second tab.
    let start_tab =
        if target_url != "about:blank" && session.client_count.load(Ordering::SeqCst) == 0 {
            let list_url = format!("http://127.0.0.1:{}/json/list", session.port);
            match state.http_client.get(&list_url).send().await {
                Ok(res) => res
                    .json::<Vec<DevToolsTarget>>()
                    .await
                    .ok()
                    .and_then(|targets| {
                        let pages: Vec<_> = targets
                            .into_iter()
                            .filter(|t| t.target_type == "page")
                            .collect();
                        match pages.as_slice() {
                            [only] if only.url == "about:blank" => Some(only.id.clone()),
                            _ => None,
                        }
                    }),
                Err(_) => None,
            }
        } else {
            None
        };

    let mut dest_url = url::Url::parse(&format!("http://127.0.0.1:{}/json/new", session.port))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;
    dest_url.set_query(Some(&target_url));

    let res = state.http_client.put(dest_url).send().await.map_err(|e| {
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

    if let Some(blank) = start_tab.filter(|id| *id != target.id) {
        let close_url = format!("http://127.0.0.1:{}/json/close/{}", session.port, blank);
        if let Err(e) = state.http_client.get(&close_url).send().await {
            warn!(session_id = %session.id, error = %e, "Could not close the blank start tab");
        }
    }

    target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
    target.session_id = Some(session.id);
    target.profile = session.profile.clone();
    target.engine = Some(BrowserEngine::Chromium);
    if let Ok(mut order) = state.tab_order.lock() {
        order.rank(&target.id);
    }

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

    if let Some(session) = find_firefox_context(&state, &id).await {
        if let Some(ref hub) = session.bidi {
            return match hub.close_tab(&id).await {
                Ok(()) => (StatusCode::OK, "Target is closing").into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
            };
        }
    }

    for session in sessions {
        if session.engine == BrowserEngine::Firefox {
            continue;
        }
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

    if let Some(session) = find_firefox_context(&state, &id).await {
        if let Some(ref hub) = session.bidi {
            return match hub.activate_tab(&id).await {
                Ok(()) => (StatusCode::OK, "Target activated").into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
            };
        }
    }

    for session in sessions {
        if session.engine == BrowserEngine::Firefox {
            continue;
        }
        let req_url = format!("http://127.0.0.1:{}/json/activate/{}", session.port, id);
        if let Ok(res) = state.http_client.put(&req_url).send().await {
            if res.status().is_success() {
                return (StatusCode::OK, "Target activated").into_response();
            }
        }
    }

    (StatusCode::NOT_FOUND, "Target not found").into_response()
}

/// Registers a connecting automation client with its session; `client_gone` with
/// the returned id when it disconnects.
async fn client_arrived(
    state: &AppState,
    session: &ManagedSession,
    headers: &HeaderMap,
    params: &LaunchParams,
) -> u64 {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (kind, agent) = clients::kind_of(ua.as_deref());
    // This node's own address too: a client may come in over it (the mesh URL).
    let mut peers: Vec<(String, String)> =
        vec![(state.node_name.clone(), state.public_url.clone())];
    peers.extend(
        state
            .peers
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    let addr = clients::remote_addr(headers);
    let from = clients::describe_origin(addr, &state.node_name, &peers).await;
    info!(session_id = %session.id, kind, from = %from, "Automation client connected");
    session.clients.add(clients::ClientInfo {
        kind: kind.into(),
        agent,
        from,
        addr: addr.map(|a| a.to_string()),
        user_agent: ua,
        since: clients::now_secs(),
        agent_name: clients::clean(params.agent.as_deref()),
        conversation: clients::clean(params.conversation.as_deref()),
        cwd: clients::clean(params.cwd.as_deref()),
        machine: clients::clean(params.machine.as_deref()),
        topic: None,
    })
}

async fn handle_chromium_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    // Playwright never takes over tabs it didn't open: it opens its own and the start
    // tab would just sit there. Other clients (Puppeteer scripts take the first tab)
    // keep it.
    let agent_opens_own_tab = clients::kind_of(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .0 == "Playwright";
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
                params.engine.unwrap_or(BrowserEngine::Chromium),
                params.profile.as_deref(),
                persistent,
                params.width,
                params.height,
                params.record,
                params.max_memory_mb,
                params.idle_timeout_minutes,
                params.adblock,
                params.proxy.as_deref(),
                params.color_scheme,
                !agent_opens_own_tab,
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    ws.on_upgrade(move |socket| async move {
        session.client_count.fetch_add(1, Ordering::SeqCst);
        let client = client_arrived(&state, &session, &headers, &params).await;
        session.activity.touch();

        let ws_path = session.ws_path.trim_start_matches('/');
        let backend_ws_url = format!("ws://127.0.0.1:{}/{}", session.port, ws_path);
        info!(session_id = %session.id, url = %backend_ws_url, "Connecting client to Chromium DevTools");

        let frame_tx = {
            let recs = state.active_recordings.read().await;
            recs.get(&session.id).map(|h| h.frame_tx.clone())
        };

        match tokio_tungstenite::connect_async(&backend_ws_url).await {
            Ok((server_ws, _)) => {
                let activity = Some((session.activity.clone(), false));
                bridge_websockets(socket, server_ws, frame_tx, activity, None, None).await;
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Failed to connect to Chromium backend");
            }
        }

        session.clients.remove(client);
        let remaining = session.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if !session.persistent && remaining == 0 {
            info!(session_id = %session.id, "Auto-terminating non-persistent session");
            state.terminate_session(&session.id).await;
        }
    })
}

async fn handle_firefox_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if state.config.firefox_bin.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Firefox engine is not configured on this host",
        )
            .into_response();
    }

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
                BrowserEngine::Firefox,
                params.profile.as_deref(),
                persistent,
                params.width,
                params.height,
                params.record,
                params.max_memory_mb,
                params.idle_timeout_minutes,
                params.adblock,
                params.proxy.as_deref(),
                params.color_scheme,
                true,
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    let Some(hub) = session.bidi.clone() else {
        return (StatusCode::BAD_REQUEST, "Not a Firefox session").into_response();
    };
    // The handoff page attaches as an observer: it must not keep sessions alive.
    let counted = !params.observer.unwrap_or(false);

    ws.on_upgrade(move |socket| async move {
        let client = if counted {
            session.client_count.fetch_add(1, Ordering::SeqCst);
            Some(client_arrived(&state, &session, &headers, &params).await)
        } else {
            None
        };
        info!(session_id = %session.id, observer = !counted, "Client attached to Firefox WebDriver BiDi");

        session.activity.touch();
        hub.serve_client(socket, session.activity.clone(), !counted)
            .await;

        if let Some(c) = client {
            session.clients.remove(c);
        }
        if counted {
            let remaining = session.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
            if !session.persistent && remaining == 0 {
                info!(session_id = %session.id, "Auto-terminating non-persistent session");
                state.terminate_session(&session.id).await;
            }
        }
    })
}

async fn handle_page_ws(
    AxumPath(page_id): AxumPath<String>,
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
) -> Response {
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };

    let mut target_info: Option<(
        u16,
        Uuid,
        Arc<idle::Activity>,
        Option<String>,
        Option<String>,
    )> = None;
    for session in sessions {
        if session.bidi.is_some() {
            continue;
        }
        let url = format!("http://127.0.0.1:{}/json/list", session.port);
        if let Ok(res) = state.http_client.get(&url).send().await {
            if let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await {
                if let Some(target) = targets.into_iter().find(|t| t.id == page_id) {
                    target_info = Some((
                        session.port,
                        session.id,
                        session.activity.clone(),
                        target.title,
                        Some(target.url),
                    ));
                    break;
                }
            }
        }
    }

    let (port, session_id, activity, initial_title, initial_url) = match target_info {
        Some(info) => info,
        None => {
            warn!(page_id = %page_id, "Page not found across active sessions");
            return (StatusCode::NOT_FOUND, "Target page not found").into_response();
        }
    };

    let frame_tx = {
        let recs = state.active_recordings.read().await;
        recs.get(&session_id).map(|h| h.frame_tx.clone())
    };

    // The handoff page attaches as an observer: watching alone is not activity.
    let observer = params.observer.unwrap_or(false);
    if !observer {
        activity.touch();
    }
    let backend_url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, page_id);
    ws.on_upgrade(move |socket| async move {
        match tokio_tungstenite::connect_async(&backend_url).await {
            Ok((server_ws, _)) => {
                let activity = Some((activity, observer));
                bridge_websockets(
                    socket,
                    server_ws,
                    frame_tx,
                    activity,
                    initial_url,
                    initial_title,
                )
                .await;
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
        let rec = rec_guard.get(&session.id);
        summaries.push(session.to_summary(&state.http_client, rec).await);
    }
    summaries
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

async fn profile_session(state: &AppState, id: Uuid) -> Option<ManagedSession> {
    let guard = state.sessions.read().await;
    guard.values().find(|s| s.profile_id == Some(id)).cloned()
}

async fn profile_info(state: &AppState, profile: &Profile) -> ProfileInfo {
    let session_id = profile_session(state, profile.meta.id).await.map(|s| s.id);
    let modified_at_secs = fs::metadata(&profile.dir)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    ProfileInfo {
        id: Some(profile.meta.id),
        name: profile.meta.name.clone(),
        engine: profile.meta.engine,
        storage_dir: Some(profile.dir.display().to_string()),
        size_bytes: compute_dir_size(profile.dir.clone()).await,
        is_active: session_id.is_some(),
        session_id,
        created_at_secs: Some(profile.meta.created_at_secs),
        modified_at_secs,
        proxy: profile.meta.proxy.clone(),
        color_scheme: profile.meta.color_scheme.unwrap_or_default(),
        homepage: profile.meta.homepage.clone(),
    }
}

async fn lookup_profile(state: &AppState, key: &str) -> Result<Profile, ApiError> {
    profiles::find(&state.config.profiles_dir, key)
        .await
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("Profile '{}' not found", key),
            )
        })
}

fn new_profile_name(name: &str) -> Result<String, ApiError> {
    profiles::clean_name(name)
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "Invalid profile name"))
}

async fn ensure_name_free(
    state: &AppState,
    name: &str,
    except: Option<Uuid>,
) -> Result<(), ApiError> {
    if profiles::name_taken(&state.config.profiles_dir, name, except).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile '{}' already exists", name),
        ));
    }
    Ok(())
}

/// Copy a profile's data into a new id directory under a new name.
async fn copy_profile(state: &AppState, source: &Profile, name: &str) -> Result<Profile, ApiError> {
    let dir = state.config.profiles_dir.join(Uuid::new_v4().to_string());
    if let Err(e) = copy_dir_all(source.dir.clone(), dir.clone()).await {
        error!(from = %source.meta.name, to = %name, error = %e, "Failed to copy profile directory");
        let _ = fs::remove_dir_all(&dir).await;
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to copy profile directory",
        ));
    }
    profiles::adopt(dir, name, source.meta.engine)
        .await
        .map_err(|e| {
            error!(profile = %name, error = %e, "Failed to write profile metadata");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write profile metadata",
            )
        })
}

async fn get_local_profiles(state: &AppState) -> Vec<ProfileInfo> {
    let mut infos = Vec::new();
    for profile in profiles::list(&state.config.profiles_dir).await {
        infos.push(profile_info(state, &profile).await);
    }
    infos
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
    let phone = req.screen == ScreenMode::Phone;
    let session = state
        .spawn_session(
            req.engine,
            req.profile.as_deref(),
            req.persistent,
            req.width.or(phone.then_some(PHONE_SIZE.0)),
            req.height.or(phone.then_some(PHONE_SIZE.1)),
            req.record,
            req.max_memory_mb,
            req.idle_timeout_minutes,
            req.adblock,
            req.proxy.as_deref(),
            req.color_scheme,
            true,
        )
        .await
        .map_err(|(status, msg)| (status, Json(serde_json::json!({ "error": msg }))))?;
    if let Some(name) = req.name.as_deref().and_then(names::clean) {
        *session.name.write().unwrap() = name;
    }
    *session.screen.write().unwrap() = req.screen;
    *session.show_touches.write().unwrap() = req.show_touches;

    let rec = state
        .active_recordings
        .read()
        .await
        .get(&session.id)
        .cloned();
    let summary = session.to_summary(&state.http_client, rec.as_ref()).await;
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
        guard.get(&id).cloned().ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ))?
    };

    let rec = state
        .active_recordings
        .read()
        .await
        .get(&session.id)
        .cloned();
    let summary = session.to_summary(&state.http_client, rec.as_ref()).await;
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
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.name)?;
    let proxy = req
        .proxy
        .as_deref()
        .map(|k| state.check_proxy(k))
        .transpose()?;
    let _guard = state.profiles_lock.lock().await;
    ensure_name_free(&state, &name, None).await?;

    let mut profile = match req.clone_from.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(source) => {
            let source = lookup_profile(&state, source).await?;
            copy_profile(&state, &source, &name).await?
        }
        None => profiles::create(&state.config.profiles_dir, &name, req.engine)
            .await
            .map_err(|e| {
                error!(profile = %name, error = %e, "Failed to create profile");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create profile",
                )
            })?,
    };

    let homepage = req
        .homepage
        .as_deref()
        .map(homepage_url)
        .transpose()?
        .flatten();
    if proxy.is_some() || req.color_scheme.is_some() || homepage.is_some() {
        if let Some(proxy) = proxy {
            profile.meta.proxy = proxy;
        }
        if req.color_scheme.is_some() {
            profile.meta.color_scheme = req.color_scheme;
        }
        if homepage.is_some() {
            profile.meta.homepage = homepage;
        }
        profiles::write_meta(&profile.dir, &profile.meta)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    info!(profile = %name, id = %profile.meta.id, "Profile created");
    Ok((
        StatusCode::CREATED,
        Json(profile_info(&state, &profile).await),
    ))
}

async fn api_get_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let session = match profile_session(&state, profile.meta.id).await {
        Some(s) => {
            let rec = state.active_recordings.read().await.get(&s.id).cloned();
            Some(s.to_summary(&state.http_client, rec.as_ref()).await)
        }
        None => None,
    };
    Ok(Json(ProfileDetails {
        profile: profile_info(&state, &profile).await,
        session,
    }))
}

/// Rename a profile. Its directory is named by id, so a running session keeps going.
/// A profile homepage as given: an http(s) address (a bare host gets https://);
/// empty clears it.
fn homepage_url(input: &str) -> Result<Option<String>, ApiError> {
    let v = input.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let full = if v.contains("://") {
        v.to_string()
    } else {
        format!("https://{v}")
    };
    match url::Url::parse(&full) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
            Ok(Some(u.to_string()))
        }
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "The homepage must be an http(s) address",
        )),
    }
}

async fn api_update_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let mut profile = lookup_profile(&state, &key).await?;
    if let Some(ref requested) = req.name {
        let name = new_profile_name(requested)?;
        if name != profile.meta.name {
            ensure_name_free(&state, &name, Some(profile.meta.id)).await?;
            let old = std::mem::replace(&mut profile.meta.name, name.clone());
            if let Err(e) = profiles::write_meta(&profile.dir, &profile.meta).await {
                error!(profile = %old, error = %e, "Failed to write profile metadata");
                return Err(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to rename profile",
                ));
            }
            for session in state.sessions.write().await.values_mut() {
                if session.profile_id == Some(profile.meta.id) {
                    session.profile = Some(name.clone());
                }
            }
            info!(from = %old, to = %name, id = %profile.meta.id, "Profile renamed");
        }
    }
    if let Some(ref requested) = req.proxy {
        let proxy = state.check_proxy(requested)?;
        if proxy != profile.meta.proxy {
            profile.meta.proxy = proxy;
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            // A running session keeps its proxy until it is restarted.
            info!(profile = %profile.meta.name, proxy = ?profile.meta.proxy, "Profile proxy changed");
        }
    }
    if let Some(scheme) = req.color_scheme {
        if Some(scheme) != profile.meta.color_scheme {
            profile.meta.color_scheme = Some(scheme);
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            info!(profile = %profile.meta.name, color_scheme = ?scheme, "Profile appearance changed");
        }
    }
    if let Some(ref requested) = req.homepage {
        let homepage = homepage_url(requested)?;
        if homepage != profile.meta.homepage {
            profile.meta.homepage = homepage;
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            info!(profile = %profile.meta.name, homepage = ?profile.meta.homepage, "Profile homepage changed");
        }
    }
    Ok(Json(profile_info(&state, &profile).await))
}

async fn api_clone_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CloneProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.target_name)?;
    let _guard = state.profiles_lock.lock().await;
    let source = lookup_profile(&state, &key).await?;
    ensure_name_free(&state, &name, None).await?;
    let profile = copy_profile(&state, &source, &name).await?;
    info!(from = %source.meta.name, to = %name, "Profile cloned");
    Ok((
        StatusCode::CREATED,
        Json(profile_info(&state, &profile).await),
    ))
}

async fn api_delete_profile(
    AxumPath(key): AxumPath<String>,
    Query(params): Query<DeleteProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    if let Some(session) = profile_session(&state, profile.meta.id).await {
        if !params.force.unwrap_or(false) {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("Profile is currently in use by active session {}. Pass ?force=true to terminate session and delete.", session.id),
            ));
        }
        info!(profile = %profile.meta.name, session_id = %session.id, "Force terminating session before deleting profile");
        state.terminate_session(&session.id).await;
    }

    if let Err(e) = fs::remove_dir_all(&profile.dir).await {
        error!(profile = %profile.meta.name, error = %e, "Failed to remove profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to remove profile directory",
        ));
    }

    backups::remove_all(&state.config.profiles_dir, profile.meta.id).await;
    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile removed");
    Ok((
        StatusCode::OK,
        Json(
            serde_json::json!({ "status": "deleted", "profile": profile.meta.name, "id": profile.meta.id }),
        ),
    ))
}

/// Make sure no browser holds the profile: refuse while a session uses it,
/// unless `force` stops that session.
async fn release_profile(
    state: &AppState,
    profile: &Profile,
    force: bool,
    action: &str,
) -> Result<(), ApiError> {
    let Some(session) = profile_session(state, profile.meta.id).await else {
        return Ok(());
    };
    if !force {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile is currently in use by active session {}. Pass ?force=true to terminate session and {action}.", session.id),
        ));
    }
    info!(profile = %profile.meta.name, session_id = %session.id, "Force terminating session before {action}");
    state.terminate_session(&session.id).await;
    Ok(())
}

/// Back the profile up before a destructive action, keeping only the newest few.
async fn auto_backup(state: &AppState, profile: &Profile, what: &str) -> Result<(), ApiError> {
    let dir = &state.config.profiles_dir;
    backups::create(
        dir,
        profile,
        Some(&format!("{}{what}", backups::AUTO_PREFIX)),
    )
    .await
    .map_err(|e| {
        error!(profile = %profile.meta.name, error = %e, "Safety backup failed");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Safety backup failed, nothing was changed: {e}"),
        )
    })?;
    backups::prune_auto(dir, profile.meta.id).await;
    Ok(())
}

async fn api_list_backups(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    Ok(Json(
        backups::list(&state.config.profiles_dir, profile.meta.id).await,
    ))
}

async fn api_create_backup(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateBackupRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    if let Some(session) = profile_session(&state, profile.meta.id).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "Cannot back up an active profile (in use by session {})",
                session.id
            ),
        ));
    }
    let label = body.and_then(|Json(b)| b.label);
    let info = backups::create(&state.config.profiles_dir, &profile, label.as_deref())
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, error = %e, "Backup failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
    info!(profile = %profile.meta.name, backup = %info.id, size_bytes = info.size_bytes, "Profile backed up");
    Ok((StatusCode::CREATED, Json(info)))
}

async fn api_delete_backup(
    AxumPath((key, backup)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    if !backups::delete(&state.config.profiles_dir, profile.meta.id, &backup).await {
        return Err(api_error(StatusCode::NOT_FOUND, "Backup not found"));
    }
    Ok(Json(
        serde_json::json!({ "status": "deleted", "backup": backup }),
    ))
}

async fn api_restore_backup(
    AxumPath((key, backup)): AxumPath<(String, String)>,
    Query(params): Query<DeleteProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let profile = lookup_profile(&state, &key).await?;
    let dir = &state.config.profiles_dir;
    if backups::get(dir, profile.meta.id, &backup).await.is_none() {
        return Err(api_error(StatusCode::NOT_FOUND, "Backup not found"));
    }
    release_profile(&state, &profile, params.force.unwrap_or(false), "restore").await?;
    auto_backup(&state, &profile, "pre-restore").await?;
    backups::restore(dir, &profile, &backup)
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, backup = %backup, error = %e, "Restore failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
    info!(profile = %profile.meta.name, backup = %backup, "Profile restored from backup");
    Ok(Json(profile_info(&state, &profile).await))
}

/// Wipe a profile's cookies, logins and storage but keep the profile itself
/// (its name, engine, proxy, appearance and homepage).
async fn api_clear_profile(
    AxumPath(key): AxumPath<String>,
    Query(params): Query<ClearProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    release_profile(&state, &profile, params.force.unwrap_or(false), "clear").await?;
    // A safety net: clearing cannot be undone otherwise.
    if params.backup.unwrap_or(true) {
        auto_backup(&state, &profile, "pre-clear").await?;
    }

    let io_err = |e: std::io::Error| {
        error!(profile = %profile.meta.name, error = %e, "Failed to clear profile data");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to clear profile data: {e}"),
        )
    };
    let mut entries = fs::read_dir(&profile.dir).await.map_err(io_err)?;
    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        if entry.file_name() == profiles::META_FILE {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().await.map_err(io_err)?.is_dir();
        if is_dir {
            fs::remove_dir_all(&path).await.map_err(io_err)?;
        } else {
            fs::remove_file(&path).await.map_err(io_err)?;
        }
    }

    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile data cleared");
    Ok(Json(profile_info(&state, &profile).await))
}

async fn export_profile_archive(state: &AppState, profile: &Profile) -> Result<Vec<u8>, ApiError> {
    if let Some(session) = profile_session(state, profile.meta.id).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "Cannot export active profile (in use by session {})",
                session.id
            ),
        ));
    }

    let output = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg("-")
        .arg("-C")
        .arg(&profile.dir)
        .arg(".")
        .output()
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, error = %e, "Failed to run tar command for profile export");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to archive profile: {}", e))
        })?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        error!(profile = %profile.meta.name, error = %err, "tar export failed");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tar command failed: {}", err),
        ));
    }

    Ok(output.stdout)
}

async fn api_export_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let bytes = export_profile_archive(&state, &profile).await?;
    // Header values are ASCII: keep a readable file name, fall back to the id.
    let file_stem: String = profile
        .meta
        .name
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' || *c == ' ')
        .collect();
    let file_stem = match file_stem.trim() {
        "" => profile.meta.id.to_string(),
        s => s.to_string(),
    };
    let cd_value = format!("attachment; filename=\"{}.tar.gz\"", file_stem);
    let cd_header =
        HeaderValue::from_str(&cd_value).unwrap_or_else(|_| HeaderValue::from_static("attachment"));

    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/gzip"),
            ),
            (header::CONTENT_DISPOSITION, cd_header),
        ],
        bytes,
    ))
}

/// Import an archive as the profile named in the path. The archive's own
/// `profile.json` supplies the engine; archives from the old name-based
/// layout fall back to the `firefox_` naming convention.
async fn api_import_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ImportProfileParams>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&name)?;
    if body.is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "Empty archive body"));
    }

    let _guard = state.profiles_lock.lock().await;
    let existing = profiles::find(&state.config.profiles_dir, &name)
        .await
        .filter(|p| p.meta.name == name);
    if let Some(ref existing) = existing {
        if !params.overwrite.unwrap_or(false) {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("Profile '{}' already exists on target", name),
            ));
        }
        if let Some(session) = profile_session(&state, existing.meta.id).await {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!(
                    "Cannot overwrite active profile (in use by session {})",
                    session.id
                ),
            ));
        }
    }

    let target_dir = state.config.profiles_dir.join(Uuid::new_v4().to_string());
    if let Err(e) = fs::create_dir_all(&target_dir).await {
        error!(profile = %name, error = %e, "Failed to create profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create profile directory",
        ));
    }

    let mut child = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg("-")
        .arg("-C")
        .arg(&target_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            error!(error = %e, "Failed to spawn tar process for import");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn tar: {}", e),
            )
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(&body).await {
            error!(error = %e, "Failed to write archive stream to tar stdin");
            let _ = fs::remove_dir_all(&target_dir).await;
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write to tar: {}", e),
            ));
        }
    }

    let output = child.wait_with_output().await.map_err(|e| {
        error!(error = %e, "Failed to wait on tar process");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tar extraction error: {}", e),
        )
    })?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        error!(profile = %name, error = %err, "tar extraction failed");
        let _ = fs::remove_dir_all(&target_dir).await;
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tar extraction failed: {}", err),
        ));
    }

    let (name, engine) = match profiles::read_meta(&target_dir).await {
        Some(meta) => (name, meta.engine),
        None => profiles::legacy_identity(&name),
    };
    let profile = match profiles::adopt(target_dir.clone(), &name, engine).await {
        Ok(p) => p,
        Err(e) => {
            error!(profile = %name, error = %e, "Failed to write profile metadata");
            let _ = fs::remove_dir_all(&target_dir).await;
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write profile metadata",
            ));
        }
    };
    // Replace the old copy only once the new one is complete.
    if let Some(existing) = existing {
        if let Err(e) = fs::remove_dir_all(&existing.dir).await {
            warn!(profile = %name, error = %e, "Imported profile, but failed to remove the overwritten copy");
        }
    }

    let info = profile_info(&state, &profile).await;
    info!(profile = %name, id = %profile.meta.id, size_bytes = %info.size_bytes, "Profile imported successfully");
    Ok((StatusCode::CREATED, Json(info)))
}

async fn api_transfer_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TransferProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let source = lookup_profile(&state, &key).await?;

    let target_node = req.target_node.trim();
    if target_node.is_empty() || target_node == state.node_name {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Target node must be a different node in the cluster",
        ));
    }

    let target_name = req
        .target_name
        .as_deref()
        .and_then(profiles::clean_name)
        .unwrap_or_else(|| source.meta.name.clone());

    let target_peer_url = get_peer_url(&state, target_node).await.ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Target node '{}' not found in mesh", target_node),
        )
    })?;

    // 1. Export local profile to bytes (profile.json carries the engine)
    let archive_bytes = export_profile_archive(&state, &source).await?;

    // 2. Upload to target node's /api/profiles/{target_name}/import
    let mut import_url = peer_profile_url(&target_peer_url, &target_name, Some("import"));
    if req.overwrite {
        import_url.push_str("?overwrite=true");
    }

    let upload_res = state
        .peer_request(state.http_client.post(&import_url))
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(archive_bytes)
        .send()
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to upload profile archive to target node");
            api_error(
                StatusCode::BAD_GATEWAY,
                format!("Failed to send profile to target node: {}", e),
            )
        })?;

    if !upload_res.status().is_success() {
        let status =
            StatusCode::from_u16(upload_res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let error_json = upload_res.json::<serde_json::Value>().await.unwrap_or_else(
            |_| serde_json::json!({ "error": "Target node rejected profile import" }),
        );
        return Err((status, Json(error_json)));
    }

    // 3. If delete_source was requested, remove local directory
    if req.delete_source {
        if let Err(e) = fs::remove_dir_all(&source.dir).await {
            warn!(profile = %source.meta.name, error = %e, "Profile transferred, but failed to delete source directory");
        } else {
            info!(profile = %source.meta.name, "Source profile deleted after transfer");
        }
    }

    info!(
        source = %source.meta.name,
        target_node = %target_node,
        target_name = %target_name,
        "Profile transferred successfully"
    );

    Ok(Json(serde_json::json!({
        "status": "transferred",
        "source_node": state.node_name,
        "target_node": target_node,
        "source_name": source.meta.name,
        "source_id": source.meta.id,
        "target_name": target_name,
        "deleted_source": req.delete_source,
    })))
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
    let mut supported_engines = vec![BrowserEngine::Chromium];
    if state.config.firefox_bin.is_some() {
        supported_engines.push(BrowserEngine::Firefox);
    }

    Ok(Json(NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        revision: REVISION.map(str::to_string),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: sessions_count,
        max_sessions: state.max_sessions,
        profiles_count,
        recordings_count,
        storage_used_bytes,
        max_storage_bytes,
        supported_engines,
        proxies: state.proxy_infos(),
        settings: state.settings.get(),
    }))
}

pub async fn fetch_mesh_overview(state: &AppState) -> MeshOverview {
    let local_sessions = get_local_sessions(state).await;
    let local_profiles = get_local_profiles(state).await;
    let local_targets = get_local_targets(state).await;
    let local_recordings = recorder::list_recordings(&state.recorder_config).await;
    let local_storage_used_bytes: u64 = local_recordings.iter().map(|r| r.size_bytes).sum();
    let mut local_supported_engines = vec![BrowserEngine::Chromium];
    if state.config.firefox_bin.is_some() {
        local_supported_engines.push(BrowserEngine::Firefox);
    }

    let local_node = NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        revision: REVISION.map(str::to_string),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: local_sessions.len(),
        max_sessions: state.max_sessions,
        profiles_count: local_profiles.len(),
        recordings_count: local_recordings.len(),
        storage_used_bytes: local_storage_used_bytes,
        max_storage_bytes: state.recorder_config.max_storage_bytes,
        supported_engines: local_supported_engines,
        proxies: state.proxy_infos(),
        settings: state.settings.get(),
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
                    builder = builder.bearer_auth(s).header("X-Browser-Cluster-Secret", s);
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
                    let (
                        node_name,
                        active_sessions,
                        max_sessions,
                        profiles_count,
                        uptime_seconds,
                        recordings_count,
                        storage_used_bytes,
                        max_storage_bytes,
                        supported_engines,
                        proxies,
                        version,
                        revision,
                        settings,
                    ) = match node_info {
                        Ok(info) => (
                            info.name,
                            info.active_sessions,
                            info.max_sessions,
                            info.profiles_count,
                            info.uptime_seconds,
                            info.recordings_count,
                            info.storage_used_bytes,
                            info.max_storage_bytes,
                            info.supported_engines,
                            info.proxies,
                            Some(info.version),
                            info.revision,
                            Some(info.settings),
                        ),
                        Err(_) => (
                            p_name.clone(),
                            0,
                            8,
                            0,
                            0,
                            0,
                            0,
                            0,
                            vec![BrowserEngine::Chromium],
                            Vec::new(),
                            None,
                            None,
                            None,
                        ),
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
                            version,
                            revision,
                            latency_ms: Some(latency_ms),
                            active_sessions,
                            max_sessions,
                            profiles_count,
                            recordings_count,
                            storage_used_bytes,
                            uptime_seconds,
                            supported_engines,
                            proxies,
                            settings,
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
                        version: None,
                        revision: None,
                        latency_ms: Some(latency_ms),
                        active_sessions: 0,
                        max_sessions: 8,
                        profiles_count: 0,
                        recordings_count: 0,
                        storage_used_bytes: 0,
                        uptime_seconds: 0,
                        supported_engines: vec![BrowserEngine::Chromium],
                        proxies: Vec::new(),
                        settings: None,
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
                        version: None,
                        revision: None,
                        latency_ms: None,
                        active_sessions: 0,
                        max_sessions: 8,
                        profiles_count: 0,
                        recordings_count: 0,
                        storage_used_bytes: 0,
                        uptime_seconds: 0,
                        supported_engines: vec![BrowserEngine::Chromium],
                        proxies: Vec::new(),
                        settings: None,
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
        if let Ok((
            p_state,
            node_name,
            node_url,
            p_sessions,
            p_profiles,
            p_targets,
            p_recordings,
            p_max_storage,
        )) = task.await
        {
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

    all_recordings.sort_by(|a, b| {
        b.recording
            .started_at_secs
            .cmp(&a.recording.started_at_secs)
    });

    let total_nodes = 1 + peer_states.len();
    let total_sessions = all_sessions.len();
    let total_profiles = all_profiles.len();
    let total_recordings = all_recordings.len();

    MeshOverview {
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
    }
}

async fn api_mesh_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let overview = fetch_mesh_overview(&state).await;
    Ok(Json(overview))
}

async fn handle_overview_ws(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let token_opt = params.get("token").map(|s| s.as_str());
    if let Err(e) = state.check_auth_opt(&headers, token_opt) {
        return e.into_response();
    }

    ws.on_upgrade(move |mut socket| async move {
        // Send initial overview immediately
        let overview = fetch_mesh_overview(&state).await;
        if let Ok(json_str) = serde_json::to_string(&overview) {
            if socket.send(axum::extract::ws::Message::Text(json_str.into())).await.is_err() {
                return;
            }
        }

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(2000));
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let overview = fetch_mesh_overview(&state).await;
                    if let Ok(json_str) = serde_json::to_string(&overview) {
                        if socket.send(axum::extract::ws::Message::Text(json_str.into())).await.is_err() {
                            break;
                        }
                    }
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(axum::extract::ws::Message::Text(txt))) => {
                            if txt.as_str() == "refresh" {
                                let overview = fetch_mesh_overview(&state).await;
                                if let Ok(json_str) = serde_json::to_string(&overview) {
                                    let _ = socket.send(axum::extract::ws::Message::Text(json_str.into())).await;
                                }
                            } else if txt.as_str() == "ping" {
                                let _ = socket.send(axum::extract::ws::Message::Text("pong".into())).await;
                            }
                        }
                        Some(Ok(axum::extract::ws::Message::Ping(data))) => {
                            if socket.send(axum::extract::ws::Message::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(axum::extract::ws::Message::Close(_))) | None | Some(Err(_)) => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    })
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
            let mut headers = HeaderMap::new();
            if let Some(ct) = res.headers().get(header::CONTENT_TYPE) {
                headers.insert(header::CONTENT_TYPE, ct.clone());
            } else {
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            if let Some(cd) = res.headers().get(header::CONTENT_DISPOSITION) {
                headers.insert(header::CONTENT_DISPOSITION, cd.clone());
            }
            let bytes = res.bytes().await.unwrap_or_default();
            (status, headers, bytes).into_response()
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

/// Proxy `record` / `stop-record` to the peer owning the session.
async fn api_peer_session_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, id, action)): AxumPath<(String, String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    if !matches!(
        action.as_str(),
        "record" | "stop-record" | "pause-record" | "resume-record" | "idle-timeout"
    ) {
        return (StatusCode::NOT_FOUND, "Unknown session action").into_response();
    }
    let Some(peer_url) = get_peer_url(&state, &peer).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
        )
            .into_response();
    };
    let url = match query {
        Some(q) if !q.is_empty() => format!("{}/api/sessions/{}/{}?{}", peer_url, id, action, q),
        _ => format!("{}/api/sessions/{}/{}", peer_url, id, action),
    };
    let res = state
        .peer_request(state.http_client.post(&url))
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
    let mut url = peer_profile_url(&peer_url, &name, None);
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
    let url = peer_profile_url(&peer_url, &name, Some("clone"));
    let res = state
        .peer_request(state.http_client.post(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

#[derive(Deserialize, Serialize)]
struct UpdateSettingsRequest {
    /// Proxy id; "none" or "" for direct. Absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_proxy: Option<String>,
    /// Engine defaults, replaced as a whole. Absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engines: Option<settings::Engines>,
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
    let mut settings = state.settings.get();
    if let Some(ref key) = req.default_proxy {
        settings.default_proxy = if key.trim().is_empty() {
            None
        } else {
            state.check_proxy(key)?
        };
    }
    if let Some(engines) = req.engines {
        settings.engines = engines;
    }
    state.settings.set(settings.clone()).await.map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not save the settings: {e}"),
        )
    })?;
    info!(settings = ?settings, "Node settings changed");
    Ok(Json(settings))
}

async fn api_peer_update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
    Json(req): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(peer_url) = get_peer_url(&state, &peer).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
        )
            .into_response();
    };
    let url = format!("{}/api/settings", peer_url.trim_end_matches('/'));
    let res = state
        .peer_request(state.http_client.patch(&url))
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
    let url = peer_profile_url(&peer_url, &name, None);
    let res = state
        .peer_request(state.http_client.patch(&url))
        .json(&req)
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_clear_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> impl IntoResponse {
    forward_profile_request(
        &state,
        &headers,
        reqwest::Method::POST,
        &peer,
        &name,
        &["clear"],
        query,
        None,
    )
    .await
}

/// Backup routes on a peer: the request's path below the profile is replayed there.
async fn api_peer_backups(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: axum::http::Method,
    uri: axum::http::Uri,
    AxumPath(params): AxumPath<HashMap<String, String>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: Bytes,
) -> impl IntoResponse {
    let (Some(peer), Some(name)) = (params.get("peer"), params.get("name")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut tail = vec!["backups"];
    if let Some(backup) = params.get("backup") {
        tail.push(backup);
        if uri.path().ends_with("/restore") {
            tail.push("restore");
        }
    }
    let method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    forward_profile_request(
        &state,
        &headers,
        method,
        peer,
        name,
        &tail,
        query,
        (!body.is_empty()).then_some(body),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn forward_profile_request(
    state: &AppState,
    headers: &HeaderMap,
    method: reqwest::Method,
    peer: &str,
    name: &str,
    tail: &[&str],
    query: Option<String>,
    body: Option<Bytes>,
) -> Response {
    if let Err(e) = state.check_auth(headers) {
        return e.into_response();
    }
    let Some(peer_url) = get_peer_url(state, peer).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Peer '{}' not found in mesh", peer) })),
        )
            .into_response();
    };
    let mut url = peer_profile_url(&peer_url, name, None);
    if let Ok(mut parsed) = url::Url::parse(&url) {
        if let Ok(mut segments) = parsed.path_segments_mut() {
            segments.extend(tail);
        }
        url = parsed.to_string();
    }
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(&q);
    }
    let mut req = state.peer_request(state.http_client.request(method, &url));
    if let Some(body) = body {
        req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    proxy_response(req.send().await).await
}

async fn api_peer_export_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
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
    let url = peer_profile_url(&peer_url, &name, Some("export"));
    let res = state.peer_request(state.http_client.get(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_import_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    Query(params): Query<ImportProfileParams>,
    body: Bytes,
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
    let mut url = peer_profile_url(&peer_url, &name, Some("import"));
    if let Some(true) = params.overwrite {
        url.push_str("?overwrite=true");
    }
    let res = state
        .peer_request(state.http_client.post(&url))
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(body)
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_transfer_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((peer, name)): AxumPath<(String, String)>,
    Json(req): Json<TransferProfileRequest>,
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
    let url = peer_profile_url(&peer_url, &name, Some("transfer"));
    let res = state
        .peer_request(state.http_client.post(&url))
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
    if !frames_path.exists() {
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
        return Ok(Json(
            serde_json::json!({ "status": "already_recording", "session_id": id }),
        ));
    }

    let rec_id = state.start_recording(&session).await;
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
    session.activity.set_timeout_secs(params.minutes * 60);
    Ok(Json(serde_json::json!({
        "session_id": id,
        "idle_timeout_seconds": params.minutes * 60
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

/// Chromium's own DevTools frontend for a session (bundled with the browser, served
/// on its debugging port), for the live view's DevTools button. It connects to a page
/// through /devtools/page/{id} like the view does.
async fn devtools_frontend(
    AxumPath((session_id, path)): AxumPath<(Uuid, String)>,
    State(state): State<AppState>,
) -> Response {
    let port = state
        .sessions
        .read()
        .await
        .get(&session_id)
        .filter(|s| s.engine == BrowserEngine::Chromium)
        .map(|s| s.port);
    let Some(port) = port else {
        return (StatusCode::NOT_FOUND, "No Chromium session with this id").into_response();
    };
    if path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let res = match state
        .http_client
        .get(format!("http://127.0.0.1:{port}/devtools/{path}"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
    let body = match res.bytes().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let mut resp = (status, body).into_response();
    if let Some(ct) = content_type {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    resp
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
    if !req.find {
        info!(session_id = %id, rp_id = %req.rp_id, "Passkey approved in the live view");
    }
    let kind = match (req.find, &req.challenge) {
        (true, _) => "find",
        (false, Some(_)) => "assert",
        (false, None) => "inject",
    };
    // The origin to sign for comes from the browser's own page list, never from
    // the page: a page must not obtain an assertion for another site.
    let origin = if kind == "assert" {
        let targets = get_local_targets(&state).await;
        let page = targets
            .iter()
            .find(|t| t.id == req.page_id && t.session_id == Some(id))
            .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found in this session"))?;
        let url = url::Url::parse(&page.url)
            .map_err(|_| api_error(StatusCode::BAD_REQUEST, "The page has no web origin"))?;
        Some(url.origin().ascii_serialization())
    } else {
        None
    };
    let signing = passkeys::Signing {
        origin,
        challenge: req.challenge,
        allow: req.allow,
    };
    let request = state
        .passkeys
        .start(req.page_id, req.rp_id, kind, req.entry, signing)
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(serde_json::json!({ "request": request })))
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
            .next(Duration::from_secs(25), poll.finds)
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
    body: Bytes,
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
    fs::write(&path, &body).await.map_err(io)?;
    info!(session_id = %id, path = %path.display(), bytes = body.len(), "Stored dropped file");
    Ok(Json(serde_json::json!({
        "path": path,
        "size_bytes": body.len(),
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
    // The shared client's 3 s timeout covers the whole body; a large recording
    // needs far longer, so stream it through with a generous per-request limit.
    let mut req = state
        .peer_request(state.http_client.get(&url))
        .timeout(Duration::from_secs(3600));
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range.clone());
    }
    match req.send().await {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
            ] {
                if let Some(v) = r.headers().get(name.as_str()) {
                    if let Ok(v) = HeaderValue::from_bytes(v.as_bytes()) {
                        builder = builder.header(name, v);
                    }
                }
            }
            builder
                .body(axum::body::Body::from_stream(r.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
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
    let res = state
        .peer_request(state.http_client.delete(&url))
        .send()
        .await;
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
    let res = state
        .peer_request(state.http_client.post(&url))
        .send()
        .await;
    proxy_response(res).await
}

async fn api_peer_json_new(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
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
    let url = match raw_query {
        Some(q) if !q.is_empty() => format!("{}/json/new?{}", peer_url, q),
        _ => format!("{}/json/new", peer_url),
    };
    let res = state.peer_request(state.http_client.put(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_json_close(
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
    let url = format!("{}/json/close/{}", peer_url, id);
    let res = state.peer_request(state.http_client.put(&url)).send().await;
    proxy_response(res).await
}

async fn api_peer_json_activate(
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
    let url = format!("{}/json/activate/{}", peer_url, id);
    let res = state.peer_request(state.http_client.put(&url)).send().await;
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

/// `{peer}/api/profiles/{name}[/{action}]`, the name encoded as one path segment.
fn peer_profile_url(peer_url: &str, name: &str, action: Option<&str>) -> String {
    let mut url = match url::Url::parse(peer_url) {
        Ok(url) => url,
        Err(_) => {
            let action = action.map(|a| format!("/{a}")).unwrap_or_default();
            return format!("{}/api/profiles/{}{}", peer_url, name, action);
        }
    };
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.pop_if_empty().extend(["api", "profiles", name]);
        if let Some(action) = action {
            segments.push(action);
        }
    }
    url.to_string()
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
    fn tab_order_is_first_seen_not_most_recent() {
        let tab = |id: &str| DevToolsTarget {
            id: id.into(),
            title: None,
            target_type: "page".into(),
            url: String::new(),
            devtools_frontend_url: None,
            web_socket_debugger_url: None,
            session_id: None,
            profile: None,
            engine: None,
            audible: false,
        };
        let mut order = TabOrder::default();
        let mut list = vec![tab("a"), tab("b"), tab("c")];
        order.sort(&mut list);
        // Chromium reports "c" first once it is brought to the front.
        let mut list = vec![tab("c"), tab("a"), tab("b")];
        order.sort(&mut list);
        assert_eq!(
            list.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Closed tabs are forgotten; new ones go last.
        order.rank("d");
        let mut list = vec![tab("d"), tab("c"), tab("a")];
        order.sort(&mut list);
        assert_eq!(
            list.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "c", "d"]
        );
        assert!(!order.seen.contains_key("b"));
    }

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
    fn peer_profile_urls_encode_the_name() {
        assert_eq!(
            peer_profile_url(
                "https://browser.h.ts.example/",
                "Google (p@x.dev)",
                Some("import")
            ),
            "https://browser.h.ts.example/api/profiles/Google%20(p@x.dev)/import"
        );
        assert_eq!(
            peer_profile_url("https://browser.h.ts.example", "a b", None),
            "https://browser.h.ts.example/api/profiles/a%20b"
        );
    }
}
