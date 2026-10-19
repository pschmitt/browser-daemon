//! Session lifecycle: admission, spawning, adoption, termination and the /api/sessions handlers.

use super::*;

/// Sessions that are launching but not in the map yet. Whoever wants to know
/// whether a profile or a slot is taken has to count these too.
#[derive(Default)]
pub struct Starting {
    pub(crate) inner: std::sync::Mutex<StartingInner>,
    pub(crate) done: tokio::sync::Notify,
}

#[derive(Default)]
pub(crate) struct StartingInner {
    pub(crate) count: usize,
    pub(crate) profiles: std::collections::HashSet<Uuid>,
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
    pub(crate) starting: Arc<Starting>,
    pub(crate) profile: Option<Uuid>,
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
pub(crate) enum Slot {
    /// The profile has a session already.
    Attach(Box<ManagedSession>),
    Reserved(Reservation),
}

/// Claim a slot for a launch, or find the session the profile already has.
/// A profile another request is launching right now is waited for, then
/// attached to.
pub(crate) async fn reserve_slot(
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

impl AppState {
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
            viewer_count: Arc::new(AtomicUsize::new(0)),
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
        crate::downloads::watch(self.clone(), &session);
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
    pub(crate) async fn reserve_slot(
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
            .map_or((false, false), |r| (true, r.audio_on()));
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
                        viewer_count: Arc::new(AtomicUsize::new(0)),
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
                    crate::downloads::watch(self.clone(), &session);
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
    pub(crate) async fn discard_record(&self, rec: &persist::SessionRecord) {
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
    pub(crate) async fn resolve_or_create_profile(
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
    pub(crate) async fn profile_record_audio(&self, profile: Option<Uuid>) -> bool {
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
            // Whenever the session has sound, so it can be switched on later.
            self.config
                .audio
                .clone()
                .zip(session.audio_sink.clone())
                .map(|(cfg, sink)| {
                    (
                        rec_audio::Source { cfg, sink },
                        audio.unwrap_or(session.record_audio),
                    )
                }),
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
        self.downloads.forget(self.config.state_dir(), id).await;
        // Live views of the session end with it.
        self.viewers.close_session(*id);
        self.thumbnails.forget(*id);
        // A public link to it ends with it, and so do its notes.
        self.shares
            .remove_target(shares::ShareKind::Session, *id)
            .await;
        notes::forget_session(self, *id).await;
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
        self.history.forget(&history_key(None, *id));
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
    /// An integration rather than an agent ("Home Assistant · HRworks"): shown under
    /// that name, and not counted as an agent whatever library it drives the browser with.
    pub client: Option<String>,
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
pub(crate) const PHONE_SIZE: (u32, u32) = (412, 915);

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
pub(crate) fn writable_extension_copy(src: &Path, profile_dir: &Path) -> Option<PathBuf> {
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

pub(crate) fn spawn_bidi_frame_feeder(hub: Arc<BidiHub>, handle: SessionRecorderHandle) {
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

pub(crate) async fn get_local_sessions(state: &AppState) -> Vec<SessionSummary> {
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

pub(crate) async fn api_list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    Ok(Json(get_local_sessions(&state).await))
}

pub(crate) async fn api_create_session(
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

pub(crate) async fn api_get_session(
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
    // With its public link, for the live view.
    let mut session_json = serde_json::to_value(&summary).unwrap_or_default();
    if let Some(share) = state
        .shares
        .all()
        .into_iter()
        .find(|s| s.kind == shares::ShareKind::Session && s.target == id)
    {
        session_json["share"] = serde_json::to_value(state.share_view(share)).unwrap_or_default();
    }

    if let Some(sum) = notes::session_summary(&state, id).await {
        session_json["notes"] = serde_json::to_value(sum).unwrap_or_default();
    }

    Ok(Json(serde_json::json!({
        "session": session_json,
        "targets": targets,
    })))
}

pub(crate) async fn api_delete_session(
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

#[cfg(test)]
mod tests {
    use super::*;

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
            viewer_count: Arc::new(AtomicUsize::new(0)),
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
}
