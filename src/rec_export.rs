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

/// In concise mode, a still period lasts at most this long (ms).
pub const CONCISE_CAP_MS: u64 = 5_000;

/// Name prefix of an export's work directory inside the recording's: left behind
/// by a crash, it is removed at startup (see recorder::mark_orphaned_recordings).
pub const WORK_PREFIX: &str = ".export-";

/// The video's length (ms): the recording's, or up to the last frame and a moment
/// more if that is later.
fn export_total_ms(recorded_ms: u64, last_frame_ms: u64) -> u64 {
    recorded_ms.max(last_frame_ms + LAST_FRAME_MS)
}

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

/// The concat demuxer's list: each frame for as long as it was shown, the first
/// one from the start (the sound starts there too), the last one until the
/// recording ended. In concise mode, each gap between frames is capped at 5 seconds.
/// Returns the concat script and the sum of durations of all frames before the last one.
fn concat_list(frames: &[(String, u64)], total_ms: u64, concise: bool) -> (String, u64) {
    let mut out = String::from("ffconcat version 1.0\n");
    let mut sum_durations = 0u64;
    for (i, (file, t)) in frames.iter().enumerate() {
        let from = if i == 0 { 0 } else { *t };
        let until = frames
            .get(i + 1)
            .map_or(total_ms.max(t + LAST_FRAME_MS), |(_, n)| *n);
        let raw_dur = until.saturating_sub(from).max(1);
        let dur = if concise {
            raw_dur.min(CONCISE_CAP_MS)
        } else {
            raw_dur
        };
        if i + 1 < frames.len() {
            sum_durations += dur;
        }
        let secs = dur as f64 / 1000.0;
        out.push_str(&format!("file '{file}'\nduration {secs:.3}\n"));
    }
    (out, sum_durations)
}

/// The audio slice intervals `(start_ms, end_ms)` to keep for concise mode,
/// matching the frames shown. Contiguous intervals are merged.
fn audio_intervals(frames: &[(String, u64)], total_ms: u64, concise: bool) -> Vec<(u64, u64)> {
    if !concise {
        return vec![(0, total_ms)];
    }
    let mut intervals: Vec<(u64, u64)> = Vec::new();
    for (i, (_, t)) in frames.iter().enumerate() {
        let from = if i == 0 { 0 } else { *t };
        let until = frames
            .get(i + 1)
            .map_or(total_ms.max(t + LAST_FRAME_MS), |(_, n)| *n);
        let raw_dur = until.saturating_sub(from).max(1);
        let dur = raw_dur.min(CONCISE_CAP_MS);
        let start = from;
        let end = from + dur;
        if let Some(last) = intervals.last_mut() {
            if last.1 >= start {
                last.1 = last.1.max(end);
                continue;
            }
        }
        intervals.push((start, end));
    }
    intervals
}

/// Encodes the recording; the open file of the video, its size and a name for
/// it. The work files are gone by then (the open file still reads).
pub async fn export_webm(
    config: &RecorderConfig,
    id: Uuid,
    concise: bool,
) -> Result<(tokio::fs::File, u64, String), String> {
    let ffmpeg = config.ffmpeg.clone().ok_or("No ffmpeg on this node")?;
    // Before the wait for a turn: the cleanup must not take the recording meanwhile.
    let _busy = recorder::mark_busy(&[id]).ok_or("Still recording: stop it first")?;
    let meta = recorder::get_recording(config, id)
        .await
        .ok_or("No such recording")?;
    let _turn = EXPORTS.acquire().await.map_err(|e| e.to_string())?;
    let started = Instant::now();
    let rec_dir = config.recordings_dir.join(id.to_string());
    let work = rec_dir.join(format!("{WORK_PREFIX}{}", Uuid::new_v4()));
    crate::persist::private_dir(&work)
        .await
        .map_err(|e| format!("No room to work: {e}"))?;
    let result = encode(&ffmpeg, &meta, id, &rec_dir, &work, concise).await;
    // Off the async workers' hands: thousands of frame files.
    let _ = tokio::fs::remove_dir_all(&work).await;
    let (file, size, frames, with_audio) = result?;
    info!(recording_id = %id, frames, concise, audio = with_audio, size, took_ms = started.elapsed().as_millis() as u64, "Exported recording as WebM");
    let date = meta.started_at_secs;
    let name = if concise {
        format!("recording-{}-{date}-concise.webm", &id.to_string()[..8])
    } else {
        format!("recording-{}-{date}.webm", &id.to_string()[..8])
    };
    Ok((file, size, name))
}

/// The encoding itself, in the work directory `dir`: the open video file, its size,
/// the frames in it and whether it has sound.
async fn encode(
    ffmpeg: &std::path::Path,
    meta: &recorder::RecordingMeta,
    id: Uuid,
    rec_dir: &std::path::Path,
    dir: &std::path::Path,
    concise: bool,
) -> Result<(tokio::fs::File, u64, usize, bool), String> {
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
    let last_t = frames.last().map_or(0, |(_, t)| *t);
    let total_ms = export_total_ms(meta.length_ms(), last_t);
    let (concat_content, sum_durations) = concat_list(&frames, total_ms, concise);
    let list = dir.join("frames.ffconcat");
    tokio::fs::write(&list, concat_content)
        .await
        .map_err(|e| e.to_string())?;

    let out = dir.join("export.webm");
    let audio = rec_dir.join(crate::rec_audio::FILE);
    let with_audio = meta.audio && audio.exists();
    let mut cmd = Command::new(ffmpeg);
    crate::proc::die_with_parent(&mut cmd);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list);

    let mut needs_audio_concat = false;
    let audio_list = dir.join("audio.ffconcat");
    if with_audio {
        if concise {
            let intervals = audio_intervals(&frames, total_ms, true);
            if !(intervals.len() == 1 && intervals[0].0 == 0 && intervals[0].1 >= total_ms) {
                needs_audio_concat = true;
                let mut a_out = String::from("ffconcat version 1.0\n");
                let audio_escaped = audio.to_string_lossy().replace('\'', "'\\''");
                for (start_ms, end_ms) in intervals {
                    let in_sec = start_ms as f64 / 1000.0;
                    let out_sec = end_ms as f64 / 1000.0;
                    a_out.push_str(&format!(
                        "file '{audio_escaped}'\ninpoint {in_sec:.3}\noutpoint {out_sec:.3}\n"
                    ));
                }
                tokio::fs::write(&audio_list, a_out)
                    .await
                    .map_err(|e| e.to_string())?;
                cmd.args(["-f", "concat", "-safe", "0", "-i"])
                    .arg(&audio_list);
            } else {
                cmd.arg("-i").arg(&audio);
            }
        } else {
            cmd.arg("-i").arg(&audio);
        }
    }

    // Frames of another size (the window was resized) are fitted in, centred.
    // The concat demuxer drops the last image's duration: the last frame is held
    // with tpad instead, and the video cut to the recording's length.
    let hold_ms = if concise {
        (total_ms.saturating_sub(last_t)).clamp(LAST_FRAME_MS, CONCISE_CAP_MS)
    } else {
        total_ms.saturating_sub(last_t)
    };
    let hold = hold_ms as f64 / 1000.0;
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps=15,tpad=stop_mode=clone:stop_duration={hold:.3},format=yuv420p"
    );
    let total_video_ms = if concise {
        sum_durations + hold_ms
    } else {
        total_ms
    };
    let length = format!("{:.3}", total_video_ms as f64 / 1000.0);
    cmd.args(["-map", "0:v", "-vf", &vf, "-t", &length])
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
        if needs_audio_concat {
            cmd.args(["-map", "1:a", "-c:a", "libopus"]);
        } else {
            cmd.args(["-map", "1:a", "-c:a", "copy"]);
        }
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
    Ok((file, size, frames.len(), with_audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_last_as_long_as_they_were_shown() {
        // The first frame came a second in: it shows from the start.
        let frames = vec![
            ("a.jpg".to_string(), 1000),
            ("b.jpg".to_string(), 1500),
            ("c.jpg".to_string(), 1600),
        ];
        let (list, sum) = concat_list(&frames, 4000, false);
        assert_eq!(
            list,
            "ffconcat version 1.0\nfile 'a.jpg'\nduration 1.500\nfile 'b.jpg'\nduration 0.100\nfile 'c.jpg'\nduration 2.400\n"
        );
        assert_eq!(sum, 1600);
        // A recording whose length is unknown still shows its last frame a moment.
        let (tail_list, _) = concat_list(&frames[2..], 0, false);
        assert!(tail_list.contains("duration 2.100"));
    }

    #[test]
    fn concise_mode_caps_long_gaps_at_five_seconds() {
        let frames = vec![
            ("a.jpg".to_string(), 0),
            ("b.jpg".to_string(), 20_000),
            ("c.jpg".to_string(), 21_000),
        ];
        let (list, sum) = concat_list(&frames, 100_000, true);
        assert_eq!(
            list,
            "ffconcat version 1.0\nfile 'a.jpg'\nduration 5.000\nfile 'b.jpg'\nduration 1.000\nfile 'c.jpg'\nduration 5.000\n"
        );
        assert_eq!(sum, 6_000);
    }

    #[test]
    fn concise_audio_intervals_cut_long_gaps() {
        let frames = vec![
            ("a.jpg".to_string(), 0),
            ("b.jpg".to_string(), 20_000),
            ("c.jpg".to_string(), 21_000),
        ];
        let intervals = audio_intervals(&frames, 50_000, true);
        assert_eq!(intervals, vec![(0, 5_000), (20_000, 26_000)]);
    }

    #[test]
    fn the_video_lasts_as_long_as_the_recording() {
        // The recording's own length wins (ms, not whole seconds)...
        assert_eq!(export_total_ms(12_750, 12_000), 12_750);
        // ...unless the last frame is later than it says.
        assert_eq!(export_total_ms(5_000, 5_000), 5_500);
        assert_eq!(export_total_ms(0, 0), 500);
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
