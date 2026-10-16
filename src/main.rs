mod audible;
mod audio;
mod auth;
mod autofill;
mod backups;
mod bidi;
mod browser;
mod clients;
mod ctl;
mod display;
mod favicon;
mod history;
mod idle;
mod mcp;
mod names;
mod page_input;
mod passkey_helper;
mod passkeys;
mod persist;
mod proc;
mod profiles;
mod proxies;
mod proxy;
mod rec_audio;
mod rec_export;
mod recorder;
mod relay;
mod secrets;
mod settings;
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

use crate::bidi::BidiHub;
use crate::browser::{ActiveBrowser, BrowserConfig, BrowserEngine, SessionState};
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
    /// Whether a recording started without saying takes the sound (the
    /// profile's setting, else the node's).
    pub record_audio: bool,
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
            hub.call_timeout(
                "browsingContext.getTree",
                serde_json::json!({ "maxDepth": 0 }),
                Duration::from_secs(3),
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

        self.summary(tabs_count, recording)
    }

    /// The summary, given how many tabs the session has.
    pub fn summary(
        &self,
        tabs_count: usize,
        recording: Option<&SessionRecorderHandle>,
    ) -> SessionSummary {
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
            recording_audio: recording.is_some_and(|r| r.audio),
            record_audio: self.record_audio,
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
    /// The recording takes the session's sound too.
    #[serde(default)]
    pub recording_audio: bool,
    /// A recording started without saying would take the sound.
    #[serde(default)]
    pub record_audio: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<BrowserConfig>,
    pub max_sessions: usize,
    pub sessions: Arc<RwLock<HashMap<Uuid, ManagedSession>>>,
    /// For this node's own browsers and the overview's polls: gives up after 3 s.
    pub http_client: reqwest::Client,
    /// For requests to peers that change something or carry a body: no limit of
    /// its own, each request sets the one it needs.
    pub peer_client: reqwest::Client,
    pub node_name: String,
    pub public_url: String,
    pub peers: Arc<RwLock<HashMap<String, String>>>,
    pub start_time: Instant,
    pub cluster_secret: Option<String>,
    /// Who may talk to the daemon (see auth.rs).
    pub auth: Arc<auth::Auth>,
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
    /// Pages visited per profile and session, for the live view's address bar.
    pub history: Arc<history::History>,
    /// Small screenshots of the sessions' front tabs, for the dashboard's cards.
    pub thumbnails: Arc<thumbnail::Thumbnails>,
    /// Sessions being started: they count against the cap and hold their profile.
    pub starting: Arc<Starting>,
    /// One lock per profile, held while a profile is checked and then changed or
    /// removed, and while a session of it is being admitted.
    pub profile_locks: Arc<ProfileLocks>,
    /// Profile directory sizes, which take a walk of the disk to find out.
    pub dir_sizes: Arc<DirSizes>,
    /// The mesh overview, computed once for everybody who wants it.
    pub overview: Arc<OverviewHub>,
}

/// How long a profile directory's size is taken as it was.
const DIR_SIZE_TTL: Duration = Duration::from_secs(60);

/// Sizes of directories, remembered for a while: a profile can be gigabytes and
/// every dashboard asks every two seconds.
#[derive(Default)]
pub struct DirSizes(std::sync::Mutex<HashMap<PathBuf, (Instant, u64)>>);

impl DirSizes {
    fn fresh(&self, dir: &Path, now: Instant) -> Option<u64> {
        let map = self.0.lock().unwrap();
        let (at, size) = map.get(dir)?;
        (now.saturating_duration_since(*at) < DIR_SIZE_TTL).then_some(*size)
    }

    fn store(&self, dir: &Path, now: Instant, size: u64) {
        let mut map = self.0.lock().unwrap();
        // Directories that are gone for good (deleted profiles) do not pile up.
        map.retain(|_, (at, _)| now.saturating_duration_since(*at) < DIR_SIZE_TTL * 10);
        map.insert(dir.to_path_buf(), (now, size));
    }

    /// Its contents changed (cleared, restored, removed): measure again next time.
    pub fn forget(&self, dir: &Path) {
        self.0.lock().unwrap().remove(dir);
    }

    pub async fn of(&self, dir: &Path) -> u64 {
        if let Some(size) = self.fresh(dir, Instant::now()) {
            return size;
        }
        let size = compute_dir_size(dir.to_path_buf()).await;
        self.store(dir, Instant::now(), size);
        size
    }
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

/// Sessions that are launching but not in the map yet. Whoever wants to know
/// whether a profile or a slot is taken has to count these too.
#[derive(Default)]
pub struct Starting {
    inner: std::sync::Mutex<StartingInner>,
    done: tokio::sync::Notify,
}

#[derive(Default)]
struct StartingInner {
    count: usize,
    profiles: std::collections::HashSet<Uuid>,
}

impl Starting {
    /// A session of the profile is launching.
    pub fn has_profile(&self, id: Uuid) -> bool {
        self.inner.lock().unwrap().profiles.contains(&id)
    }
}

/// A launching session's claim on a slot (and its profile); released on drop,
/// which must come after the session is in the map.
pub struct Reservation {
    starting: Arc<Starting>,
    profile: Option<Uuid>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        {
            let mut inner = self.starting.inner.lock().unwrap();
            inner.count = inner.count.saturating_sub(1);
            if let Some(id) = self.profile {
                inner.profiles.remove(&id);
            }
        }
        self.starting.done.notify_waiters();
    }
}

/// What asking for a slot came to.
enum Slot {
    /// The profile has a session already.
    Attach(Box<ManagedSession>),
    Reserved(Reservation),
}

/// Claim a slot for a launch, or find the session the profile already has.
/// A profile another request is launching right now is waited for, then
/// attached to.
async fn reserve_slot(
    sessions: &RwLock<HashMap<Uuid, ManagedSession>>,
    starting: &Arc<Starting>,
    profile_locks: &ProfileLocks,
    max_sessions: usize,
    id: Option<Uuid>,
) -> Result<Slot, (StatusCode, &'static str)> {
    // Deleting or clearing a profile holds this while it checks for sessions.
    let _profile_guard = match id {
        Some(id) => Some(profile_locks.lock(id).await),
        None => None,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let done = starting.done.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        {
            // Sessions are inserted under the write lock before their reservation
            // goes: with the read lock held, "no session, no reservation" is final.
            let sessions = sessions.read().await;
            let mut inner = starting.inner.lock().unwrap();
            if let Some(id) = id {
                if let Some(session) = sessions.values().find(|s| s.profile_id == Some(id)) {
                    return Ok(Slot::Attach(Box::new(session.clone())));
                }
            }
            if !id.is_some_and(|id| inner.profiles.contains(&id)) {
                if sessions.len() + inner.count >= max_sessions {
                    warn!(
                        active = sessions.len(),
                        starting = inner.count,
                        max = max_sessions,
                        "Session limit reached"
                    );
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Session limit reached. Try again later.",
                    ));
                }
                inner.count += 1;
                if let Some(id) = id {
                    inner.profiles.insert(id);
                }
                return Ok(Slot::Reserved(Reservation {
                    starting: starting.clone(),
                    profile: id,
                }));
            }
        }
        if tokio::time::timeout_at(deadline, done).await.is_err() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "The profile is still starting. Try again.",
            ));
        }
    }
}

#[derive(Default)]
pub struct ProfileLocks(std::sync::Mutex<HashMap<Uuid, Arc<Mutex<()>>>>);

impl ProfileLocks {
    pub async fn lock(&self, id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.0.lock().unwrap().entry(id).or_default().clone();
        lock.lock_owned().await
    }
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

    /// Accepts the cluster secret or a token derived from it (header, cookie or
    /// `query_token`); with no cluster secret everything passes.
    pub fn check_auth_opt(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        if self
            .auth
            .credential(headers, query_token, auth::now())
            .is_none()
        {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "Unauthorized: invalid or missing credentials"
                })),
            ));
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
        default_tabs: bool,
        open: Vec<String>,
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
        let (profile_record, profile_record_audio) = profile
            .as_ref()
            .map_or((None, None), |p| (p.meta.record, p.meta.record_audio));

        // A profile runs in at most one browser: attach to its session if it has one.
        // Otherwise a slot is reserved (also against another request launching the same
        // profile at the same time) before the slow launch.
        let reservation = match self.reserve_slot(profile.as_ref()).await? {
            Slot::Attach(session) => {
                info!(session_id = %session.id, engine = %engine, profile = ?session.profile, "Attaching to existing profile session");
                session.activity.touch();
                return Ok(*session);
            }
            Slot::Reserved(r) => r,
        };

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
            default_tabs,
            &open,
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
            record_audio: profile_record_audio.unwrap_or(self.recorder_config.audio),
            created_at: browser.created_at,
            client_count: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(idle::Activity::new(
                idle_timeout_minutes.map_or(self.idle_timeout_secs, idle::minutes_to_secs),
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
        // In the map: the reservation is no longer needed.
        drop(reservation);

        // The request's choice, then the profile's, then the node's.
        if record
            .or(profile_record)
            .unwrap_or(self.recorder_config.enabled)
        {
            self.start_recording(&session, None).await;
        }
        self.persist_session(&session).await;

        Ok(session)
    }

    /// Claim a slot for a launch, or find the session the profile already has.
    async fn reserve_slot(
        &self,
        profile: Option<&Profile>,
    ) -> Result<Slot, (StatusCode, &'static str)> {
        reserve_slot(
            &self.sessions,
            &self.starting,
            &self.profile_locks,
            self.max_sessions,
            profile.map(|p| p.meta.id),
        )
        .await
    }

    /// Save what a restarted daemon needs to adopt the session's browser.
    pub async fn persist_session(&self, session: &ManagedSession) {
        let (recording, recording_audio) = self
            .active_recordings
            .read()
            .await
            .get(&session.id)
            .map_or((false, false), |r| (true, r.audio));
        let guard = session.browser.lock().await;
        let Some(browser) = guard.as_ref() else {
            return;
        };
        let record = browser.record(SessionState {
            name: session.name.read().unwrap().clone(),
            persistent: session.persistent,
            color_scheme: session.color_scheme,
            screen: *session.screen.read().unwrap(),
            show_touches: *session.show_touches.read().unwrap(),
            idle_timeout_secs: session.activity.timeout_secs(),
            shared_display: session
                .display_size
                .is_none()
                .then(|| session.display.clone())
                .flatten(),
            video: session.video,
            recording,
            recording_audio,
        });
        if let Err(e) = persist::save(self.config.state_dir(), &record).await {
            warn!(session_id = %session.id, error = %e, "Could not save the session record");
        }
    }

    /// Take over the sessions an earlier run left running (see `proc`).
    pub async fn adopt_sessions(&self) {
        for rec in persist::load_all(self.config.state_dir()).await {
            match ActiveBrowser::adopt(&self.config, &rec).await {
                Ok(browser) => {
                    let session = ManagedSession {
                        id: browser.id,
                        name: Arc::new(std::sync::RwLock::new(rec.name.clone())),
                        clients: Arc::default(),
                        engine: browser.engine,
                        port: browser.port,
                        ws_path: browser.ws_path.clone(),
                        profile: browser.profile_name.clone(),
                        profile_id: browser.profile_id,
                        persistent: rec.persistent,
                        record_audio: self.profile_record_audio(browser.profile_id).await,
                        created_at: browser.created_at,
                        client_count: Arc::new(AtomicUsize::new(0)),
                        activity: Arc::new(idle::Activity::new(rec.idle_timeout_secs)),
                        adblock: browser.adblock,
                        relay: browser.relay.clone(),
                        audio_sink: browser.audio.as_ref().map(|s| s.name.clone()),
                        display: browser
                            .display
                            .as_ref()
                            .map(|d| d.name.clone())
                            .or_else(|| rec.shared_display.clone()),
                        display_size: browser.display.as_ref().map(|d| (d.width, d.height)),
                        window_size: browser.window_size,
                        video: rec.video,
                        color_scheme: rec.color_scheme,
                        screen: Arc::new(std::sync::RwLock::new(rec.screen)),
                        show_touches: Arc::new(std::sync::RwLock::new(rec.show_touches)),
                        audible: Arc::new(AtomicBool::new(false)),
                        bidi: browser.bidi.clone(),
                        browser: Arc::new(Mutex::new(Some(browser))),
                    };
                    self.sessions
                        .write()
                        .await
                        .insert(session.id, session.clone());
                    if rec.recording {
                        self.start_recording(&session, rec.recording_audio).await;
                    }
                    info!(session_id = %rec.id, engine = %rec.engine, profile = ?rec.profile, "Adopted session of the previous run");
                }
                Err(e) => {
                    warn!(session_id = %rec.id, error = %e, "Could not adopt session of the previous run; cleaning it up");
                    self.discard_record(&rec).await;
                }
            }
        }
    }

    /// Stop what a session record stands for, as far as it still exists.
    async fn discard_record(&self, rec: &persist::SessionRecord) {
        if let Some(mut p) = proc::Proc::adopt(rec.pid, rec.pid_start) {
            p.terminate(Duration::from_secs(3)).await;
        }
        if let Some(ref d) = rec.display {
            if let Some(mut vd) =
                display::VirtualDisplay::adopt(&d.name, d.width, d.height, d.pid, d.pid_start)
            {
                vd.stop().await;
            }
        }
        if let (Some(cfg), Some(a)) = (&self.config.audio, &rec.audio) {
            // Dropped: unloads the sink.
            drop(audio::Sink::adopt(cfg, a.sink.clone(), a.module));
        }
        if let Some(ref dir) = rec.temp_dir {
            let _ = fs::remove_dir_all(dir).await;
        }
        if let Some(ref log) = rec.firefox_log {
            let _ = fs::remove_file(log).await;
        }
        persist::remove(self.config.state_dir(), rec.id).await;
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

    /// A profile's sound setting for recordings, else the node's.
    async fn profile_record_audio(&self, profile: Option<Uuid>) -> bool {
        let meta = match profile {
            Some(id) => profiles::find(&self.config.profiles_dir, &id.to_string()).await,
            None => None,
        };
        meta.and_then(|p| p.meta.record_audio)
            .unwrap_or(self.recorder_config.audio)
    }

    /// `audio`: take the session's sound too (default: the session's, see
    /// `ManagedSession::record_audio`). A session records once: when it already does,
    /// that recording's id comes back with `false`. `None`: the session is gone.
    pub async fn start_recording(
        &self,
        session: &ManagedSession,
        audio: Option<bool>,
    ) -> Option<(Uuid, bool)> {
        // Check and insert under one lock, or two requests at once would leave a
        // recorder nothing can stop.
        let mut recordings = self.active_recordings.write().await;
        if let Some(running) = recordings.get(&session.id) {
            return Some((running.recording_id, false));
        }
        // A session that was terminated meanwhile must not get a recorder.
        if !self.sessions.read().await.contains_key(&session.id) {
            return None;
        }
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
            audio
                .unwrap_or(session.record_audio)
                .then(|| {
                    Some(rec_audio::Source {
                        cfg: self.config.audio.clone()?,
                        sink: session.audio_sink.clone()?,
                    })
                })
                .flatten(),
        );
        // Firefox has no screencast: feed the recorder with BiDi screenshots instead.
        if let Some(ref hub) = session.bidi {
            spawn_bidi_frame_feeder(hub.clone(), handle.clone());
        }
        let rec_id = handle.recording_id;
        recordings.insert(session.id, handle);
        Some((rec_id, true))
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
        // Live views of the session end with it.
        self.viewers.close_session(*id);
        let mut terminated = false;
        if let Some(session) = removed {
            // Out of the session before the slow part (a profile's browser gets up to
            // 27 s to quit): the periodic save of every other session would wait for
            // the lock otherwise, and finds nothing to save here.
            let browser = session.browser.lock().await.take();
            if let Some(mut b) = browser {
                b.terminate().await;
                info!(session_id = %id, "Session terminated");
                terminated = true;
            }
        }
        // After the browser is gone and out of the session: a periodic save can no
        // longer write the record again.
        persist::remove(self.config.state_dir(), *id).await;
        self.history
            .forget(&history_key(None, *id));
        terminated
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
    /// Open the profile's default tabs (the default); false: a blank tab.
    pub default_tabs: Option<bool>,
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
    /// Open the profile's default tabs (the default); false: a blank tab.
    pub default_tabs: Option<bool>,
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
            // In a realm of its own: the page cannot fake what it reads.
            let info = hub
                .call_timeout(
                    "script.evaluate",
                    serde_json::json!({
                        "expression": "JSON.stringify([location.href, document.title])",
                        "target": { "context": ctx, "sandbox": "browser-daemon" },
                        "awaitPromise": false
                    }),
                    Duration::from_secs(5),
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
    /// Record the profile's sessions; the node's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    /// Their recordings take the sound; the node's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_audio: Option<bool>,
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
    /// Pages its sessions open with.
    #[serde(default)]
    pub tabs: Vec<String>,
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
    /// Pages its sessions open with: a list, or addresses separated by spaces.
    #[serde(default, deserialize_with = "tabs_field")]
    pub tabs: Option<Vec<String>>,
    /// Record its sessions (the node's default when absent or null).
    #[serde(default)]
    pub record: Option<bool>,
    /// Its recordings take the sound (the node's default when absent or null).
    #[serde(default)]
    pub record_audio: Option<bool>,
}

/// Default tabs as a JSON list, or as one string of addresses separated by
/// whitespace or commas (what a prompt or `curl` gives).
fn tabs_field<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Tabs {
        List(Vec<String>),
        Text(String),
    }
    Ok(Option::<Tabs>::deserialize(d)?.map(|t| match t {
        Tabs::List(list) => list,
        Tabs::Text(text) => text
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect(),
    }))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateProfileRequest {
    pub name: Option<String>,
    /// Proxy id new sessions use; "none" (or "") clears it.
    pub proxy: Option<String>,
    /// Appearance of new sessions; a running one keeps its own until restarted.
    pub color_scheme: Option<ColorScheme>,
    /// Pages new sessions open with; empty clears them.
    #[serde(default, deserialize_with = "tabs_field")]
    pub tabs: Option<Vec<String>>,
    /// Record new sessions (true/false), or null for the node's default.
    #[serde(default, deserialize_with = "nullable")]
    pub record: Option<Option<bool>>,
    /// Recordings take the sound (true/false), or null for the node's default.
    #[serde(default, deserialize_with = "nullable")]
    pub record_audio: Option<Option<bool>>,
}

/// A field that may be set to null (Some(None)) as well as left out (None).
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
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

/// Report a failed client command on one line and exit non-zero.
fn ctl_exit(result: Result<(), Box<dyn std::error::Error>>) -> Result<(), Box<dyn std::error::Error>> {
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
                    out.push(Suggestion { url: tab, title: None, kind: "tab" });
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
            None => out.push(Suggestion { url: e.url, title: e.title, kind: "history" }),
        }
    }
    out.truncate(limit);
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SaveProfileRequest {
    name: String,
}

/// Keep what a throwaway session has gathered (logins, cookies, storage) as a new
/// profile. The browser is quit properly so its data is on disk, the data copied,
/// and a session of the new profile opens the same pages in its place.
async fn api_session_save_as_profile(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SaveProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.name)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    if let Some(ref p) = session.profile {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("The session already uses the profile '{p}'"),
        ));
    }
    ensure_name_free(&state, &name, None).await?;

    let urls: Vec<String> = session_targets(&state, &session)
        .await
        .into_iter()
        .filter(|t| {
            t.target_type == "page"
                && (t.url.starts_with("http://") || t.url.starts_with("https://"))
        })
        .map(|t| t.url)
        .collect();
    let recording = state.active_recordings.read().await.contains_key(&id);

    // The profile first: when it cannot be made, the browser is left alone.
    let profile = {
        let _guard = state.profiles_lock.lock().await;
        ensure_name_free(&state, &name, None).await?;
        profiles::create(&state.config.profiles_dir, &name, session.engine)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let meta = match keep_session_data(&session, &profile).await {
        Ok(meta) => meta,
        Err((err, quit)) => {
            let _ = fs::remove_dir_all(&profile.dir).await;
            // The browser has quit: the session is a shell without a browser.
            if quit {
                state.terminate_session(&id).await;
            }
            return Err(err);
        }
    };

    // The throwaway ends (its directory goes with it); the profile takes over.
    let (width, height) = session.window_size;
    let adblock = session.adblock;
    let color_scheme = session.color_scheme;
    state.terminate_session(&id).await;
    let new = state
        .spawn_session(
            session.engine,
            Some(&name),
            true,
            Some(width),
            Some(height),
            Some(recording),
            None,
            None,
            Some(adblock),
            None,
            Some(color_scheme),
            true,
            false,
            urls,
        )
        .await
        .map_err(|(status, msg)| api_error(status, msg))?;
    info!(profile = %name, from = %id, session_id = %new.id, "Session saved as a profile");
    let rec = state.active_recordings.read().await.get(&new.id).cloned();
    let summary = new.to_summary(&state.http_client, rec.as_ref()).await;
    Ok(Json(serde_json::json!({
        "profile": profile_info(&state, &meta_profile(&profile, meta)).await,
        "session": summary,
    })))
}

/// Quit a throwaway session's browser and copy its data into the profile. On
/// failure: the error, and whether the browser has quit by then.
async fn keep_session_data(
    session: &ManagedSession,
    profile: &Profile,
) -> Result<profiles::ProfileMeta, (ApiError, bool)> {
    // Quit the browser (data to disk), leaving its directory in place.
    let src = {
        let mut guard = session.browser.lock().await;
        let browser = guard.as_mut().ok_or_else(|| {
            (
                api_error(StatusCode::CONFLICT, "The session is not running"),
                false,
            )
        })?;
        let src = browser.temp_dir_path().cloned().ok_or_else(|| {
            (
                api_error(StatusCode::CONFLICT, "The session has no data to keep"),
                false,
            )
        })?;
        browser.quiesce().await;
        src
    };
    let failed = |msg: String| (api_error(StatusCode::INTERNAL_SERVER_ERROR, msg), true);

    let copied = tokio::process::Command::new("cp")
        .arg("-a")
        .arg("--")
        .arg(src.join("."))
        .arg(&profile.dir)
        .status()
        .await;
    if !matches!(copied, Ok(s) if s.success()) {
        return Err(failed("Could not copy the session's data".into()));
    }
    // What belongs to the run, not to the data.
    for leftover in [
        "SingletonLock",
        "SingletonSocket",
        "SingletonCookie",
        "DevToolsActivePort",
        "lock",
        ".parentlock",
        "parent.lock",
    ] {
        let _ = fs::remove_file(profile.dir.join(leftover)).await;
    }
    for cache in [
        "Default/Cache",
        "Default/Code Cache",
        "Default/GPUCache",
        "GrShaderCache",
        "ShaderCache",
        "component_crx_cache",
        "extensions_crx_cache",
        "cache2",
    ] {
        let _ = fs::remove_dir_all(profile.dir.join(cache)).await;
    }
    // The profile remembers the proxy and appearance the session had.
    let mut meta = profile.meta.clone();
    meta.proxy = session.relay.current();
    meta.color_scheme = Some(session.color_scheme);
    profiles::write_meta(&profile.dir, &meta)
        .await
        .map_err(|e| failed(e.to_string()))?;
    Ok(meta)
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
    Profile { meta, dir: profile.dir.clone() }
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
            info!(count = removed, "Removed temporary profiles and logs no session claimed");
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
        .route("/api/pages/{page_id}/insert-text", post(api_page_insert_text))
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
        .route("/api/recordings/{id}/favicon", get(api_get_recording_favicon))
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

/// The tabs one session shows, as a DevTools target list.
async fn session_targets(state: &AppState, session: &ManagedSession) -> Vec<DevToolsTarget> {
    if let Some(ref hub) = session.bidi {
        return hub
            .contexts()
            .await
            .into_iter()
            .map(|ctx| firefox_target(session, ctx))
            .collect();
    }
    let url = format!("http://127.0.0.1:{}/json/list", session.port);
    let Ok(res) = state.http_client.get(&url).send().await else {
        return Vec::new();
    };
    let mut targets = res.json::<Vec<DevToolsTarget>>().await.unwrap_or_default();
    for target in &mut targets {
        target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
        target.session_id = Some(session.id);
        target.profile = session.profile.clone();
        target.engine = Some(BrowserEngine::Chromium);
    }
    targets
}

async fn snapshot_sessions(state: &AppState) -> Vec<ManagedSession> {
    state.sessions.read().await.values().cloned().collect()
}

async fn get_local_targets(state: &AppState) -> Vec<DevToolsTarget> {
    let sessions = snapshot_sessions(state).await;
    // All sessions at once: a hung one costs one wait, not one per session.
    let mut all_targets: Vec<DevToolsTarget> = futures_util::future::join_all(
        sessions
            .iter()
            .map(|session| session_targets(state, session)),
    )
    .await
    .into_iter()
    .flatten()
    .collect();

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

/// The session showing a tab, and the tab.
async fn find_page(state: &AppState, page_id: &str) -> Option<(ManagedSession, DevToolsTarget)> {
    let sessions = snapshot_sessions(state).await;
    let found = futures_util::future::join_all(sessions.into_iter().map(|session| async move {
        let target = session_targets(state, &session)
            .await
            .into_iter()
            .find(|t| t.id == page_id)?;
        Some((session, target))
    }))
    .await;
    found.into_iter().flatten().next()
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

/// A page that logs the browser in: the cookie comes with it, never the secret.
fn login_page(state: &AppState, headers: &HeaderMap, html: &'static str) -> Response {
    let mut res = (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Html(html),
    )
        .into_response();
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .next()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
        });
    if let Some(cookie) = state
        .auth
        .cookie(secure, auth::now())
        .and_then(|c| HeaderValue::from_str(&c).ok())
    {
        res.headers_mut().append(header::SET_COOKIE, cookie);
    }
    res
}

async fn dashboard_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    login_page(&state, &headers, DASHBOARD_HTML)
}

async fn handoff_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    login_page(&state, &headers, HANDOFF_HTML)
}

/// A token for tools that cannot send the cookie or a header (`?auth=` on a
/// CDP endpoint). `null` when the daemon has no cluster secret.
async fn api_auth_token(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    state.check_auth(&headers)?;
    let issued = state.auth.issue(auth::now());
    Ok::<_, ApiError>((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(serde_json::json!({
            "token": issued.as_ref().map(|(t, _)| t),
            "expires_at": issued.as_ref().map(|(_, e)| e),
        })),
    ))
}

/// The error reply of the DevTools-style routes.
fn json_err(status: StatusCode, msg: impl ToString) -> ApiError {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// Opens a tab. It belongs to the session named by `session_id`, or the one
/// showing the tab `page`; neither given, a session of its own is started. Never
/// another session's: that could be somebody's logged-in profile.
async fn json_new(
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
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

    let session = if let Some(sid) = session_id {
        state
            .sessions
            .read()
            .await
            .get(&sid)
            .cloned()
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "Session not found"))?
    } else if let Some(ref pid) = page {
        find_page(&state, pid)
            .await
            .map(|(session, _)| session)
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "Page not found"))?
    } else {
        state
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
                true,
                Vec::new(),
            )
            .await
            .map_err(|(st, err)| json_err(st, err))?
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
        let ctx = hub
            .create_tab(&target_url)
            .await
            .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;
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
            let pages: Vec<_> = session_targets(&state, &session)
                .await
                .into_iter()
                .filter(|t| t.target_type == "page")
                .collect();
            match pages.as_slice() {
                [only] if only.url == "about:blank" => Some(only.id.clone()),
                _ => None,
            }
        } else {
            None
        };

    let mut dest_url = url::Url::parse(&format!("http://127.0.0.1:{}/json/new", session.port))
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    dest_url.set_query(Some(&target_url));

    let res = state
        .http_client
        .put(dest_url)
        .send()
        .await
        .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;

    let mut target = res
        .json::<DevToolsTarget>()
        .await
        .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;

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
    let Some((session, _)) = find_page(&state, &id).await else {
        return (StatusCode::NOT_FOUND, "Target not found").into_response();
    };
    if let Some(ref hub) = session.bidi {
        return match hub.close_tab(&id).await {
            Ok(()) => (StatusCode::OK, "Target is closing").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        };
    }
    let req_url = format!("http://127.0.0.1:{}/json/close/{}", session.port, id);
    match state.http_client.put(&req_url).send().await {
        Ok(res) if res.status().is_success() => {
            (StatusCode::OK, "Target is closing").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Target not found").into_response(),
    }
}

async fn json_activate(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some((session, _)) = find_page(&state, &id).await else {
        return (StatusCode::NOT_FOUND, "Target not found").into_response();
    };
    if let Some(ref hub) = session.bidi {
        return match hub.activate_tab(&id).await {
            Ok(()) => (StatusCode::OK, "Target activated").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        };
    }
    let req_url = format!("http://127.0.0.1:{}/json/activate/{}", session.port, id);
    match state.http_client.put(&req_url).send().await {
        Ok(res) if res.status().is_success() => {
            (StatusCode::OK, "Target activated").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Target not found").into_response(),
    }
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

/// An automation client's place on a session. Taken before the WebSocket upgrade
/// and given back however it ends, so a client that vanishes during the upgrade
/// (or a failed upgrade) cannot leave a non-persistent session running unused.
struct ClientSlot {
    state: AppState,
    session: ManagedSession,
    /// Counts as a client (a live view's observer does not).
    counted: bool,
    client: Option<u64>,
}

impl ClientSlot {
    fn new(state: &AppState, session: &ManagedSession, counted: bool) -> Self {
        if counted {
            session.client_count.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            state: state.clone(),
            session: session.clone(),
            counted,
            client: None,
        }
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.session.clients.remove(client);
        }
        if !self.counted {
            return;
        }
        let remaining = self.session.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if !self.session.persistent && remaining == 0 {
            let state = self.state.clone();
            let id = self.session.id;
            tokio::spawn(async move {
                info!(session_id = %id, "Auto-terminating non-persistent session");
                state.terminate_session(&id).await;
            });
        }
    }
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
                // Playwright ignores tabs it did not open: the default tabs would just
                // sit there. It can still ask (`default_tabs=true`).
                params.default_tabs.unwrap_or(!agent_opens_own_tab),
                Vec::new(),
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    let mut slot = ClientSlot::new(&state, &session, true);
    ws.on_upgrade(move |socket| async move {
        slot.client = Some(client_arrived(&state, &session, &headers, &params).await);
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
                bridge_websockets(
                    socket,
                    server_ws,
                    frame_tx,
                    activity,
                    None,
                    None,
                    true,
                    session.profile_id.is_some(),
                )
                .await;
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Failed to connect to Chromium backend");
            }
        }

        drop(slot);
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
                params.default_tabs.unwrap_or(true),
                Vec::new(),
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

    let mut slot = ClientSlot::new(&state, &session, counted);
    ws.on_upgrade(move |socket| async move {
        if counted {
            slot.client = Some(client_arrived(&state, &session, &headers, &params).await);
        }
        info!(session_id = %session.id, observer = !counted, "Client attached to Firefox WebDriver BiDi");

        session.activity.touch();
        hub.serve_client(socket, session.activity.clone(), !counted)
            .await;
        drop(slot);
    })
}

async fn handle_page_ws(
    AxumPath(page_id): AxumPath<String>,
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
) -> Response {
    let found = find_page(&state, &page_id)
        .await
        .filter(|(session, _)| session.bidi.is_none());
    let Some((session, target)) = found else {
        warn!(page_id = %page_id, "Page not found across active sessions");
        return (StatusCode::NOT_FOUND, "Target page not found").into_response();
    };
    let (port, session_id, activity) = (session.port, session.id, session.activity.clone());
    let (initial_title, initial_url) = (target.title, Some(target.url));

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
                    false,
                    false,
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
    let sessions = snapshot_sessions(state).await;
    let recordings = state.active_recordings.read().await.clone();
    // At once, and with the locks released: one hung tab must not hold the API up.
    futures_util::future::join_all(
        sessions
            .iter()
            .map(|session| session.to_summary(&state.http_client, recordings.get(&session.id))),
    )
    .await
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
        size_bytes: state.dir_sizes.of(&profile.dir).await,
        is_active: session_id.is_some(),
        session_id,
        created_at_secs: Some(profile.meta.created_at_secs),
        modified_at_secs,
        proxy: profile.meta.proxy.clone(),
        color_scheme: profile.meta.color_scheme.unwrap_or_default(),
        tabs: profile.meta.tabs.clone(),
        record: profile.meta.record,
        record_audio: profile.meta.record_audio,
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
    let profiles = profiles::list(&state.config.profiles_dir).await;
    futures_util::future::join_all(profiles.iter().map(|p| profile_info(state, p))).await
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
            req.default_tabs.unwrap_or(true),
            Vec::new(),
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
    let targets = session_targets(&state, &session).await;
    let tabs = targets.iter().filter(|t| t.target_type == "page").count();
    let summary = session.summary(tabs, rec.as_ref());

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

    let tabs = req.tabs.as_deref().map(tab_urls).transpose()?;
    if proxy.is_some()
        || req.color_scheme.is_some()
        || tabs.is_some()
        || req.record.is_some()
        || req.record_audio.is_some()
    {
        if let Some(proxy) = proxy {
            profile.meta.proxy = proxy;
        }
        if req.record.is_some() {
            profile.meta.record = req.record;
        }
        if req.record_audio.is_some() {
            profile.meta.record_audio = req.record_audio;
        }
        if req.color_scheme.is_some() {
            profile.meta.color_scheme = req.color_scheme;
        }
        if let Some(tabs) = tabs {
            profile.meta.tabs = tabs;
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
/// Default tabs as given: each an http(s) address (a bare host gets https://);
/// empty entries are dropped, repeats too.
fn tab_urls(input: &[String]) -> Result<Vec<String>, ApiError> {
    let mut out: Vec<String> = Vec::new();
    for entry in input {
        if let Some(url) = tab_url(entry)? {
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    Ok(out)
}

/// One default tab as given: an http(s) address (a bare host gets https://);
/// empty is none.
fn tab_url(input: &str) -> Result<Option<String>, ApiError> {
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
            "A default tab must be an http(s) address",
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
    if let Some(ref requested) = req.tabs {
        let tabs = tab_urls(requested)?;
        if tabs != profile.meta.tabs {
            profile.meta.tabs = tabs;
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            info!(profile = %profile.meta.name, tabs = ?profile.meta.tabs, "Profile default tabs changed");
        }
    }
    let (record, record_audio) = (
        req.record.unwrap_or(profile.meta.record),
        req.record_audio.unwrap_or(profile.meta.record_audio),
    );
    if (record, record_audio) != (profile.meta.record, profile.meta.record_audio) {
        profile.meta.record = record;
        profile.meta.record_audio = record_audio;
        profiles::write_meta(&profile.dir, &profile.meta)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // A running session keeps recording as it does until it is restarted.
        info!(profile = %profile.meta.name, record = ?record, record_audio = ?record_audio, "Profile recording settings changed");
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
    // Nothing starts a session of the profile between the check and the removal.
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    release_profile(&state, &profile, params.force.unwrap_or(false), "delete").await?;

    state.dir_sizes.forget(&profile.dir);
    if let Err(e) = fs::remove_dir_all(&profile.dir).await {
        error!(profile = %profile.meta.name, error = %e, "Failed to remove profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to remove profile directory",
        ));
    }

    backups::remove_all(&state.config.profiles_dir, profile.meta.id).await;
    state.history.delete(&profile.meta.id.to_string()).await;
    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile removed");
    Ok((
        StatusCode::OK,
        Json(
            serde_json::json!({ "status": "deleted", "profile": profile.meta.name, "id": profile.meta.id }),
        ),
    ))
}

/// What holds a profile.
enum ProfileUse {
    Session(Uuid),
    Starting,
}

async fn profile_use(state: &AppState, id: Uuid) -> Option<ProfileUse> {
    if let Some(session) = profile_session(state, id).await {
        return Some(ProfileUse::Session(session.id));
    }
    state
        .starting
        .has_profile(id)
        .then_some(ProfileUse::Starting)
}

/// Make sure no browser holds the profile: refuse while a session uses it,
/// unless `force` stops that session. The caller holds the profile's lock, so no
/// session is admitted meanwhile.
async fn release_profile(
    state: &AppState,
    profile: &Profile,
    force: bool,
    action: &str,
) -> Result<(), ApiError> {
    let session = match profile_use(state, profile.meta.id).await {
        None => return Ok(()),
        Some(ProfileUse::Starting) => {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("A session of the profile is starting. Try again to {action}."),
            ))
        }
        Some(ProfileUse::Session(id)) => id,
    };
    if !force {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile is currently in use by active session {session}. Pass ?force=true to terminate session and {action}."),
        ));
    }
    info!(profile = %profile.meta.name, session_id = %session, "Force terminating session before {action}");
    state.terminate_session(&session).await;
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
    // A session must not start on the profile while it is archived.
    let _guard = state.profiles_lock.lock().await;
    let profile = lookup_profile(&state, &key).await?;
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    if let Some(used) = profile_use(&state, profile.meta.id).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            match used {
                ProfileUse::Session(id) => {
                    format!("Cannot back up an active profile (in use by session {id})")
                }
                ProfileUse::Starting => "Cannot back up a profile that is starting".into(),
            },
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
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    release_profile(&state, &profile, params.force.unwrap_or(false), "restore").await?;
    auto_backup(&state, &profile, "pre-restore").await?;
    backups::restore(dir, &profile, &backup)
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, backup = %backup, error = %e, "Restore failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
    state.dir_sizes.forget(&profile.dir);
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
    let _lock = state.profile_locks.lock(profile.meta.id).await;
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

    state.dir_sizes.forget(&profile.dir);
    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile data cleared");
    Ok(Json(profile_info(&state, &profile).await))
}

/// What a child process wrote to stderr (a little of it), collected while it runs: a
/// full pipe would stop the child.
fn drain_stderr(stderr: Option<tokio::process::ChildStderr>) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        const KEEP: usize = 8192;
        let Some(mut stderr) = stderr else {
            return String::new();
        };
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = KEEP.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        String::from_utf8_lossy(&kept).trim().to_string()
    })
}

/// The profile directory as a gzipped tar, produced while it is read: the archive
/// never sits in memory. A failing tar ends the stream with an error, so the
/// receiver sees a broken archive rather than a short, valid one.
fn tar_export(
    dir: &Path,
) -> std::io::Result<impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
    let mut child = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .arg(".")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("tar has no output"))?;
    let errors = drain_stderr(child.stderr.take());
    Ok(futures_util::stream::unfold(
        Some((child, stdout, errors)),
        |state| async move {
            use tokio::io::AsyncReadExt;
            let (mut child, mut stdout, errors) = state?;
            let mut buf = vec![0u8; 64 * 1024];
            match stdout.read(&mut buf).await {
                Ok(0) => match child.wait().await {
                    Ok(status) if status.success() => None,
                    other => {
                        let why = errors.await.unwrap_or_default();
                        error!(error = %why, status = ?other, "tar export failed");
                        Some((
                            Err(std::io::Error::other(format!("tar failed: {why}"))),
                            None,
                        ))
                    }
                },
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), Some((child, stdout, errors))))
                }
                Err(e) => Some((Err(e), None)),
            }
        },
    ))
}

/// Why a profile cannot be exported or moved right now.
async fn ensure_profile_idle(
    state: &AppState,
    profile: &Profile,
    what: &str,
) -> Result<(), ApiError> {
    match profile_use(state, profile.meta.id).await {
        None => Ok(()),
        Some(ProfileUse::Session(id)) => Err(api_error(
            StatusCode::CONFLICT,
            format!("Cannot {what} active profile (in use by session {id})"),
        )),
        Some(ProfileUse::Starting) => Err(api_error(
            StatusCode::CONFLICT,
            format!("Cannot {what} a profile that is starting"),
        )),
    }
}

async fn api_export_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let archive = {
        // Checked and started under the profile's lock: no session starts in between.
        let _lock = state.profile_locks.lock(profile.meta.id).await;
        ensure_profile_idle(&state, &profile, "export").await?;
        tar_export(&profile.dir).map_err(|e| {
            error!(profile = %profile.meta.name, error = %e, "Failed to run tar command for profile export");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to archive profile: {e}"),
            )
        })?
    };
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
        axum::body::Body::from_stream(archive),
    ))
}

/// Archives being extracted wait in a hidden directory (`.import-<id>`, which no
/// profile listing shows) until they are complete.
const IMPORT_PREFIX: &str = ".import-";

/// A directory removed when this goes, also when the request is dropped halfway
/// (the client left).
struct StagingDir(Option<PathBuf>);

impl StagingDir {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    /// It was moved into place: nothing to clean up.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let Some(dir) = self.0.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    let _ = fs::remove_dir_all(dir).await;
                });
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// Unpack a gzipped tar from a request body into `dir`, as it arrives. Returns the
/// bytes received.
async fn extract_archive(body: axum::body::Body, dir: &Path) -> Result<u64, ApiError> {
    use futures_util::StreamExt;
    let mut child = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg("-")
        .arg("--no-same-owner")
        .arg("-C")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            error!(error = %e, "Failed to spawn tar process for import");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn tar: {e}"),
            )
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| api_error(StatusCode::INTERNAL_SERVER_ERROR, "tar has no input"))?;
    // Read while tar runs: it blocks on a full stderr pipe, and we on its stdin.
    let errors = drain_stderr(child.stderr.take());

    let mut chunks = Box::pin(capped_chunks(body, MAX_UPLOAD_BYTES));
    let mut received = 0u64;
    let mut write_failed = false;
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(stdin);
                let _ = child.kill().await;
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    format!("Upload failed: {e}"),
                ));
            }
        };
        received += chunk.len() as u64;
        // A failing write means tar gave up (a broken archive): its message says why.
        if stdin.write_all(&chunk).await.is_err() {
            write_failed = true;
            break;
        }
    }
    drop(stdin);
    if received == 0 {
        let _ = child.kill().await;
        return Err(api_error(StatusCode::BAD_REQUEST, "Empty archive body"));
    }
    let status = child.wait().await.map_err(|e| {
        error!(error = %e, "Failed to wait on tar process");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tar extraction error: {e}"),
        )
    })?;
    if write_failed || !status.success() {
        let why = errors.await.unwrap_or_default();
        error!(error = %why, "tar extraction failed");
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("tar extraction failed: {why}"),
        ));
    }
    Ok(received)
}

/// Import an archive as the profile named in the path. The archive's own
/// `profile.json` supplies the engine; archives from the old name-based
/// layout fall back to the `firefox_` naming convention. The archive is
/// unpacked as it arrives, into a hidden directory that becomes the profile once
/// it is complete.
async fn api_import_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ImportProfileParams>,
    body: axum::body::Body,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&name)?;
    let overwrite = params.overwrite.unwrap_or(false);
    let exists = |name: &str| {
        let dir = state.config.profiles_dir.clone();
        let name = name.to_string();
        async move {
            profiles::find(&dir, &name)
                .await
                .filter(|p| p.meta.name == name)
        }
    };
    // Before the upload is read: a profile that is in the way is no reason to wait for it.
    if !overwrite && exists(&name).await.is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile '{name}' already exists on target"),
        ));
    }

    let mut staging = StagingDir(Some(
        state
            .config
            .profiles_dir
            .join(format!("{IMPORT_PREFIX}{}", Uuid::new_v4())),
    ));
    if let Err(e) = fs::create_dir_all(staging.path()).await {
        error!(profile = %name, error = %e, "Failed to create profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create profile directory",
        ));
    }
    extract_archive(body, staging.path()).await?;

    let (name, engine) = match profiles::read_meta(staging.path()).await {
        Some(meta) => (name, meta.engine),
        None => profiles::legacy_identity(&name),
    };
    let _guard = state.profiles_lock.lock().await;
    let existing = exists(&name).await;
    let _existing_lock = match existing {
        Some(ref e) => Some(state.profile_locks.lock(e.meta.id).await),
        None => None,
    };
    if let Some(ref existing) = existing {
        if !overwrite {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("Profile '{name}' already exists on target"),
            ));
        }
        if profile_use(&state, existing.meta.id).await.is_some() {
            return Err(api_error(
                StatusCode::CONFLICT,
                "Cannot overwrite active profile (in use by a session)",
            ));
        }
    }

    let target_dir = state.config.profiles_dir.join(Uuid::new_v4().to_string());
    fs::rename(staging.path(), &target_dir).await.map_err(|e| {
        error!(profile = %name, error = %e, "Failed to move the imported profile into place");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create profile directory",
        )
    })?;
    staging.disarm();
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
        state.dir_sizes.forget(&existing.dir);
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

    // No session starts on the profile while it is copied (and perhaps removed).
    let _lock = state.profile_locks.lock(source.meta.id).await;
    ensure_profile_idle(&state, &source, "export").await?;

    // 1. Archive the local profile as a stream (profile.json carries the engine)
    let archive = tar_export(&source.dir).map_err(|e| {
        error!(profile = %source.meta.name, error = %e, "Failed to run tar command for profile export");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to archive profile: {e}"),
        )
    })?;

    // 2. Pass it on to the target node's /api/profiles/{target_name}/import as it is produced
    let mut import_url = peer_profile_url(&target_peer_url, &target_name, Some("import"));
    if req.overwrite {
        import_url.push_str("?overwrite=true");
    }

    let upload_res = state
        .peer_request(state.peer_client.post(&import_url))
        .timeout(PeerWait::Long.timeout())
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(reqwest::Body::wrap_stream(archive))
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

    // 3. If delete_source was requested, remove the local directory (unless a session
    //    got hold of the profile meanwhile: it has newer data than what was sent)
    let mut deleted_source = false;
    if req.delete_source {
        if profile_use(&state, source.meta.id).await.is_some() {
            warn!(profile = %source.meta.name, "Profile transferred, but a session uses it now: source kept");
        } else {
            state.dir_sizes.forget(&source.dir);
            if let Err(e) = fs::remove_dir_all(&source.dir).await {
                warn!(profile = %source.meta.name, error = %e, "Profile transferred, but failed to delete source directory");
            } else {
                info!(profile = %source.meta.name, "Source profile deleted after transfer");
                deleted_source = true;
            }
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
        "deleted_source": deleted_source,
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

/// How long a peer may take to answer one of the overview's requests.
const PEER_POLL_TIMEOUT: Duration = Duration::from_millis(1800);

/// What one peer reported.
struct PeerReport {
    state: PeerState,
    name: String,
    url: String,
    sessions: Vec<SessionSummary>,
    profiles: Vec<ProfileInfo>,
    targets: Vec<DevToolsTarget>,
    recordings: Vec<RecordingMeta>,
    max_storage_bytes: u64,
}

/// A GET of the overview's from a peer; empty when it cannot be had.
async fn peer_poll<T: serde::de::DeserializeOwned + Default>(state: &AppState, url: String) -> T {
    match state
        .peer_request(state.http_client.get(url))
        .timeout(PEER_POLL_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => T::default(),
    }
}

async fn fetch_peer(state: &AppState, name: String, url: String) -> PeerReport {
    let unreachable = |latency_ms: Option<u64>, error: String| PeerReport {
        state: PeerState {
            name: name.clone(),
            url: url.clone(),
            status: "unreachable".to_string(),
            version: None,
            revision: None,
            latency_ms,
            active_sessions: 0,
            max_sessions: 8,
            profiles_count: 0,
            recordings_count: 0,
            storage_used_bytes: 0,
            uptime_seconds: 0,
            supported_engines: vec![BrowserEngine::Chromium],
            proxies: Vec::new(),
            settings: None,
            error: Some(error),
        },
        name: name.clone(),
        url: url.clone(),
        sessions: Vec::new(),
        profiles: Vec::new(),
        targets: Vec::new(),
        recordings: Vec::new(),
        max_storage_bytes: 0,
    };

    let t0 = Instant::now();
    let node = state
        .peer_request(state.http_client.get(format!("{url}/api/mesh/node")))
        .timeout(PEER_POLL_TIMEOUT)
        .send()
        .await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    let res = match node {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => return unreachable(Some(latency_ms), format!("HTTP {}", res.status())),
        Err(err) => return unreachable(None, err.to_string()),
    };
    let info: Option<NodeInfo> = res.json().await.ok();

    // The rest at once: a slow peer costs one wait, not four.
    let (sessions, profiles, targets, recordings) = tokio::join!(
        peer_poll::<Vec<SessionSummary>>(state, format!("{url}/api/sessions")),
        peer_poll::<Vec<ProfileInfo>>(state, format!("{url}/api/profiles")),
        peer_poll::<Vec<DevToolsTarget>>(state, format!("{url}/json/list")),
        peer_poll::<Vec<RecordingMeta>>(state, format!("{url}/api/recordings")),
    );

    let (peer, name, max_storage_bytes) = match info {
        Some(info) => (
            PeerState {
                name: info.name.clone(),
                url: url.clone(),
                status: "online".to_string(),
                version: Some(info.version),
                revision: info.revision,
                latency_ms: Some(latency_ms),
                active_sessions: info.active_sessions,
                max_sessions: info.max_sessions,
                profiles_count: info.profiles_count,
                recordings_count: info.recordings_count,
                storage_used_bytes: info.storage_used_bytes,
                uptime_seconds: info.uptime_seconds,
                supported_engines: info.supported_engines,
                proxies: info.proxies,
                settings: Some(info.settings),
                error: None,
            },
            info.name,
            info.max_storage_bytes,
        ),
        // Answered, but not as a node: counted online with what is known.
        None => (
            PeerState {
                name: name.clone(),
                url: url.clone(),
                status: "online".to_string(),
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
                error: None,
            },
            name,
            0,
        ),
    };
    PeerReport {
        state: peer,
        name,
        url,
        sessions,
        profiles,
        targets,
        recordings,
        max_storage_bytes,
    }
}

pub async fn fetch_mesh_overview(state: &AppState) -> MeshOverview {
    let peers: Vec<(String, String)> = {
        let guard = state.peers.read().await;
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };

    // This node and the peers at the same time.
    let local = async {
        let (targets, profiles, recordings) = tokio::join!(
            get_local_targets(state),
            get_local_profiles(state),
            recorder::list_recordings(&state.recorder_config),
        );
        // The tab counts come from the target list: no second round of requests.
        let sessions = snapshot_sessions(state).await;
        let active = state.active_recordings.read().await.clone();
        let sessions: Vec<SessionSummary> = sessions
            .iter()
            .map(|s| {
                let tabs = targets
                    .iter()
                    .filter(|t| t.session_id == Some(s.id) && t.target_type == "page")
                    .count();
                s.summary(tabs, active.get(&s.id))
            })
            .collect();
        (sessions, profiles, targets, recordings)
    };
    let remote = futures_util::future::join_all(
        peers
            .into_iter()
            .map(|(name, url)| fetch_peer(state, name, url)),
    );
    let ((local_sessions, local_profiles, local_targets, local_recordings), reports) =
        tokio::join!(local, remote);

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

    let mut peer_states = Vec::new();
    let mut all_sessions = Vec::new();
    let mut all_profiles = Vec::new();
    let mut all_targets = Vec::new();
    let mut all_recordings = Vec::new();

    let (node_name, node_url) = (local_node.name.clone(), local_node.url.clone());
    all_sessions.extend(local_sessions.into_iter().map(|session| MeshSession {
        session,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_profiles.extend(local_profiles.into_iter().map(|profile| MeshProfile {
        profile,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_targets.extend(local_targets.into_iter().map(|target| MeshTarget {
        target,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_recordings.extend(local_recordings.into_iter().map(|recording| MeshRecording {
        recording,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));

    let mut total_capacity = local_node.max_sessions;
    let mut online_nodes = 1;
    let mut total_storage_bytes = local_node.storage_used_bytes;
    let mut max_storage_bytes = local_node.max_storage_bytes;

    for report in reports {
        if report.state.status == "online" {
            online_nodes += 1;
            total_capacity += report.state.max_sessions;
            total_storage_bytes += report.state.storage_used_bytes;
            max_storage_bytes += report.max_storage_bytes;
        }
        peer_states.push(report.state);
        let (node_name, node_url) = (report.name, report.url);
        all_sessions.extend(report.sessions.into_iter().map(|session| MeshSession {
            session,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_profiles.extend(report.profiles.into_iter().map(|profile| MeshProfile {
            profile,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_targets.extend(report.targets.into_iter().map(|target| MeshTarget {
            target,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_recordings.extend(
            report
                .recordings
                .into_iter()
                .map(|recording| MeshRecording {
                    recording,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                }),
        );
    }

    all_recordings.sort_by_key(|r| std::cmp::Reverse(r.recording.started_at_secs));

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
    let snapshot = state.overview_snapshot(Duration::from_millis(750)).await;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        snapshot.json.clone(),
    ))
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
            Json(serde_json::json!({ "error": "The recording is in use (being recorded, exported or merged)" })),
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
    let sessions: Vec<ManagedSession> = {
        let guard = state.sessions.read().await;
        guard.values().cloned().collect()
    };
    let mut found = None;
    for session in sessions {
        if session.bidi.is_some() {
            continue;
        }
        let url = format!("http://127.0.0.1:{}/json/list", session.port);
        let Ok(res) = state.http_client.get(&url).send().await else {
            continue;
        };
        let Ok(targets) = res.json::<Vec<DevToolsTarget>>().await else {
            continue;
        };
        if let Some(t) = targets.into_iter().find(|t| t.id == page_id) {
            found = Some((session, t.url));
            break;
        }
    }
    let Some((session, page_url)) = found else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Page not found (Chromium sessions only)",
        ));
    };
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
    let page = targets
        .iter()
        .find(|t| t.id == page_id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found"))?;
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
        .authorize_fill(req.request, &page_id, &req.host)
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
    fn default_tabs_come_as_a_list_or_as_words() {
        let req = |json: &str| serde_json::from_str::<UpdateProfileRequest>(json).unwrap().tabs;
        assert_eq!(req(r#"{"tabs":["a.test","b.test"]}"#), Some(vec!["a.test".into(), "b.test".into()]));
        assert_eq!(req(r#"{"tabs":"a.test  b.test,c.test"}"#).map(|t| t.len()), Some(3));
        assert_eq!(req(r#"{"tabs":[]}"#), Some(vec![]));
        assert_eq!(req(r#"{}"#), None);
        let urls = tab_urls(&["outlook.office.com".into(), "".into(), "https://outlook.office.com/".into()]).unwrap();
        assert_eq!(urls, vec!["https://outlook.office.com/".to_string()]);
        assert!(tab_urls(&["ftp://x.test/".into()]).is_err());
    }

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

    async fn fake_session(profile: Option<Uuid>) -> ManagedSession {
        ManagedSession {
            id: Uuid::new_v4(),
            name: Arc::new(std::sync::RwLock::new("test".into())),
            clients: Arc::default(),
            engine: BrowserEngine::Chromium,
            port: 1,
            ws_path: String::new(),
            profile: None,
            profile_id: profile,
            persistent: true,
            record_audio: false,
            created_at: Instant::now(),
            client_count: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(idle::Activity::new(0)),
            adblock: false,
            relay: relay::Relay::start(None).await.unwrap(),
            audio_sink: None,
            display: None,
            display_size: None,
            window_size: (1, 1),
            video: false,
            color_scheme: ColorScheme::default(),
            screen: Arc::default(),
            show_touches: Arc::default(),
            audible: Arc::new(AtomicBool::new(false)),
            browser: Arc::new(Mutex::new(None)),
            bidi: None,
        }
    }

    type Sessions = Arc<RwLock<HashMap<Uuid, ManagedSession>>>;

    async fn slot(
        sessions: &Sessions,
        starting: &Arc<Starting>,
        locks: &ProfileLocks,
        max: usize,
        profile: Option<Uuid>,
    ) -> Result<Slot, (StatusCode, &'static str)> {
        reserve_slot(sessions, starting, locks, max, profile).await
    }

    #[tokio::test]
    async fn launches_count_against_the_cap_before_they_are_sessions() {
        let sessions = Sessions::default();
        let (starting, locks) = (Arc::new(Starting::default()), ProfileLocks::default());
        let first = slot(&sessions, &starting, &locks, 2, None).await.unwrap();
        let _second = slot(&sessions, &starting, &locks, 2, None).await.unwrap();
        let third = slot(&sessions, &starting, &locks, 2, None).await;
        assert!(matches!(third, Err((StatusCode::SERVICE_UNAVAILABLE, _))));
        // A failed launch gives its slot back.
        drop(first);
        assert!(slot(&sessions, &starting, &locks, 2, None).await.is_ok());
    }

    #[tokio::test]
    async fn a_profile_launched_twice_at_once_gets_one_browser() {
        let sessions = Sessions::default();
        let (starting, locks) = (
            Arc::new(Starting::default()),
            Arc::new(ProfileLocks::default()),
        );
        let profile = Uuid::new_v4();
        let Slot::Reserved(reservation) = slot(&sessions, &starting, &locks, 8, Some(profile))
            .await
            .unwrap()
        else {
            panic!("the first launch reserves");
        };
        assert!(starting.has_profile(profile));
        // Another profile is not held up.
        assert!(matches!(
            slot(&sessions, &starting, &locks, 8, Some(Uuid::new_v4())).await,
            Ok(Slot::Reserved(_))
        ));
        let second = {
            let (sessions, starting, locks) = (sessions.clone(), starting.clone(), locks.clone());
            tokio::spawn(async move {
                reserve_slot(&sessions, &starting, &locks, 8, Some(profile)).await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!second.is_finished(), "waits for the launch in progress");
        // The launch finishes: the session is in the map, then the reservation goes.
        let session = fake_session(Some(profile)).await;
        let id = session.id;
        sessions.write().await.insert(id, session);
        drop(reservation);
        let Ok(Slot::Attach(attached)) = second.await.unwrap() else {
            panic!("the second request attaches");
        };
        assert_eq!(attached.id, id);
        assert!(!starting.has_profile(profile));
    }

    #[tokio::test]
    async fn a_profile_that_is_being_changed_is_not_admitted_meanwhile() {
        let sessions = Sessions::default();
        let (starting, locks) = (
            Arc::new(Starting::default()),
            Arc::new(ProfileLocks::default()),
        );
        let profile = Uuid::new_v4();
        let held = locks.lock(profile).await;
        let launch = {
            let (sessions, starting, locks) = (sessions.clone(), starting.clone(), locks.clone());
            tokio::spawn(async move {
                reserve_slot(&sessions, &starting, &locks, 8, Some(profile)).await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!launch.is_finished());
        drop(held);
        assert!(matches!(launch.await.unwrap(), Ok(Slot::Reserved(_))));
    }

    #[test]
    fn directory_sizes_are_remembered_for_a_while() {
        let sizes = DirSizes::default();
        let dir = Path::new("/profiles/a");
        let t0 = Instant::now();
        assert_eq!(sizes.fresh(dir, t0), None);
        sizes.store(dir, t0, 5);
        assert_eq!(sizes.fresh(dir, t0 + Duration::from_secs(30)), Some(5));
        assert_eq!(sizes.fresh(dir, t0 + DIR_SIZE_TTL), None, "stale");
        sizes.store(dir, t0, 7);
        sizes.forget(dir);
        assert_eq!(sizes.fresh(dir, t0), None, "forgotten after a change");
        // Old entries of directories nobody asks about are dropped.
        sizes.store(Path::new("/profiles/gone"), t0, 1);
        sizes.store(dir, t0 + DIR_SIZE_TTL * 11, 2);
        assert_eq!(sizes.0.lock().unwrap().len(), 1);
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

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bd-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_profile_survives_the_trip_through_export_and_import() {
        use futures_util::StreamExt;
        let (src, dst) = (scratch_dir("src"), scratch_dir("dst"));
        std::fs::create_dir_all(src.join("Default")).unwrap();
        std::fs::write(src.join("profile.json"), b"{}").unwrap();
        // Bigger than the pipes between us and tar, and not compressible.
        let mut big = Vec::new();
        let mut x = 0x2545_f491_u32;
        for _ in 0..(6 * 1024 * 1024) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            big.push(x as u8);
        }
        std::fs::write(src.join("Default/Cookies"), &big).unwrap();

        let archive = tar_export(&src).unwrap();
        let received = extract_archive(axum::body::Body::from_stream(archive), &dst)
            .await
            .unwrap();
        assert!(received > 6 * 1024 * 1024);
        assert_eq!(std::fs::read(dst.join("Default/Cookies")).unwrap(), big);
        assert_eq!(std::fs::read(dst.join("profile.json")).unwrap(), b"{}");

        // An export of something that is not there fails at the end of the stream.
        let missing = tar_export(&src.join("nope")).unwrap();
        let chunks: Vec<_> = missing.collect().await;
        assert!(chunks.last().unwrap().is_err());
        std::fs::remove_dir_all(src).unwrap();
        std::fs::remove_dir_all(dst).unwrap();
    }

    #[tokio::test]
    async fn a_broken_or_empty_archive_is_refused_without_stalling() {
        let dst = scratch_dir("bad");
        let body = |bytes: Vec<u8>| {
            axum::body::Body::from_stream(futures_util::stream::iter(
                bytes
                    .chunks(64 * 1024)
                    .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
                    .collect::<Vec<_>>(),
            ))
        };
        // Much more than a pipe holds: tar quits at the first bytes.
        let err = extract_archive(body(vec![b'x'; 4 * 1024 * 1024]), &dst)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1 .0["error"].as_str().unwrap().contains("tar"));
        let err = extract_archive(body(Vec::new()), &dst).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(dst).unwrap();
    }

    #[tokio::test]
    async fn a_staging_directory_goes_when_the_request_does() {
        let dir = scratch_dir("stage");
        let staging = StagingDir(Some(dir.clone()));
        drop(staging);
        for _ in 0..50 {
            if !dir.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the staging directory is still there");
    }

    #[test]
    fn profile_recording_settings_can_be_left_set_or_cleared() {
        let req: UpdateProfileRequest = serde_json::from_str(r#"{"record_audio": false}"#).unwrap();
        assert_eq!((req.record, req.record_audio), (None, Some(Some(false))));
        let req: UpdateProfileRequest = serde_json::from_str(r#"{"record": null}"#).unwrap();
        assert_eq!(req.record, Some(None));
    }
}
