use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::process::Command;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::audio::{AudioConfig, Sink};
use crate::bidi::BidiHub;
use crate::display::{DisplayConfig, VirtualDisplay};
use crate::persist::{AudioRecord, DisplayRecord, SessionRecord};
use crate::proc::{self, Proc};
use crate::profiles::{ColorScheme, Profile};
use crate::proxies::{self, Proxy};
use crate::relay::Relay;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserEngine {
    #[default]
    #[serde(alias = "Chromium", alias = "CHROMIUM")]
    Chromium,
    #[serde(alias = "Firefox", alias = "FIREFOX")]
    Firefox,
}

impl std::fmt::Display for BrowserEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserEngine::Chromium => write!(f, "chromium"),
            BrowserEngine::Firefox => write!(f, "firefox"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BrowserConfig {
    pub chromium_bin: PathBuf,
    pub firefox_bin: Option<PathBuf>,
    pub display: Option<String>,
    pub profiles_dir: PathBuf,
    pub default_width: u32,
    pub default_height: u32,
    pub max_session_memory_mb: Option<u64>,
    /// Unpacked ad blocker (uBlock Origin Lite) loaded into Chromium.
    pub chromium_adblock: Option<PathBuf>,
    /// Ad blocker add-on (uBlock Origin .xpi) installed into Firefox.
    pub firefox_adblock: Option<PathBuf>,
    /// Upstream proxies sessions can choose from.
    pub proxies: Vec<Proxy>,
    /// PulseAudio for session audio; none: browsers play into the void.
    pub audio: Option<AudioConfig>,
    /// Give every session its own Xvfb instead of the shared `display`.
    pub session_displays: Option<DisplayConfig>,
    /// H.264 live view of per-session displays.
    pub video: Option<crate::video::VideoConfig>,
    /// Render Chromium on the GPU (ANGLE on Vulkan) instead of in software, so
    /// pages see a real GPU in WebGL: software renderers are a bot tell.
    pub chromium_gpu: bool,
}

impl BrowserConfig {
    /// The state directory: next to the profiles.
    pub fn state_dir(&self) -> &Path {
        self.profiles_dir.parent().unwrap_or(&self.profiles_dir)
    }

    pub fn adblock_available(&self, engine: BrowserEngine) -> bool {
        match engine {
            BrowserEngine::Chromium => self.chromium_adblock.is_some(),
            BrowserEngine::Firefox => self.firefox_adblock.is_some(),
        }
    }
}

pub struct ActiveBrowser {
    pub id: Uuid,
    /// The window size it was launched with.
    pub window_size: (u32, u32),
    pub engine: BrowserEngine,
    pub port: u16,
    pub ws_path: String,
    /// The browser's main process: spawned by this daemon or adopted after a restart.
    pub proc: Proc,
    /// Shared WebDriver BiDi session (Firefox only).
    pub bidi: Option<Arc<BidiHub>>,
    /// User-data directory of a session without a profile, removed with it.
    temp_dir: Option<PathBuf>,
    /// Firefox's stderr (a file: a pipe would break when the daemon restarts).
    firefox_log: Option<PathBuf>,
    pub profile_name: Option<String>,
    pub profile_id: Option<Uuid>,
    pub created_at: Instant,
    pub adblock: bool,
    /// Local SOCKS relay all of the browser's traffic goes through.
    pub relay: Arc<Relay>,
    /// The session's audio sink, when the node has audio.
    pub audio: Option<Sink>,
    /// The session's own X display, in per-session display mode.
    pub display: Option<VirtualDisplay>,
}

impl ActiveBrowser {
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        config: &BrowserConfig,
        engine: BrowserEngine,
        profile: Option<&Profile>,
        width: Option<u32>,
        height: Option<u32>,
        max_memory_mb: Option<u64>,
        adblock: bool,
        proxy: Option<&Proxy>,
        color_scheme: ColorScheme,
        start_tab: bool,
        default_tabs: bool,
        open: &[String],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let id = Uuid::new_v4();
        // What the browser opens with: the profile's default tabs, else a blank tab.
        let start_pages: Vec<String> = if !open.is_empty() {
            open.to_vec()
        } else {
            profile
                .filter(|_| default_tabs)
                .map(|p| p.meta.tabs.clone())
                .filter(|tabs| !tabs.is_empty())
                .unwrap_or_default()
        };
        let relay = Relay::start(proxy.cloned()).await?;
        let audio = match config.audio {
            Some(ref cfg) => match Sink::create(cfg, id).await {
                Ok(sink) => Some(sink),
                Err(e) => {
                    warn!(session_id = %id, error = %e, "No audio for this session");
                    None
                }
            },
            None => None,
        };
        let adblock = adblock && config.adblock_available(engine);
        let width = width.unwrap_or(config.default_width);
        let height = height.unwrap_or(config.default_height);
        let session_display = match config.session_displays {
            Some(ref dcfg) => Some(VirtualDisplay::start(dcfg, width, height).await?),
            None => None,
        };
        // The X display the browser runs on: its own, or the shared one.
        let display_name = session_display
            .as_ref()
            .map(|d| d.name.clone())
            .or_else(|| config.display.clone());

        let (user_data_dir, temp_dir, profile_name, profile_id) = match profile {
            Some(profile) => {
                fs::create_dir_all(&profile.dir).await?;
                (
                    profile.dir.clone(),
                    None,
                    Some(profile.meta.name.clone()),
                    Some(profile.meta.id),
                )
            }
            None => {
                let temp = tempfile::Builder::new()
                    .prefix(&format!("browserd-{}-{}-", engine, id))
                    .tempdir()?;
                let path = temp.path().to_path_buf();
                (path, Some(temp), None, None)
            }
        };

        match engine {
            BrowserEngine::Chromium => {
                info!(
                    session_id = %id,
                    engine = "chromium",
                    profile = ?profile_name,
                    dir = %user_data_dir.display(),
                    "Launching Chromium"
                );

                let mut cmd = Command::new(&config.chromium_bin);
                cmd.arg("--remote-debugging-port=0")
                    .arg(format!("--user-data-dir={}", user_data_dir.display()))
                    .arg("--disable-blink-features=AutomationControlled")
                    // Keeps Chromium from showing a bar about the unsupported flag above.
                    .arg("--test-type")
                    .arg("--hide-crash-restore-bubble")
                    .arg(format!("--window-size={},{}", width, height))
                    // Top-left: with a display of its own, the screen is the window.
                    .arg("--window-position=0,0")
                    // Window pixels = CSS pixels: Xvfb's DPI otherwise yields an odd
                    // device pixel ratio (1.046875) and slightly blurry frames.
                    .arg("--force-device-scale-factor=1")
                    .arg("--no-first-run")
                    .arg("--no-default-browser-check")
                    .arg("--disable-sync")
                    .arg("--disable-background-networking")
                    .arg("--disable-default-apps")
                    .arg("--disable-component-update")
                    .arg("--disable-breakpad")
                    .arg("--disable-dev-shm-usage")
                    .arg("--disable-background-timer-throttling")
                    .arg("--disable-backgrounding-occluded-windows")
                    .arg("--disable-renderer-backgrounding")
                    .arg("--force-color-profile=srgb")
                    .arg("--password-store=basic");
                // Chromium keeps cookies without an expiry (many logins) across runs only
                // in its "continue where you left off" mode. Its tabs are not wanted
                // back: the saved session is dropped before each launch, so there is
                // nothing to restore but the cookies.
                if temp_dir.is_none() {
                    cmd.arg("--restore-last-session");
                    let default = user_data_dir.join("Default");
                    let _ = fs::remove_dir_all(default.join("Sessions")).await;
                    for old in [
                        "Current Session",
                        "Current Tabs",
                        "Last Session",
                        "Last Tabs",
                    ] {
                        let _ = fs::remove_file(default.join(old)).await;
                    }
                }
                // Outlives the daemon: a restart adopts it again.
                proc::own_group(&mut cmd);
                // No start tab for an agent opening its own (Playwright): it would sit
                // there unused. Chromium then opens the window with the first tab.
                if !start_pages.is_empty() {
                    cmd.args(&start_pages);
                } else if start_tab {
                    cmd.arg("about:blank");
                } else {
                    cmd.arg("--no-startup-window");
                }

                if let (true, Some(ext)) = (adblock, &config.chromium_adblock) {
                    // Unbranded Chromium still honours --load-extension; the
                    // feature guard keeps it that way should it become default.
                    cmd.arg(format!("--load-extension={}", ext.display()))
                        .arg("--disable-features=DisableLoadExtensionCommandLineSwitch");
                }

                if config.chromium_gpu {
                    // Vulkan reaches the GPU through the render node; GL would go
                    // through Xvfb, which has none, and end up in llvmpipe.
                    cmd.arg("--ignore-gpu-blocklist")
                        .arg("--use-gl=angle")
                        .arg("--use-angle=vulkan")
                        .arg("--enable-features=Vulkan,VulkanFromANGLE,DefaultANGLEVulkan");
                } else {
                    cmd.arg("--disable-gpu");
                }

                cmd.args(proxies::chromium_args(relay.port()));

                // Dark browser UI, and prefers-color-scheme: dark for pages.
                if color_scheme == ColorScheme::Dark {
                    cmd.arg("--force-dark-mode");
                }

                let effective_mem = max_memory_mb.or(config.max_session_memory_mb);
                if let Some(mem_mb) = effective_mem {
                    cmd.arg(format!("--js-flags=--max-old-space-size={}", mem_mb));
                }

                if let Some(ref sink) = audio {
                    cmd.envs(sink.env());
                }
                if let Some(ref display) = display_name {
                    cmd.env("DISPLAY", display);
                }

                // A persistent profile keeps the previous run's port file when
                // Chromium was killed; reading it would point at a dead port.
                match fs::remove_file(user_data_dir.join("DevToolsActivePort")).await {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
                // Save-password and autofill bubbles are browser UI, out of reach
                // for the live view's input; keep them from ever showing up.
                let prefs_dir = user_data_dir.join("Default");
                fs::create_dir_all(&prefs_dir).await?;
                let prefs_file = prefs_dir.join("Preferences");
                let existing = fs::read_to_string(&prefs_file).await.unwrap_or_default();
                fs::write(&prefs_file, chromium_preferences(&existing)).await?;
                let proc = Proc::spawned(cmd.spawn()?).ok_or("Chromium exited right away")?;
                relay.set_owner(proc.pid, proc.start);
                let (port, ws_path) = match wait_for_devtools_port(&user_data_dir).await {
                    Ok(res) => res,
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Chromium failed to initialize DevTools port");
                        return Err(e);
                    }
                };

                info!(session_id = %id, port = port, ws_path = %ws_path, "Chromium ready");

                Ok(Self {
                    id,
                    engine,
                    port,
                    ws_path,
                    proc,
                    bidi: None,
                    temp_dir: temp_dir.map(tempfile::TempDir::keep),
                    firefox_log: None,
                    profile_name,
                    profile_id,
                    created_at: Instant::now(),
                    adblock,
                    relay: relay.clone(),
                    audio,
                    display: session_display,
                    window_size: (width, height),
                })
            }
            BrowserEngine::Firefox => {
                let firefox_bin = config
                    .firefox_bin
                    .as_ref()
                    .ok_or("Firefox binary not configured on this host")?;

                info!(
                    session_id = %id,
                    engine = "firefox",
                    profile = ?profile_name,
                    dir = %user_data_dir.display(),
                    "Launching Firefox"
                );

                fs::create_dir_all(&user_data_dir).await?;
                // Per-launch prefs are rewritten every time: the relay's port changes,
                // and so may the appearance.
                let user_js = user_data_dir.join("user.js");
                let existing = fs::read_to_string(&user_js).await.unwrap_or_default();
                let mut prefs = proxies::firefox_prefs(relay.port());
                // Pages (prefers-color-scheme: 0 dark, 1 light) and the browser's UI.
                let dark = color_scheme == ColorScheme::Dark;
                prefs.push(format!(
                    "user_pref(\"layout.css.prefers-color-scheme.content-override\", {});",
                    if dark { 0 } else { 1 }
                ));
                prefs.push(format!(
                    "user_pref(\"ui.systemUsesDarkTheme\", {});",
                    u8::from(dark)
                ));
                // No save-password or autofill prompts: the live view can't reach them.
                for (pref, value) in [
                    ("signon.rememberSignons", "false"),
                    ("signon.autofillForms", "false"),
                    ("signon.generation.enabled", "false"),
                    ("signon.firefoxRelay.feature", "\"disabled\""),
                    ("extensions.formautofill.addresses.enabled", "false"),
                    ("extensions.formautofill.creditCards.enabled", "false"),
                    // Sessions end by being killed: no "restore session" afterwards.
                    ("browser.sessionstore.resume_from_crash", "false"),
                    // A profile keeps the cookies without an expiry that logins often
                    // use (Firefox's restore mode; the command line's pages open, not
                    // the old tabs); throwaways start blank.
                    (
                        "browser.startup.page",
                        if temp_dir.is_none() { "3" } else { "1" },
                    ),
                    // 2 = block: sites can't ask to show notifications.
                    ("permissions.default.desktop-notification", "2"),
                    // Same for location requests (a popup the view can't answer).
                    ("permissions.default.geo", "2"),
                    // No translation offers (a toolbar prompt the view can't reach).
                    ("browser.translations.enable", "false"),
                    ("browser.translations.automaticallyPopup", "false"),
                ] {
                    prefs.push(format!("user_pref(\"{pref}\", {value});"));
                }
                fs::write(&user_js, proxies::firefox_user_js(&existing, &prefs)).await?;

                let log = config
                    .state_dir()
                    .join("sessions")
                    .join(format!("{id}.log"));
                fs::create_dir_all(config.state_dir().join("sessions")).await?;
                // Headful on the virtual display, like Chromium; headless only without one.
                let mut cmd = Command::new(firefox_bin);
                if display_name.is_none() {
                    cmd.arg("--headless");
                }
                cmd.arg("--new-instance")
                    .arg("--no-remote")
                    .arg(format!("--window-size={},{}", width, height))
                    .arg("--width")
                    .arg(width.to_string())
                    .arg("--height")
                    .arg(height.to_string())
                    .arg("--profile")
                    .arg(&user_data_dir)
                    .arg("--remote-debugging-port=0")
                    .args(if start_pages.is_empty() {
                        vec!["about:blank".to_string()]
                    } else {
                        start_pages.clone()
                    })
                    .env("MOZ_ENABLE_WAYLAND", "0")
                    .stderr(std::process::Stdio::from(
                        fs::File::create(&log).await?.into_std().await,
                    ))
                    .stdout(std::process::Stdio::null());
                proc::own_group(&mut cmd);

                if let Some(ref sink) = audio {
                    cmd.envs(sink.env());
                }
                if let Some(ref display) = display_name {
                    cmd.env("DISPLAY", display);
                }

                let proc = Proc::spawned(cmd.spawn()?).ok_or("Firefox exited right away")?;
                relay.set_owner(proc.pid, proc.start);

                // A failure below drops `proc`, which kills Firefox.
                let port = match wait_for_firefox_bidi(&log, Duration::from_secs(15)).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Firefox failed to initialize WebDriver BiDi");
                        let _ = fs::remove_file(&log).await;
                        return Err(e);
                    }
                };

                let hub = match BidiHub::connect(port).await {
                    Ok(h) => h,
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Failed to open Firefox BiDi session");
                        let _ = fs::remove_file(&log).await;
                        return Err(e);
                    }
                };

                // Installed per launch: BiDi add-ons last until Firefox exits.
                let mut adblock = adblock;
                if let (true, Some(xpi)) = (adblock, &config.firefox_adblock) {
                    let res = hub
                        .call(
                            "webExtension.install",
                            serde_json::json!({
                                "extensionData": { "type": "archivePath", "path": xpi }
                            }),
                        )
                        .await;
                    if let Err(e) = res {
                        warn!(session_id = %id, error = %e, "Could not install the ad blocker");
                        adblock = false;
                    }
                }

                info!(session_id = %id, port = port, ws_path = "session", "Firefox ready");

                Ok(Self {
                    id,
                    engine,
                    port,
                    ws_path: "session".to_string(),
                    proc,
                    bidi: Some(hub),
                    temp_dir: temp_dir.map(tempfile::TempDir::keep),
                    firefox_log: Some(log),
                    profile_name,
                    profile_id,
                    created_at: Instant::now(),
                    adblock,
                    relay: relay.clone(),
                    audio,
                    display: session_display,
                    window_size: (width, height),
                })
            }
        }
    }

    pub async fn terminate(&mut self) {
        info!(session_id = %self.id, engine = %self.engine, "Terminating browser instance");
        if self.temp_dir.is_some() {
            self.proc.terminate(Duration::from_secs(3)).await;
        } else {
            // A profile outlives the session: let the browser write cookies, storage
            // and its session to disk. It is killed only when that takes too long.
            self.quiesce().await;
            self.proc.terminate(Duration::from_secs(15)).await;
        }
        if let Some(ref mut display) = self.display {
            display.stop().await;
        }
        // A throwaway profile can be large: not on the runtime's thread.
        if let Some(dir) = self.temp_dir.take() {
            let _ = fs::remove_dir_all(dir).await;
        }
        if let Some(log) = self.firefox_log.take() {
            let _ = fs::remove_file(log).await;
        }
    }

    /// The browser quits the way a user's quit does, writing its data to disk first.
    pub async fn quiesce(&mut self) {
        if self.engine == BrowserEngine::Chromium {
            self.close_chromium().await;
        } else if let Some(hub) = self.bidi.clone() {
            hub.close_browser().await;
            if !self.proc.wait_exit(Duration::from_secs(12)).await {
                warn!(session_id = %self.id, "Firefox did not quit in time");
            }
        }
    }

    /// The temporary profile of a throwaway session.
    pub fn temp_dir_path(&self) -> Option<&PathBuf> {
        self.temp_dir.as_ref()
    }

    /// Ask Chromium to close itself (all windows, then exit), and wait for it.
    async fn close_chromium(&mut self) {
        use futures_util::SinkExt;
        let ws = async {
            let res = reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}/json/version", self.port))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .ok()?;
            let v: serde_json::Value = res.json().await.ok()?;
            v["webSocketDebuggerUrl"].as_str().map(str::to_string)
        }
        .await;
        let Some(ws) = ws else { return };
        let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(ws))
                .await
        else {
            return;
        };
        let close = serde_json::json!({ "id": 1, "method": "Browser.close" });
        let _ = socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                close.to_string().into(),
            ))
            .await;
        if !self.proc.wait_exit(Duration::from_secs(12)).await {
            warn!(session_id = %self.id, "Chromium did not close in time");
        }
    }

    /// Files a session leaves on disk (temporary profile, Firefox's log), for the
    /// startup cleanup of everything else.
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.temp_dir.iter().chain(self.firefox_log.iter())
    }

    fn cleanup_files(&mut self) {
        if let Some(dir) = self.temp_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
        if let Some(log) = self.firefox_log.take() {
            let _ = std::fs::remove_file(log);
        }
    }

    /// What a restarted daemon needs to adopt this browser again.
    pub fn record(&self, state: SessionState) -> SessionRecord {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        SessionRecord {
            version: crate::persist::record_version(),
            boot_id: SessionRecord::new_boot_id(),
            id: self.id,
            name: state.name,
            engine: self.engine,
            persistent: state.persistent,
            port: self.port,
            ws_path: self.ws_path.clone(),
            pid: self.proc.pid,
            pid_start: self.proc.start,
            profile: self.profile_name.clone(),
            profile_id: self.profile_id,
            temp_dir: self.temp_dir.clone(),
            firefox_log: self.firefox_log.clone(),
            window_size: self.window_size,
            adblock: self.adblock,
            color_scheme: state.color_scheme,
            screen: state.screen,
            show_touches: state.show_touches,
            idle_timeout_secs: state.idle_timeout_secs,
            relay_port: self.relay.port(),
            proxy: self.relay.current(),
            audio: self.audio.as_ref().map(|a| AudioRecord {
                sink: a.name.clone(),
                module: a.module(),
            }),
            display: self.display.as_ref().map(|d| {
                let (pid, pid_start) = d.identity();
                DisplayRecord {
                    name: d.name.clone(),
                    width: d.width,
                    height: d.height,
                    pid,
                    pid_start,
                }
            }),
            shared_display: state.shared_display,
            video: state.video,
            created_at_secs: wall.saturating_sub(self.created_at.elapsed().as_secs()),
            recording: state.recording,
            recording_audio: Some(state.recording_audio),
        }
    }

    /// Take over a browser an earlier daemon run left running. Anything that
    /// fails drops the half-built parts, which kills the browser: one this
    /// daemon cannot drive is of no use.
    pub async fn adopt(
        config: &BrowserConfig,
        rec: &SessionRecord,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut proc = Proc::adopt(rec.pid, rec.pid_start).ok_or("browser process is gone")?;
        let display = match rec.display {
            Some(ref d) => Some(
                VirtualDisplay::adopt(&d.name, d.width, d.height, d.pid, d.pid_start)
                    .ok_or("session display is gone")?,
            ),
            None => None,
        };
        let proxy = rec
            .proxy
            .as_ref()
            .and_then(|id| config.proxies.iter().find(|p| &p.id == id))
            .cloned();
        let relay = Relay::start_on(rec.relay_port, proxy).await?;
        relay.set_owner(proc.pid, proc.start);
        let audio = match (&config.audio, &rec.audio) {
            (Some(cfg), Some(a)) => Some(Sink::adopt(cfg, a.sink.clone(), a.module)),
            _ => None,
        };
        let bidi = match rec.engine {
            BrowserEngine::Chromium => {
                let reachable = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::net::TcpStream::connect(("127.0.0.1", rec.port)),
                )
                .await;
                if !matches!(reachable, Ok(Ok(_))) {
                    return Err("browser's debugging port does not answer".into());
                }
                None
            }
            BrowserEngine::Firefox => {
                let hub = BidiHub::connect(rec.port).await?;
                // Add-ons installed over BiDi live until Firefox exits; installing
                // again replaces the add-on, so this is safe either way.
                if let (true, Some(xpi)) = (rec.adblock, &config.firefox_adblock) {
                    let _ = hub
                        .call(
                            "webExtension.install",
                            serde_json::json!({
                                "extensionData": { "type": "archivePath", "path": xpi }
                            }),
                        )
                        .await;
                }
                Some(hub)
            }
        };
        if !proc.alive() {
            return Err("browser process died while adopting it".into());
        }
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(rec.created_at_secs);
        Ok(Self {
            id: rec.id,
            window_size: rec.window_size,
            engine: rec.engine,
            port: rec.port,
            ws_path: rec.ws_path.clone(),
            proc,
            bidi,
            temp_dir: rec.temp_dir.clone(),
            firefox_log: rec.firefox_log.clone(),
            profile_name: rec.profile.clone(),
            profile_id: rec.profile_id,
            created_at: Instant::now()
                .checked_sub(Duration::from_secs(age))
                .unwrap_or_else(Instant::now),
            adblock: rec.adblock,
            relay,
            audio,
            display,
        })
    }
}

impl Drop for ActiveBrowser {
    fn drop(&mut self) {
        // The processes are killed by their `Proc`s.
        self.cleanup_files();
    }
}

/// Session state kept outside the browser, saved with it.
pub struct SessionState {
    pub name: String,
    pub persistent: bool,
    pub color_scheme: ColorScheme,
    pub screen: crate::ScreenMode,
    pub show_touches: Option<bool>,
    pub idle_timeout_secs: u64,
    pub shared_display: Option<String>,
    pub video: bool,
    pub recording: bool,
    pub recording_audio: bool,
}

/// The profile's `Preferences` with the password manager and autofill switched
/// off, notifications blocked, and the last run marked clean (sessions end by being killed, which would
/// offer to restore pages); everything else Chromium stored there is kept.
fn chromium_preferences(existing: &str) -> String {
    let mut prefs: serde_json::Value = serde_json::from_str(existing)
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    for (path, value) in [
        ("credentials_enable_service", serde_json::Value::Bool(false)),
        ("credentials_enable_autosignin", false.into()),
        ("profile.password_manager_enabled", false.into()),
        ("autofill.profile_enabled", false.into()),
        ("autofill.credit_card_enabled", false.into()),
        ("profile.exit_type", "Normal".into()),
        ("profile.exited_cleanly", true.into()),
        // No "Translate this page?" bubble: browser UI the live view can't reach.
        ("translate.enabled", false.into()),
        // 2 = block: sites can't ask to show notifications.
        (
            "profile.default_content_setting_values.notifications",
            2.into(),
        ),
        // 2 = block: no "know your location" prompt either.
        (
            "profile.default_content_setting_values.geolocation",
            2.into(),
        ),
    ] {
        let mut node = &mut prefs;
        let mut keys = path.split('.').peekable();
        while let Some(key) = keys.next() {
            if !node[key].is_object() && keys.peek().is_some() {
                node[key] = serde_json::json!({});
            }
            if keys.peek().is_none() {
                node[key] = value.clone();
            } else {
                node = &mut node[key];
            }
        }
    }
    prefs.to_string()
}

async fn wait_for_devtools_port(
    user_data_dir: &Path,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let port_file = user_data_dir.join("DevToolsActivePort");
    let start = Instant::now();
    let timeout = Duration::from_secs(12);

    while start.elapsed() < timeout {
        if fs::try_exists(&port_file).await.unwrap_or(false) {
            if let Ok(content) = fs::read_to_string(&port_file).await {
                let lines: Vec<&str> = content.lines().collect();
                if lines.len() >= 2 {
                    let port = lines[0].trim().parse::<u16>()?;
                    let ws_path = lines[1].trim().to_string();
                    if port > 0 {
                        return Ok((port, ws_path));
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Err(format!(
        "Timed out waiting for DevToolsActivePort in {}",
        user_data_dir.display()
    )
    .into())
}

async fn wait_for_firefox_bidi(
    log: &Path,
    timeout: Duration,
) -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(text) = fs::read_to_string(log).await {
            if let Some(port) = bidi_port(&text) {
                return Ok(port);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("Timed out waiting for Firefox WebDriver BiDi port on stderr".into())
}

/// The port from "WebDriver BiDi listening on ws://127.0.0.1:<port>" (or
/// "ws://localhost:<port>", e.g. with a proxy set) in Firefox's output.
fn bidi_port(output: &str) -> Option<u16> {
    const MARKER: &str = "WebDriver BiDi listening on ws://";
    output.lines().find_map(|line| {
        let pos = line.find(MARKER)?;
        let authority = line[pos + MARKER.len()..]
            .split(|c: char| c == '/' || c.is_whitespace())
            .next()?;
        authority
            .rsplit(':')
            .next()?
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firefox_bidi_port_is_read_from_its_output() {
        assert_eq!(
            bidi_port("noise\nWebDriver BiDi listening on ws://127.0.0.1:40123\n"),
            Some(40123)
        );
        assert_eq!(
            bidi_port("WebDriver BiDi listening on ws://localhost:9222/ x"),
            Some(9222)
        );
        assert_eq!(bidi_port("WebDriver BiDi listening on ws://x:0"), None);
        assert_eq!(bidi_port("nothing yet"), None);
    }

    #[test]
    fn chromium_preferences_disable_password_manager() {
        let out: serde_json::Value = serde_json::from_str(&chromium_preferences(
            r#"{"profile":{"name":"x","password_manager_enabled":true},"autofill":7}"#,
        ))
        .unwrap();
        assert_eq!(out["profile"]["name"], "x");
        assert_eq!(out["profile"]["password_manager_enabled"], false);
        assert_eq!(out["credentials_enable_service"], false);
        assert_eq!(out["autofill"]["credit_card_enabled"], false);
        assert_eq!(out["profile"]["exit_type"], "Normal");
        assert_eq!(out["translate"]["enabled"], false);
        assert_eq!(
            out["profile"]["default_content_setting_values"]["notifications"],
            2
        );
        assert_eq!(
            out["profile"]["default_content_setting_values"]["geolocation"],
            2
        );
        let fresh: serde_json::Value = serde_json::from_str(&chromium_preferences("")).unwrap();
        assert_eq!(fresh["autofill"]["profile_enabled"], false);
    }
}
