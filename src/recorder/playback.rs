//! Recording metadata and bounded frame playback.

use super::*;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RecordingMeta {
    pub id: Uuid,
    pub session_id: Uuid,
    pub profile: Option<String>,
    pub node: String,
    pub started_at_secs: u64,
    pub ended_at_secs: Option<u64>,
    pub duration_seconds: u64,
    /// The length in milliseconds (0 in recordings of older versions: see `length_ms`).
    #[serde(default)]
    pub duration_ms: u64,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub initial_url: Option<String>,
    pub final_url: Option<String>,
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
    /// Content type of the icon stored with the recording (`favicon.bin`), grabbed from
    /// the site itself during the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_type: Option<String>,
    /// Marked to be kept: the cleanup never removes it. Not stored in `meta.json`
    /// (the recorder rewrites that while recording) but as a `keep` file beside it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    pub status: String, // "recording", "completed", "interrupted"
    /// The recording has sound (`audio.webm`, see rec_audio.rs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audio: bool,
    /// Made by merging these recordings, in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_from: Vec<Uuid>,
    /// When the merge was made: the cleanup's age counts from here, not from
    /// the start of the oldest source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at_secs: Option<u64>,
}

impl RecordingMeta {
    /// The recording's length (ms).
    pub fn length_ms(&self) -> u64 {
        if self.duration_ms > 0 {
            self.duration_ms
        } else {
            self.duration_seconds.saturating_mul(1000)
        }
    }

    /// The moment the cleanup measures the recording's age from.
    pub(super) fn age_reference_secs(&self) -> u64 {
        self.merged_at_secs
            .unwrap_or(self.started_at_secs)
            .max(self.ended_at_secs.unwrap_or(0))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RecordingFrame {
    pub t: u64, // ms offset from recording start
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub data: String, // base64 JPEG
}

/// Fetch a bounded playback window without reading the images preceding it.
/// Frame timestamps are monotonic, so probing complete JSON lines locates it.
pub async fn playback_window(path: &Path, at: u64) -> std::io::Result<(Vec<RecordingFrame>, bool)> {
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
    let file = fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let mut reader = tokio::io::BufReader::new(file);
    let (mut low, mut high) = (0, size);
    let mut line = String::new();
    while high.saturating_sub(low) > 1024 * 1024 {
        let mid = low + (high - low) / 2;
        reader.seek(std::io::SeekFrom::Start(mid)).await?;
        line.clear();
        reader.read_line(&mut line).await?; // skip the partial line
        let start = reader.stream_position().await?;
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            high = mid;
            continue;
        }
        match serde_json::from_str::<RecordingFrame>(&line) {
            Ok(frame) if frame.t <= at => low = start,
            _ => high = mid,
        }
    }
    reader.seek(std::io::SeekFrom::Start(low)).await?;
    let mut frames = Vec::new();
    let mut bytes = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok((frames, true));
        }
        let Ok(frame) = serde_json::from_str::<RecordingFrame>(&line) else {
            continue;
        };
        if frame.t <= at {
            frames.clear();
            bytes = 0;
        }
        bytes += line.len();
        frames.push(frame);
        if frames.len() >= 120 || (frames.len() >= 2 && bytes >= 8 * 1024 * 1024) {
            return Ok((frames, reader.stream_position().await? >= size));
        }
    }
}
