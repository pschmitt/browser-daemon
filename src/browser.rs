use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct BrowserConfig {
    pub chromium_bin: PathBuf,
    pub display: Option<String>,
    pub profiles_dir: PathBuf,
    pub default_width: u32,
    pub default_height: u32,
}

pub struct ActiveBrowser {
    pub id: Uuid,
    pub port: u16,
    pub ws_path: String,
    pub child: Child,
    _temp_dir: Option<tempfile::TempDir>,
    pub profile_name: Option<String>,
    pub created_at: Instant,
}

impl ActiveBrowser {
    pub async fn spawn(
        config: &BrowserConfig,
        profile: Option<&str>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let id = Uuid::new_v4();
        let width = width.unwrap_or(config.default_width);
        let height = height.unwrap_or(config.default_height);

        let (user_data_dir, temp_dir, profile_name) = match profile {
            Some(name) if !name.trim().is_empty() => {
                let sanitized: String = name
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                let dir = config.profiles_dir.join(&sanitized);
                fs::create_dir_all(&dir).await?;
                (dir, None, Some(sanitized))
            }
            _ => {
                let temp = tempfile::Builder::new()
                    .prefix(&format!("browserd-{}-", id))
                    .tempdir()?;
                let path = temp.path().to_path_buf();
                (path, Some(temp), None)
            }
        };

        info!(
            session_id = %id,
            profile = ?profile_name,
            dir = %user_data_dir.display(),
            "Launching Chromium"
        );

        let mut cmd = Command::new(&config.chromium_bin);
        cmd.arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--disable-blink-features=AutomationControlled")
            .arg(format!("--window-size={},{}", width, height))
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

        if let Some(ref display) = config.display {
            cmd.env("DISPLAY", display);
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
            port,
            ws_path,
            child,
            _temp_dir: temp_dir,
            profile_name,
            created_at: Instant::now(),
        })
    }

    pub async fn terminate(&mut self) {
        info!(session_id = %self.id, "Terminating Chromium instance");
        if let Err(e) = self.child.start_kill() {
            warn!(session_id = %self.id, error = %e, "Failed to send kill to child process");
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
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
