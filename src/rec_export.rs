//! A recording as a WebM video, encoded here with ffmpeg: its frames at their
//! own times (VP9) and its sound (Opus, as recorded). The dashboard's own export
//! renders frames as fast as it can and has no sound; this one keeps both.

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Instant;

use base64::Engine as _;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

use crate::recorder::{self, RecorderConfig, RecordingFrame};

/// One export at a time: encoding takes the node's cores.
static EXPORTS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// The last frame stays this long at least (ms).
const LAST_FRAME_MS: u64 = 500;

/// Width and height of a JPEG, from its start-of-frame marker.
fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// The concat demuxer's list: each frame for as long as it was shown, the last
/// one until the recording ended (listed twice, or ffmpeg drops its duration).
fn concat_list(frames: &[(String, u64)], total_ms: u64) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for (i, (file, t)) in frames.iter().enumerate() {
        let until = frames
            .get(i + 1)
            .map_or(total_ms.max(t + LAST_FRAME_MS), |(_, n)| *n);
        let secs = (until.saturating_sub(*t)).max(1) as f64 / 1000.0;
        out.push_str(&format!("file '{file}'\nduration {secs:.3}\n"));
    }
    if let Some((file, _)) = frames.last() {
        out.push_str(&format!("file '{file}'\n"));
    }
    out
}

/// Encodes the recording; the open file of the video, its size and a name for
/// it. The work files are gone by then (the open file still reads).
pub async fn export_webm(
    config: &RecorderConfig,
    id: Uuid,
) -> Result<(tokio::fs::File, u64, String), String> {
    let ffmpeg = config.ffmpeg.clone().ok_or("No ffmpeg on this node")?;
    let meta = recorder::get_recording(config, id)
        .await
        .ok_or("No such recording")?;
    if meta.status == "recording" {
        return Err("Still recording: stop it first".into());
    }
    let _turn = EXPORTS.acquire().await.map_err(|e| e.to_string())?;
    let started = Instant::now();
    let rec_dir = config.recordings_dir.join(id.to_string());
    let work = tempfile::Builder::new()
        .prefix(".export-")
        .tempdir_in(&rec_dir)
        .map_err(|e| format!("No room to work: {e}"))?;
    let dir = work.path();

    // The frames as files, and the size they all fit in.
    let f = tokio::fs::File::open(rec_dir.join("frames.jsonl"))
        .await
        .map_err(|_| "The recording has no frames")?;
    let mut lines = tokio::io::BufReader::new(f).lines();
    let (mut frames, mut w, mut h) = (Vec::new(), 0u32, 0u32);
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let Ok(frame) = serde_json::from_str::<RecordingFrame>(&line) else {
            continue;
        };
        let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(frame.data.as_bytes())
        else {
            continue;
        };
        if let Some((fw, fh)) = jpeg_size(&jpeg) {
            (w, h) = (w.max(fw), h.max(fh));
        }
        let name = format!("{:07}.jpg", frames.len());
        tokio::fs::write(dir.join(&name), &jpeg)
            .await
            .map_err(|e| e.to_string())?;
        frames.push((name, frame.t));
    }
    if frames.is_empty() {
        return Err("The recording has no frames".into());
    }
    let (w, h) = (w.max(2).div_ceil(2) * 2, h.max(2).div_ceil(2) * 2);
    let total_ms = meta.duration_seconds * 1000;
    let list = dir.join("frames.ffconcat");
    tokio::fs::write(&list, concat_list(&frames, total_ms))
        .await
        .map_err(|e| e.to_string())?;

    let out = dir.join("export.webm");
    let audio = rec_dir.join(crate::rec_audio::FILE);
    let with_audio = meta.audio && audio.exists();
    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list);
    if with_audio {
        cmd.arg("-i").arg(&audio);
    }
    // Frames of another size (the window was resized) are fitted in, centred.
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps=15,format=yuv420p"
    );
    cmd.args(["-map", "0:v", "-vf", &vf])
        .args([
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
        ])
        .args(["-row-mt", "1", "-crf", "38", "-b:v", "0", "-threads", "4"]);
    if with_audio {
        cmd.args(["-map", "1:a", "-c:a", "copy"]);
    }
    let res = cmd
        .args(["-f", "webm"])
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Could not run ffmpeg: {e}"))?;
    if !res.status.success() {
        let err = String::from_utf8_lossy(&res.stderr);
        warn!(recording_id = %id, error = %err.trim(), "WebM export failed");
        return Err(format!(
            "ffmpeg failed: {}",
            err.lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("no output")
                .trim()
        ));
    }
    let file = tokio::fs::File::open(&out)
        .await
        .map_err(|e| e.to_string())?;
    let size = file.metadata().await.map_err(|e| e.to_string())?.len();
    drop(work);
    info!(recording_id = %id, frames = frames.len(), audio = with_audio, size, took_ms = started.elapsed().as_millis() as u64, "Exported recording as WebM");
    let date = meta.started_at_secs;
    Ok((
        file,
        size,
        format!("recording-{}-{date}.webm", &id.to_string()[..8]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_last_as_long_as_they_were_shown() {
        let frames = vec![
            ("a.jpg".to_string(), 0),
            ("b.jpg".to_string(), 1500),
            ("c.jpg".to_string(), 1600),
        ];
        let list = concat_list(&frames, 4000);
        assert_eq!(
            list,
            "ffconcat version 1.0\nfile 'a.jpg'\nduration 1.500\nfile 'b.jpg'\nduration 0.100\nfile 'c.jpg'\nduration 2.400\nfile 'c.jpg'\n"
        );
        // A recording whose length is unknown still shows its last frame a moment.
        assert!(concat_list(&frames[..1], 0).contains("duration 0.500"));
    }

    #[test]
    fn jpeg_dimensions_from_the_frame_header() {
        // SOI, an APP0 segment, then SOF0 with height 600 and width 800.
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        j.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20, 0x03]);
        j.extend([0u8; 12]);
        assert_eq!(jpeg_size(&j), Some((800, 600)));
        assert_eq!(jpeg_size(b"not a jpeg"), None);
    }
}
