//! A recording's sound: the session's sink monitor recorded with ffmpeg as Opus,
//! one segment per stretch between pauses (named by where it starts on the
//! recording's clock, which stands still while paused), joined into
//! `audio.webm` when the recording ends. A recording that stayed silent keeps
//! no audio. Merged recordings join their sources' audio the same way.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::audio::AudioConfig;
use crate::recorder::RecordingClock;

/// The joined audio of a recording, next to its `frames.jsonl`.
pub const FILE: &str = "audio.webm";
/// Segments while recording.
const PARTS: &str = "audio-parts";
/// Louder than this somewhere (dBFS), or the audio is dropped as silence.
const SILENCE_DB: f64 = -60.0;

/// Where a recording's sound comes from.
pub struct Source {
    pub cfg: AudioConfig,
    pub sink: String,
}

/// Records `src` into `dir` until `stop` is set, following the clock's pauses.
/// The task answers whether the recording kept audio.
pub fn spawn(
    src: Source,
    dir: PathBuf,
    clock: Arc<RecordingClock>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<bool> {
    tokio::spawn(async move {
        let parts = dir.join(PARTS);
        if let Err(e) = tokio::fs::create_dir_all(&parts).await {
            warn!(error = %e, "Could not create the audio directory: recording without sound");
            return false;
        }
        let mut current: Option<(Child, Instant)> = None;
        let mut segments: Vec<(u64, PathBuf)> = Vec::new();
        let mut early_deaths = 0;
        loop {
            let stopping = stop.load(Ordering::SeqCst);
            if stopping || clock.is_paused() {
                if let Some((child, _)) = current.take() {
                    finish(child).await;
                }
                if stopping {
                    break;
                }
            } else if let Some((child, since)) = current.as_mut() {
                // ffmpeg went away (the sink was removed?): a new segment on the next
                // round, unless it keeps dying right away.
                if let Ok(Some(_)) = child.try_wait() {
                    if since.elapsed() < Duration::from_secs(2) {
                        early_deaths += 1;
                    }
                    current = None;
                    if early_deaths >= 3 {
                        warn!(sink = %src.sink, "ffmpeg keeps failing: recording without sound from here");
                        break;
                    }
                }
            } else {
                let t = clock.elapsed().as_millis() as u64;
                let path = parts.join(format!("{t:010}.webm"));
                match record(&src, &path) {
                    Ok(child) => {
                        current = Some((child, Instant::now()));
                        segments.push((t, path));
                    }
                    Err(e) => {
                        warn!(error = %e, "Could not start ffmpeg: recording without sound");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if let Some((child, _)) = current.take() {
            finish(child).await;
        }
        let kept = join(&src.cfg.ffmpeg, &segments, &dir.join(FILE)).await;
        let _ = tokio::fs::remove_dir_all(&parts).await;
        kept
    })
}

/// Joins the segments a recording cut short (the daemon stopped) left behind.
pub async fn salvage(ffmpeg: &Path, dir: &Path) -> bool {
    let parts = dir.join(PARTS);
    let Ok(mut rd) = tokio::fs::read_dir(&parts).await else {
        return false;
    };
    let mut segments = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let p = e.path();
        if let Some(t) = p.file_stem().and_then(|s| s.to_str()?.parse::<u64>().ok()) {
            segments.push((t, p));
        }
    }
    segments.sort();
    let kept = join(ffmpeg, &segments, &dir.join(FILE)).await;
    let _ = tokio::fs::remove_dir_all(&parts).await;
    kept
}

fn record(src: &Source, path: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new(&src.cfg.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "pulse", "-server"])
        .arg(src.cfg.server_arg())
        .args(["-sample_rate", "48000", "-channels", "2", "-i"])
        .arg(format!("{}.monitor", src.sink))
        .args(["-c:a", "libopus", "-b:a", "64k", "-f", "webm"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::proc::die_with_parent(&mut cmd);
    cmd.spawn()
}

/// Stops ffmpeg the way Ctrl+C does, so it finishes the file.
async fn finish(mut child: Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on our own child.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGINT);
        }
    }
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// The filter that puts each input at its offset (ms) and mixes them.
fn mix_filter(offsets: &[u64]) -> String {
    let mut f: Vec<String> = offsets
        .iter()
        .enumerate()
        .map(|(i, t)| format!("[{i}:a]adelay={t}:all=1[a{i}]"))
        .collect();
    let labels: String = (0..offsets.len()).map(|i| format!("[a{i}]")).collect();
    f.push(format!(
        "{labels}amix=inputs={}:normalize=0:duration=longest[out]",
        offsets.len()
    ));
    f.join(";")
}

/// Joins audio files, each starting at its offset (ms), into `out`. False (and no
/// `out`) when there is nothing to join or all of it is silence.
pub async fn join(ffmpeg: &Path, inputs: &[(u64, PathBuf)], out: &Path) -> bool {
    let mut present = Vec::new();
    for (t, p) in inputs {
        if tokio::fs::metadata(p).await.is_ok_and(|m| m.len() > 0) {
            present.push((*t, p.clone()));
        }
    }
    if present.is_empty() {
        return false;
    }
    let tmp = out.with_extension("tmp.webm");
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    for (_, p) in &present {
        cmd.arg("-i").arg(p);
    }
    let offsets: Vec<u64> = present.iter().map(|(t, _)| *t).collect();
    let ok = cmd
        .arg("-filter_complex")
        .arg(mix_filter(&offsets))
        .args(["-map", "[out]", "-c:a", "libopus", "-b:a", "64k", "-f", "webm"])
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map(|o| {
            if !o.status.success() {
                warn!(error = %String::from_utf8_lossy(&o.stderr).trim(), "Could not join the recording's audio");
            }
            o.status.success()
        })
        .unwrap_or(false);
    if !ok {
        let _ = tokio::fs::remove_file(&tmp).await;
        return false;
    }
    if is_silent(ffmpeg, &tmp).await {
        info!(file = %out.display(), "The recording stayed silent: no audio kept");
        let _ = tokio::fs::remove_file(&tmp).await;
        return false;
    }
    tokio::fs::rename(&tmp, out).await.is_ok()
}

/// The loudest moment of `file`, from ffmpeg's volumedetect, is below SILENCE_DB.
async fn is_silent(ffmpeg: &Path, file: &Path) -> bool {
    let Ok(out) = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(file)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
    else {
        return false;
    };
    max_volume(&String::from_utf8_lossy(&out.stderr)).is_none_or(|db| db < SILENCE_DB)
}

/// `max_volume: -12.3 dB` from volumedetect's report (None for -inf or none).
fn max_volume(report: &str) -> Option<f64> {
    report
        .lines()
        .find_map(|l| l.split("max_volume:").nth(1))
        .and_then(|v| v.trim().trim_end_matches("dB").trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_delayed_and_mixed() {
        assert_eq!(
            mix_filter(&[0, 1500]),
            "[0:a]adelay=0:all=1[a0];[1:a]adelay=1500:all=1[a1];[a0][a1]amix=inputs=2:normalize=0:duration=longest[out]"
        );
    }

    #[test]
    fn loudness_from_the_report() {
        let r = "[Parsed_volumedetect_0 @ 0x1] mean_volume: -30.1 dB\n[Parsed_volumedetect_0 @ 0x1] max_volume: -12.5 dB\n";
        assert_eq!(max_volume(r), Some(-12.5));
        assert_eq!(max_volume("max_volume: -inf dB"), None);
        assert_eq!(max_volume("nothing"), None);
    }
}
