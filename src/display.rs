//! One virtual X display (Xvfb) per session, as an alternative to a single shared
//! one: each browser window then has a screen of its own, which can be captured
//! on its own (streamed as video) without other sessions' windows on top.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::proc::{self, Proc};
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
/// Smallest screen: room for the handoff view's Native mode, which resizes the
/// window to the viewer's screen (and a common monitor size for pages to see).
const MIN_SCREEN: (u32, u32) = (3840, 2160);

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
    proc: Proc,
}

/// Lock file of a display whose server is gone (killed without cleaning up)?
fn stale_lock(n: u32) -> bool {
    lock_pid(n).is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists())
}

/// The pid in a display's lock file.
fn lock_pid(n: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/tmp/.X{n}-lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Display numbers are picked one launch at a time: two launches at once would
/// both see the same number free.
static ALLOCATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn in_use(n: u32) -> bool {
    Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

impl VirtualDisplay {
    /// Start an Xvfb for a `width`x`height` window on the first free display.
    pub async fn start(cfg: &DisplayConfig, width: u32, height: u32) -> std::io::Result<Self> {
        let (sw, sh) = (
            (width + MARGIN).max(MIN_SCREEN.0),
            (height + MARGIN).max(MIN_SCREEN.1),
        );
        let _turn = ALLOCATE.lock().await;
        for n in FIRST_DISPLAY..=LAST_DISPLAY {
            if stale_lock(n) {
                debug!(display = n, "Removing a stale X lock");
                let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
                let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
            }
            if in_use(n) {
                continue;
            }
            // Outlives the daemon, like the browser on it.
            let mut cmd = Command::new(&cfg.xvfb);
            cmd.arg(format!(":{n}"))
                .args(["-screen", "0", &format!("{sw}x{sh}x24")])
                .args(["-nolisten", "tcp", "-noreset"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            proc::own_group(&mut cmd);
            let mut child = cmd.spawn()?;
            let pid = child.id();
            // Ready once its socket exists and the lock is ours; gone means another
            // server (not ours to clean up after) won the race.
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
            let mut ready = false;
            for _ in 0..60 {
                if child.try_wait()?.is_some() {
                    break;
                }
                if socket.exists() && pid.is_some() && lock_pid(n) == pid {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !ready {
                let _ = child.start_kill();
                let _ = child.wait().await;
                // Killed, it cannot remove what it made; a server that won the number is not ours.
                if pid.is_some() && lock_pid(n) == pid {
                    let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
                    let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
                }
                continue;
            }
            let Some(proc) = Proc::spawned(child) else {
                continue;
            };
            let vd = Self {
                name: format!(":{n}"),
                width: sw,
                height: sh,
                proc,
            };
            vd.park_pointer(cfg).await;
            info!(display = %vd.name, size = %format!("{sw}x{sh}"), "Started session display");
            return Ok(vd);
        }
        Err(std::io::Error::other("no free X display number"))
    }

    /// A display of an earlier daemon run, if its server is still there.
    pub fn adopt(name: &str, width: u32, height: u32, pid: u32, start: u64) -> Option<Self> {
        Some(Self {
            name: name.to_string(),
            width,
            height,
            proc: Proc::adopt(pid, start)?,
        })
    }

    /// Pid and start time of the server, to find it again after a restart.
    pub fn identity(&self) -> (u32, u64) {
        (self.proc.pid, self.proc.start)
    }

    async fn park_pointer(&self, cfg: &DisplayConfig) {
        let Some(ref xdotool) = cfg.xdotool else {
            return;
        };
        if !park(xdotool, &self.name, (self.width, self.height)).await {
            warn!(display = %self.name, "Could not park the X pointer");
        }
    }

    pub async fn stop(&mut self) {
        self.proc.terminate(Duration::from_secs(2)).await;
        // Killed, the server cannot remove its lock and socket itself; another
        // server's (the number was taken over) are left alone.
        let Ok(n) = self.name.trim_start_matches(':').parse::<u32>() else {
            return;
        };
        if lock_pid(n).is_none_or(|pid| pid == self.proc.pid) {
            let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
        }
    }
}

/// Move the X pointer into the screen's margin, off the window.
async fn park(xdotool: &Path, display: &str, (width, height): (u32, u32)) -> bool {
    xdotool_run(
        xdotool,
        display,
        &[
            "mousemove".into(),
            (width - 1).to_string(),
            (height - 1).to_string(),
        ],
    )
    .await
    .is_some()
}

async fn xdotool_run(xdotool: &Path, display: &str, args: &[String]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(xdotool)
            .env("DISPLAY", display)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A native popup window on a session's display: the browser's context menu, a
/// `<select>` list, ... Input sent over CDP or BiDi goes to the page and passes
/// these by, so a view has to send its input to the display while one is open.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Popup {
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
}

/// The popups open on `display` (`screen` is its size): visible windows well
/// smaller than the browser's window (the largest one below the root). Titles
/// don't tell: Chromium's menus have none, Firefox's are called "Firefox", and
/// Firefox's main window has an untitled child as large as itself.
pub async fn popups(xdotool: &Path, display: &str, screen: (u32, u32)) -> Vec<Popup> {
    let args = ["search", "--onlyvisible", "--name", ""].map(String::from);
    let Some(ids) = xdotool_run(xdotool, display, &args).await else {
        return Vec::new(); // no match: xdotool exits 1
    };
    let mut windows = Vec::new();
    for id in ids.split_whitespace() {
        let geometry = xdotool_run(
            xdotool,
            display,
            &["getwindowgeometry".into(), "--shell".into(), id.into()],
        )
        .await;
        if let Some(p) = geometry.as_deref().and_then(parse_geometry) {
            windows.push(p);
        }
    }
    pick_popups(windows, screen)
}

fn pick_popups(windows: Vec<Popup>, screen: (u32, u32)) -> Vec<Popup> {
    let area = |p: &Popup| u64::from(p.w) * u64::from(p.h);
    let windows: Vec<Popup> = windows
        .into_iter()
        .filter(|p| (p.w, p.h) != screen && p.w > 1 && p.h > 1)
        .collect();
    let Some(main) = windows.iter().map(area).max() else {
        return Vec::new();
    };
    let mut out: Vec<Popup> = Vec::new();
    for p in windows {
        if area(&p) * 2 < main && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// `getwindowgeometry --shell` output: WINDOW=, X=, Y=, WIDTH=, HEIGHT=, SCREEN=.
fn parse_geometry(text: &str) -> Option<Popup> {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    Some(Popup {
        x: get("X")?.parse().ok()?,
        y: get("Y")?.parse().ok()?,
        w: get("WIDTH")?.parse().ok()?,
        h: get("HEIGHT")?.parse().ok()?,
    })
}

/// Input for a session's display (screen coordinates), sent while a popup is open.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum XInput {
    Move {
        x: u32,
        y: u32,
    },
    Down {
        x: u32,
        y: u32,
        button: u8,
    },
    Up {
        x: u32,
        y: u32,
        button: u8,
    },
    /// A DOM key name (Escape, Enter, ArrowDown, ...) or a letter or digit.
    Key {
        key: String,
    },
    /// Pointer back into the margin, once the popup is gone.
    Park,
}

/// The X keysym for a DOM key name; None for keys a popup has no use for.
fn keysym(key: &str) -> Option<String> {
    let named = match key {
        "Escape" => "Escape",
        "Enter" => "Return",
        "Tab" => "Tab",
        "Backspace" => "BackSpace",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        " " => "space",
        _ => {
            let mut chars = key.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_string()),
                _ => None,
            };
        }
    };
    Some(named.into())
}

/// The xdotool arguments for `input` on a `screen`-sized display.
fn input_args(input: &XInput, screen: (u32, u32)) -> Option<Vec<String>> {
    let inside = |x: u32, y: u32| x < screen.0 && y < screen.1;
    let button = |b: u8| match b {
        0 => Some("1"),
        1 => Some("2"),
        2 => Some("3"),
        _ => None,
    };
    let at = |x: u32, y: u32| vec!["mousemove".into(), x.to_string(), y.to_string()];
    Some(match *input {
        XInput::Move { x, y } if inside(x, y) => at(x, y),
        XInput::Down { x, y, button: b } if inside(x, y) => {
            let mut a = at(x, y);
            a.extend(["mousedown".into(), button(b)?.into()]);
            a
        }
        XInput::Up { x, y, button: b } if inside(x, y) => {
            let mut a = at(x, y);
            a.extend(["mouseup".into(), button(b)?.into()]);
            a
        }
        XInput::Key { ref key } => vec!["key".into(), keysym(key)?],
        XInput::Park => at(screen.0 - 1, screen.1 - 1),
        _ => return None,
    })
}

/// Send `input` to `display`; false when it was refused or failed.
pub async fn send_input(xdotool: &Path, display: &str, screen: (u32, u32), input: &XInput) -> bool {
    let Some(args) = input_args(input, screen) else {
        return false;
    };
    if matches!(input, XInput::Down { .. }) {
        pin_focus(xdotool, display, screen).await;
    }
    xdotool_run(xdotool, display, &args).await.is_some()
}

/// Give the keyboard focus to the browser's window (the largest titled one).
/// Without a window manager the focus follows the pointer (PointerRoot): moving
/// onto a menu took it from Firefox, which closes its popups when it loses focus.
async fn pin_focus(xdotool: &Path, display: &str, screen: (u32, u32)) {
    let args = ["search", "--onlyvisible", "--name", "."].map(String::from);
    let Some(ids) = xdotool_run(xdotool, display, &args).await else {
        return;
    };
    let mut best: Option<(u64, &str)> = None;
    for id in ids.split_whitespace() {
        let geometry = xdotool_run(
            xdotool,
            display,
            &["getwindowgeometry".into(), "--shell".into(), id.into()],
        )
        .await;
        let Some(p) = geometry.as_deref().and_then(parse_geometry) else {
            continue;
        };
        let area = u64::from(p.w) * u64::from(p.h);
        if (p.w, p.h) != screen && best.is_none_or(|(a, _)| area > a) {
            best = Some((area, id));
        }
    }
    if let Some((_, id)) = best {
        xdotool_run(xdotool, display, &["windowfocus".into(), id.into()]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_from_xdotool() {
        let text = "WINDOW=2097164\nX=118\nY=195\nWIDTH=297\nHEIGHT=349\nSCREEN=0\n";
        assert_eq!(
            parse_geometry(text),
            Some(Popup {
                x: 118,
                y: 195,
                w: 297,
                h: 349
            })
        );
        assert_eq!(parse_geometry("WINDOW=1\n"), None);
    }

    #[test]
    fn popups_by_size() {
        let p = |x, y, w, h| Popup { x, y, w, h };
        let screen = (3840, 2160);
        // Firefox: root, main window and its child, a menu (titled and its child), 1x1s.
        let firefox = vec![
            p(0, 0, 3840, 2160),
            p(4, 4, 1655, 1034),
            p(502, 402, 195, 216),
            p(3, 3, 1, 1),
            p(4, 4, 1655, 1034),
            p(502, 402, 195, 216),
        ];
        assert_eq!(pick_popups(firefox, screen), vec![p(502, 402, 195, 216)]);
        // Chromium without a menu: nothing.
        assert_eq!(
            pick_popups(vec![p(0, 0, 3840, 2160), p(0, 0, 1592, 1361)], screen),
            vec![]
        );
        assert_eq!(pick_popups(vec![p(0, 0, 3840, 2160)], screen), vec![]);
    }

    #[test]
    fn input_is_checked() {
        let screen = (3840, 2160);
        assert_eq!(
            input_args(
                &XInput::Down {
                    x: 10,
                    y: 20,
                    button: 2
                },
                screen
            ),
            Some(
                vec!["mousemove", "10", "20", "mousedown", "3"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert_eq!(input_args(&XInput::Move { x: 4000, y: 1 }, screen), None);
        assert_eq!(
            input_args(
                &XInput::Up {
                    x: 1,
                    y: 1,
                    button: 7
                },
                screen
            ),
            None
        );
        assert_eq!(
            input_args(
                &XInput::Key {
                    key: "ArrowDown".into()
                },
                screen
            ),
            Some(vec!["key".into(), "Down".into()])
        );
        assert_eq!(
            input_args(
                &XInput::Key {
                    key: "ctrl+alt+Delete".into()
                },
                screen
            ),
            None
        );
        assert_eq!(
            input_args(
                &XInput::Key {
                    key: "--help".into()
                },
                screen
            ),
            None
        );
    }
}
