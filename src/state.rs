//! Shared daemon state: the session table, the application state and their small helpers.

use super::*;

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
    /// Open live views (the web UI's view of the session).
    pub viewer_count: Arc<AtomicUsize>,
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
            viewer_count: self.viewer_count.load(Ordering::SeqCst),
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
            recording_audio: recording.is_some_and(SessionRecorderHandle::audio_on),
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
    /// Open live views of it, in a browser or the TUI.
    #[serde(default)]
    pub viewer_count: usize,
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
    /// Base URL of public share links.
    pub share_url: String,
    /// Public links to recordings and sessions (see shares.rs).
    pub shares: Arc<shares::Store>,
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
    /// What the sessions' pages downloaded (see downloads.rs).
    pub downloads: Arc<downloads::Downloads>,
    /// Untrusted, temporary browser registrations (not cluster peers).
    pub extensions: Arc<extension::Broker>,
}

/// How long a profile directory's size is taken as it was.
pub(crate) const DIR_SIZE_TTL: Duration = Duration::from_secs(60);

/// Sizes of directories, remembered for a while: a profile can be gigabytes and
/// every dashboard asks every two seconds.
#[derive(Default)]
pub struct DirSizes(std::sync::Mutex<HashMap<PathBuf, (Instant, u64)>>);

impl DirSizes {
    pub(crate) fn fresh(&self, dir: &Path, now: Instant) -> Option<u64> {
        let map = self.0.lock().unwrap();
        let (at, size) = map.get(dir)?;
        (now.saturating_duration_since(*at) < DIR_SIZE_TTL).then_some(*size)
    }

    pub(crate) fn store(&self, dir: &Path, now: Instant, size: u64) {
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
}

#[derive(Default)]
pub struct TabOrder {
    pub(crate) next: u64,
    pub(crate) seen: HashMap<String, u64>,
}

impl TabOrder {
    pub(crate) fn rank(&mut self, id: &str) -> u64 {
        if let Some(n) = self.seen.get(id) {
            return *n;
        }
        self.next += 1;
        self.seen.insert(id.to_string(), self.next);
        self.next
    }

    /// Sort tabs into a stable order and forget tabs that are gone.
    pub(crate) fn sort(&mut self, targets: &mut [DevToolsTarget]) {
        targets.sort_by_cached_key(|t| self.rank(&t.id));
        let live: std::collections::HashSet<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        self.seen.retain(|id, _| live.contains(id.as_str()));
    }
}

pub(crate) async fn compute_dir_size(path: PathBuf) -> u64 {
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
}
