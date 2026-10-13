//! Session audio: every browser plays into its own PulseAudio null sink, and
//! `/audio?session_id=…` streams that sink's monitor as Opus in WebM, encoded
//! by ffmpeg, one encoder per listener.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::{Child, Command};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct AudioConfig {
    /// PulseAudio's native socket.
    pub server: PathBuf,
    pub pactl: PathBuf,
    pub ffmpeg: PathBuf,
}

impl AudioConfig {
    fn server_arg(&self) -> String {
        format!("unix:{}", self.server.display())
    }
}

/// A session's null sink, removed with [`Sink::remove`].
#[derive(Debug)]
pub struct Sink {
    pub name: String,
    module: u32,
    cfg: AudioConfig,
}

impl Sink {
    pub async fn create(cfg: &AudioConfig, session: Uuid) -> std::io::Result<Self> {
        let name = format!("browserd_{}", session.simple());
        let out = Command::new(&cfg.pactl)
            .arg("--server")
            .arg(cfg.server_arg())
            .arg("load-module")
            .arg("module-null-sink")
            .arg(format!("sink_name={name}"))
            .arg(format!(
                "sink_properties=device.description=browser-daemon-{}",
                session.simple()
            ))
            .output()
            .await?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "pactl load-module: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let module = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|_| std::io::Error::other("pactl did not return a module index"))?;
        info!(session_id = %session, sink = %name, "Created audio sink");
        Ok(Self {
            name,
            module,
            cfg: cfg.clone(),
        })
    }

    /// Environment that sends a browser's audio into this sink.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("PULSE_SERVER", self.cfg.server_arg()),
            ("PULSE_SINK", self.name.clone()),
        ]
    }
}

/// Unloaded whenever the browser goes away, including a failed launch.
impl Drop for Sink {
    fn drop(&mut self) {
        let res = std::process::Command::new(&self.cfg.pactl)
            .arg("--server")
            .arg(self.cfg.server_arg())
            .arg("unload-module")
            .arg(self.module.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !res.is_ok_and(|s| s.success()) {
            warn!(sink = %self.name, "Could not remove audio sink");
        }
    }
}

/// Remove per-session sinks a previous run left behind (it was killed, so their
/// Drop never ran). Called at startup, before any session exists.
pub async fn remove_stale_sinks(cfg: &AudioConfig) {
    let Ok(out) = Command::new(&cfg.pactl)
        .arg("--server")
        .arg(cfg.server_arg())
        .args(["list", "short", "modules"])
        .output()
        .await
    else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut cols = line.split('\t');
        let (Some(index), Some("module-null-sink"), Some(args)) =
            (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if !args.contains("sink_name=browserd_") || args.contains("sink_name=browserd_idle") {
            continue;
        }
        let _ = Command::new(&cfg.pactl)
            .arg("--server")
            .arg(cfg.server_arg())
            .args(["unload-module", index])
            .output()
            .await;
        info!(module = %index, "Removed a stale audio sink");
    }
}

/// Sinks with a stream playing into them right now (not corked, not muted).
/// The sink's own state would not do: a listener on its monitor keeps it running.
pub async fn audible_sinks(cfg: &AudioConfig) -> Option<HashSet<String>> {
    let list = |what: &'static str| async move {
        let out = Command::new(&cfg.pactl)
            .arg("--server")
            .arg(cfg.server_arg())
            .args(["-f", "json", "list", what])
            .output()
            .await
            .ok()?;
        serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout).ok()
    };
    let sinks: HashMap<u64, String> = list("sinks")
        .await?
        .iter()
        .filter_map(|s| Some((s["index"].as_u64()?, s["name"].as_str()?.to_string())))
        .collect();
    Some(
        list("sink-inputs")
            .await?
            .iter()
            .filter(|i| i["corked"] == false && i["mute"] == false)
            .filter_map(|i| sinks.get(&i["sink"].as_u64()?).cloned())
            .collect(),
    )
}

/// An ffmpeg encoding `sink`'s monitor to Opus in WebM on stdout, tuned for
/// low latency: small clusters, 20 ms frames, no input buffering.
pub fn encoder(cfg: &AudioConfig, sink: &str) -> std::io::Result<Child> {
    Command::new(&cfg.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-fflags", "nobuffer", "-f", "pulse"])
        .arg("-server")
        .arg(cfg.server_arg())
        // 20 ms of 48 kHz stereo s16 per read, matching the Opus frames.
        .args([
            "-fragment_size",
            "3840",
            "-sample_rate",
            "48000",
            "-channels",
            "2",
        ])
        .arg("-i")
        .arg(format!("{sink}.monitor"))
        .args(["-c:a", "libopus", "-b:a", "96k", "-application", "lowdelay"])
        .args(["-frame_duration", "20"])
        .args(["-f", "webm", "-live", "1", "-cluster_time_limit", "100"])
        .args([
            "-cluster_size_limit",
            "8192",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}
