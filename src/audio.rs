//! Session audio: every browser plays into its own PulseAudio null sink, and
//! `/audio?session_id=…` streams that sink's monitor as Opus in WebM, encoded
//! by ffmpeg, one encoder per listener.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
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
    pub fn server_arg(&self) -> String {
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

    /// The sink of an earlier daemon run, by name and module index.
    pub fn adopt(cfg: &AudioConfig, name: String, module: u32) -> Self {
        Self {
            name,
            module,
            cfg: cfg.clone(),
        }
    }

    /// PulseAudio module index, to unload it later.
    pub fn module(&self) -> u32 {
        self.module
    }

    /// Environment that sends a browser's audio into this sink.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("PULSE_SERVER", self.cfg.server_arg()),
            ("PULSE_SINK", self.name.clone()),
        ]
    }
}

/// Unloaded whenever the browser goes away, including a failed launch. pactl
/// takes a moment: off the async workers, which may be the ones dropping this.
impl Drop for Sink {
    fn drop(&mut self) {
        let (cfg, module, name) = (self.cfg.clone(), self.module, self.name.clone());
        let unload = move || {
            let res = std::process::Command::new(&cfg.pactl)
                .arg("--server")
                .arg(cfg.server_arg())
                .arg("unload-module")
                .arg(module.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !res.is_ok_and(|s| s.success()) {
                warn!(sink = %name, "Could not remove audio sink");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(unload);
            }
            Err(_) => unload(),
        }
    }
}

/// Remove per-session sinks a previous run left behind (it was killed, so their
/// Drop never ran). Called at startup, before any session exists.
pub async fn remove_stale_sinks(cfg: &AudioConfig, keep: &HashSet<String>) {
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
        // A session adopted after a daemon restart still plays into its sink.
        if keep
            .iter()
            .any(|name| args.contains(&format!("sink_name={name}")))
        {
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

/// Sinks actually making sound: a stream plays into them (not corked, not
/// muted), and a short sample of their monitor is not silence. A stream alone
/// would not do: Chromium keeps one open, playing silence, after a pause (and so
/// does a page holding an idle AudioContext). Neither would the sink's state: a
/// listener on its monitor keeps it running.
pub async fn audible_sinks(cfg: &AudioConfig) -> Option<HashSet<String>> {
    let mut out = HashSet::new();
    for sink in playing_sinks(cfg).await? {
        if !silent(cfg, &sink).await {
            out.insert(sink);
        }
    }
    Some(out)
}

/// Peak of 0.2 s of `sink`'s monitor below about -50 dBFS.
async fn silent(cfg: &AudioConfig, sink: &str) -> bool {
    const SAMPLE: std::time::Duration = std::time::Duration::from_millis(200);
    const THRESHOLD: i16 = 100;
    let parec = cfg.pactl.with_file_name("parec");
    let Ok(mut child) = Command::new(parec)
        .arg("--server")
        .arg(cfg.server_arg())
        .arg(format!("--device={sink}.monitor"))
        .args(["--format=s16le", "--channels=1", "--rate=8000", "--raw"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return false;
    };
    let mut pcm = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = tokio::time::timeout(SAMPLE, stdout.read_to_end(&mut pcm)).await;
    }
    let _ = child.kill().await;
    !pcm.as_chunks::<2>()
        .0
        .iter()
        .any(|s| i16::from_le_bytes(*s).saturating_abs() > THRESHOLD)
}

/// Sinks with a stream playing into them (not corked, not muted).
async fn playing_sinks(cfg: &AudioConfig) -> Option<HashSet<String>> {
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
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
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
        .kill_on_drop(true);
    crate::proc::die_with_parent(&mut cmd);
    cmd.spawn()
}
