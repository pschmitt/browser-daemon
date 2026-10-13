//! One virtual X display (Xvfb) per session, as an alternative to a single shared
//! one: each browser window then has a screen of its own, which can be captured
//! on its own (streamed as video) without other sessions' windows on top.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// Display numbers handed out to sessions; the shared display (:98 by default)
/// stays below.
const FIRST_DISPLAY: u32 = 100;
const LAST_DISPLAY: u32 = 399;
/// The screen is this much larger than the window, and the X pointer is parked in
/// the margin: headful Chromium reports the real pointer as a mouse move on every
/// press, which over the page would cancel drags (text selection). Firefox's outer
/// window is ~55x34 px larger than the size it is given.
const MARGIN: u32 = 64;

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub xvfb: PathBuf,
    pub xdotool: Option<PathBuf>,
}

pub struct VirtualDisplay {
    /// `:N`, for DISPLAY.
    pub name: String,
    pub width: u32,
    pub height: u32,
    child: Child,
}

/// Lock file of a display whose server is gone (killed without cleaning up)?
fn stale_lock(n: u32) -> bool {
    let Ok(pid) = std::fs::read_to_string(format!("/tmp/.X{n}-lock")) else {
        return false;
    };
    match pid.trim().parse::<u32>() {
        Ok(pid) => !Path::new(&format!("/proc/{pid}")).exists(),
        Err(_) => false,
    }
}

fn in_use(n: u32) -> bool {
    Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

impl VirtualDisplay {
    /// Start an Xvfb for a `width`x`height` window on the first free display.
    pub async fn start(cfg: &DisplayConfig, width: u32, height: u32) -> std::io::Result<Self> {
        let (sw, sh) = (width + MARGIN, height + MARGIN);
        for n in FIRST_DISPLAY..=LAST_DISPLAY {
            if stale_lock(n) {
                debug!(display = n, "Removing a stale X lock");
                let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
                let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
            }
            if in_use(n) {
                continue;
            }
            let mut child = Command::new(&cfg.xvfb)
                .arg(format!(":{n}"))
                .args(["-screen", "0", &format!("{sw}x{sh}x24")])
                .args(["-nolisten", "tcp", "-noreset"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            // Ready once its socket exists; gone again means another server won the race.
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
            let mut ready = false;
            for _ in 0..60 {
                if socket.exists() {
                    ready = true;
                    break;
                }
                if child.try_wait()?.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !ready {
                let _ = child.start_kill();
                continue;
            }
            let vd = Self {
                name: format!(":{n}"),
                width: sw,
                height: sh,
                child,
            };
            vd.park_pointer(cfg).await;
            info!(display = %vd.name, size = %format!("{sw}x{sh}"), "Started session display");
            return Ok(vd);
        }
        Err(std::io::Error::other("no free X display number"))
    }

    async fn park_pointer(&self, cfg: &DisplayConfig) {
        let Some(ref xdotool) = cfg.xdotool else {
            return;
        };
        let ok = Command::new(xdotool)
            .env("DISPLAY", &self.name)
            .args([
                "mousemove",
                &(self.width - 1).to_string(),
                &(self.height - 1).to_string(),
            ])
            .output()
            .await
            .is_ok_and(|o| o.status.success());
        if !ok {
            warn!(display = %self.name, "Could not park the X pointer");
        }
    }

    pub async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
        // Killed, the server cannot remove its lock and socket itself.
        let n = self.name.trim_start_matches(':');
        let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
        let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
    }
}
