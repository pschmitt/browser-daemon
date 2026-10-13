use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::audio::{AudioConfig, Sink};
use crate::bidi::BidiHub;
use crate::display::{DisplayConfig, VirtualDisplay};
use crate::profiles::Profile;
use crate::proxies::{self, Proxy};
use crate::relay::Relay;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserEngine {
    #[serde(alias = "Chromium", alias = "CHROMIUM")]
    Chromium,
    #[serde(alias = "Firefox", alias = "FIREFOX")]
    Firefox,
}

impl Default for BrowserEngine {
    fn default() -> Self {
        BrowserEngine::Chromium
    }
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
}

impl BrowserConfig {
    pub fn adblock_available(&self, engine: BrowserEngine) -> bool {
        match engine {
            BrowserEngine::Chromium => self.chromium_adblock.is_some(),
            BrowserEngine::Firefox => self.firefox_adblock.is_some(),
        }
    }
}

pub struct ActiveBrowser {
    pub id: Uuid,
    pub engine: BrowserEngine,
    pub port: u16,
    pub ws_path: String,
    pub child: Child,
    /// Shared WebDriver BiDi session (Firefox only).
    pub bidi: Option<Arc<BidiHub>>,
    _temp_dir: Option<tempfile::TempDir>,
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
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let id = Uuid::new_v4();
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
                    .arg("--disable-gpu")
                    .arg("--disable-background-timer-throttling")
                    .arg("--disable-backgrounding-occluded-windows")
                    .arg("--disable-renderer-backgrounding")
                    .arg("--force-color-profile=srgb")
                    .arg("--password-store=basic")
                    .arg("about:blank")
                    .kill_on_drop(true);

                if let (true, Some(ext)) = (adblock, &config.chromium_adblock) {
                    // Unbranded Chromium still honours --load-extension; the
                    // feature guard keeps it that way should it become default.
                    cmd.arg(format!("--load-extension={}", ext.display()))
                        .arg("--disable-features=DisableLoadExtensionCommandLineSwitch");
                }

                cmd.args(proxies::chromium_args(relay.port()));

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
                let child = cmd.spawn()?;
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
                    child,
                    bidi: None,
                    _temp_dir: temp_dir,
                    profile_name,
                    profile_id,
                    created_at: Instant::now(),
                    adblock,
                    relay: relay.clone(),
                    audio,
                    display: session_display,
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
                // Proxy prefs are rewritten on every launch: the relay's port changes.
                let user_js = user_data_dir.join("user.js");
                let existing = fs::read_to_string(&user_js).await.unwrap_or_default();
                let prefs = proxies::firefox_prefs(relay.port());
                fs::write(&user_js, proxies::firefox_user_js(&existing, &prefs)).await?;

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
                    .arg("about:blank")
                    .env("MOZ_ENABLE_WAYLAND", "0")
                    .stderr(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .kill_on_drop(true);

                if let Some(ref sink) = audio {
                    cmd.envs(sink.env());
                }
                if let Some(ref display) = display_name {
                    cmd.env("DISPLAY", display);
                }

                let mut child = cmd.spawn()?;
                let stderr = child
                    .stderr
                    .take()
                    .ok_or("Failed to capture Firefox stderr")?;

                let port = match wait_for_firefox_bidi(stderr, Duration::from_secs(15)).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Firefox failed to initialize WebDriver BiDi");
                        let _ = child.start_kill();
                        return Err(e);
                    }
                };

                let hub = match BidiHub::connect(port).await {
                    Ok(h) => h,
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Failed to open Firefox BiDi session");
                        let _ = child.start_kill();
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
                    child,
                    bidi: Some(hub),
                    _temp_dir: temp_dir,
                    profile_name,
                    profile_id,
                    created_at: Instant::now(),
                    adblock,
                    relay: relay.clone(),
                    audio,
                    display: session_display,
                })
            }
        }
    }

    pub async fn terminate(&mut self) {
        info!(session_id = %self.id, engine = %self.engine, "Terminating browser instance");
        if let Err(e) = self.child.start_kill() {
            warn!(session_id = %self.id, error = %e, "Failed to send kill to child process");
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        if let Some(ref mut display) = self.display {
            display.stop().await;
        }
    }
}

async fn wait_for_devtools_port(
    user_data_dir: &Path,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let port_file = user_data_dir.join("DevToolsActivePort");
    let start = Instant::now();
    let timeout = Duration::from_secs(12);

    while start.elapsed() < timeout {
        if port_file.exists() {
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
    stderr: tokio::process::ChildStderr,
    timeout_duration: Duration,
) -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncBufReadExt;

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut tx = Some(tx);
        while let Ok(Some(line)) = reader.next_line().await {
            if tx.is_some() {
                // "ws://127.0.0.1:<port>", or "ws://localhost:<port>" (e.g. with a proxy set).
                const MARKER: &str = "WebDriver BiDi listening on ws://";
                if let Some(pos) = line.find(MARKER) {
                    let authority = line[pos + MARKER.len()..]
                        .split(|c: char| c == '/' || c.is_whitespace())
                        .next()
                        .unwrap_or("");
                    let port_str = authority.rsplit(':').next().unwrap_or("");
                    if let Ok(port) = port_str.parse::<u16>() {
                        if port > 0 {
                            if let Some(c) = tx.take() {
                                let _ = c.send(port);
                            }
                        }
                    }
                }
            }
        }
    });

    match tokio::time::timeout(timeout_duration, rx).await {
        Ok(Ok(port)) => Ok(port),
        Ok(Err(_)) => Err("Firefox process closed stderr without advertising BiDi port".into()),
        Err(_) => Err("Timed out waiting for Firefox WebDriver BiDi port on stderr".into()),
    }
}
