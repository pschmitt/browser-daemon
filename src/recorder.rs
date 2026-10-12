use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RecordingMeta {
    pub id: Uuid,
    pub session_id: Uuid,
    pub profile: Option<String>,
    pub node: String,
    pub started_at_secs: u64,
    pub ended_at_secs: Option<u64>,
    pub duration_seconds: u64,
    pub frame_count: u64,
    pub size_bytes: u64,
    pub initial_url: Option<String>,
    pub final_url: Option<String>,
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
    pub status: String, // "recording", "completed", "interrupted"
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

#[derive(Clone, Debug)]
pub struct RecorderConfig {
    pub enabled: bool,
    pub recordings_dir: PathBuf,
    pub max_storage_bytes: u64,
    pub max_age_secs: u64,
    pub max_count: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProxyRecordingFrame {
    pub url: Option<String>,
    pub title: Option<String>,
    pub data: String, // base64 JPEG
}

#[derive(Clone)]
pub struct SessionRecorderHandle {
    pub recording_id: Uuid,
    pub stop_flag: Arc<AtomicBool>,
    pub clock: Arc<RecordingClock>,
    pub frame_tx: tokio::sync::mpsc::UnboundedSender<ProxyRecordingFrame>,
}

impl SessionRecorderHandle {
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }
}

/// Recording time that stands still while the recording is paused, so a
/// pause leaves no frozen gap in the playback.
pub struct RecordingClock {
    start: Instant,
    state: std::sync::Mutex<PauseState>,
}

#[derive(Default)]
struct PauseState {
    paused_total: Duration,
    paused_since: Option<Instant>,
}

impl RecordingClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            state: std::sync::Mutex::new(PauseState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PauseState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused_since.is_some()
    }

    /// Returns false when the clock already was in the requested state.
    pub fn set_paused(&self, paused: bool) -> bool {
        let mut st = self.lock();
        match (paused, st.paused_since) {
            (true, None) => st.paused_since = Some(Instant::now()),
            (false, Some(since)) => {
                st.paused_total += since.elapsed();
                st.paused_since = None;
            }
            _ => return false,
        }
        true
    }

    pub fn elapsed(&self) -> Duration {
        let st = self.lock();
        let paused = st.paused_total + st.paused_since.map_or(Duration::ZERO, |s| s.elapsed());
        self.start.elapsed().saturating_sub(paused)
    }
}

pub fn start_recording(
    session_id: Uuid,
    port: u16,
    profile: Option<String>,
    node_name: String,
    config: RecorderConfig,
) -> SessionRecorderHandle {
    let recording_id = Uuid::new_v4();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let clock = Arc::new(RecordingClock::new());
    let clock_clone = clock.clone();
    let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel::<ProxyRecordingFrame>();

    tokio::spawn(async move {
        record_session_loop(
            recording_id,
            session_id,
            port,
            profile,
            node_name,
            config,
            stop_clone,
            clock_clone,
            frame_rx,
        )
        .await;
    });

    SessionRecorderHandle {
        recording_id,
        stop_flag,
        clock,
        frame_tx,
    }
}

async fn write_single_frame(
    writer: &mut tokio::fs::File,
    meta: &mut RecordingMeta,
    meta_file: &Path,
    rec_dir: &Path,
    total_frames: &mut u64,
    last_meta_flush: &mut Instant,
    clock: &RecordingClock,
    current_url: &mut Option<String>,
    current_title: &mut Option<String>,
    data: String,
    url: Option<String>,
    title: Option<String>,
) {
    if let Some(ref u) = url {
        if !u.is_empty() && u != "about:blank" {
            if meta.initial_url.is_none() {
                meta.initial_url = Some(u.clone());
            }
            if meta.favicon_url.is_none() {
                if let Ok(parsed) = url::Url::parse(u) {
                    if let Some(host) = parsed.host_str() {
                        if !host.is_empty() && host != "localhost" && !host.starts_with("127.") {
                            meta.favicon_url =
                                Some(format!("{}://{}/favicon.ico", parsed.scheme(), host));
                        }
                    }
                }
            }
            *current_url = Some(u.clone());
        }
    }
    if let Some(ref t) = title {
        if is_real_title(t) {
            *current_title = Some(t.clone());
        }
    }

    if clock.is_paused() {
        return;
    }
    let elapsed_ms = clock.elapsed().as_millis() as u64;
    let frame = RecordingFrame {
        t: elapsed_ms,
        url: current_url.clone(),
        title: current_title.clone(),
        data,
    };

    if let Ok(line) = serde_json::to_string(&frame) {
        let _ = writer.write_all(line.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        *total_frames += 1;
    }

    // Flush metadata every 4 seconds or every 30 frames
    if last_meta_flush.elapsed() > Duration::from_secs(4) || *total_frames % 30 == 0 {
        let _ = writer.flush().await;
        meta.duration_seconds = clock.elapsed().as_secs();
        meta.frame_count = *total_frames;
        meta.size_bytes = compute_rec_dir_size(rec_dir).await;
        meta.final_url = current_url.clone();
        meta.title = current_title.clone();
        let _ = write_meta(meta_file, meta).await;
        *last_meta_flush = Instant::now();
    }
}

/// Chromium reports a blank tab's URL as its title; that is no title.
fn is_real_title(title: &str) -> bool {
    !title.is_empty() && title != "about:blank"
}

/// CDP request ids at or above this ask for `document.title`.
const TITLE_REQUEST_ID: u64 = 1 << 40;
/// Re-read the title this often: single-page apps retitle without navigating.
const TITLE_REFRESH: Duration = Duration::from_secs(5);

async fn record_session_loop(
    recording_id: Uuid,
    session_id: Uuid,
    port: u16,
    profile: Option<String>,
    node_name: String,
    config: RecorderConfig,
    stop_flag: Arc<AtomicBool>,
    clock: Arc<RecordingClock>,
    mut frame_rx: tokio::sync::mpsc::UnboundedReceiver<ProxyRecordingFrame>,
) {
    let rec_dir = config.recordings_dir.join(recording_id.to_string());
    if let Err(e) = fs::create_dir_all(&rec_dir).await {
        error!(recording_id = %recording_id, error = %e, "Failed to create recording directory");
        return;
    }

    let started_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let meta_file = rec_dir.join("meta.json");
    let frames_file = rec_dir.join("frames.jsonl");

    let mut meta = RecordingMeta {
        id: recording_id,
        session_id,
        profile: profile.clone(),
        node: node_name.clone(),
        started_at_secs,
        ended_at_secs: None,
        duration_seconds: 0,
        frame_count: 0,
        size_bytes: 0,
        initial_url: None,
        final_url: None,
        title: None,
        favicon_url: None,
        status: "recording".to_string(),
    };

    let _ = write_meta(&meta_file, &meta).await;

    info!(
        recording_id = %recording_id,
        session_id = %session_id,
        port = port,
        "Started session recording"
    );

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    // Open frames file
    let mut frames_writer = match OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&frames_file)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            error!(recording_id = %recording_id, error = %e, "Failed to open frames file");
            meta.status = "interrupted".to_string();
            let _ = write_meta(&meta_file, &meta).await;
            return;
        }
    };

    let mut current_url: Option<String> = None;
    let mut current_title: Option<String> = None;
    let mut total_frames: u64 = 0;
    let mut last_proxy_frame = Instant::now() - Duration::from_secs(60);
    let mut last_meta_flush = Instant::now();

    // Main recording loop
    while !stop_flag.load(Ordering::SeqCst) {
        // 1. Drain any proxy frames that arrived from handoff.html
        while let Ok(pf) = frame_rx.try_recv() {
            last_proxy_frame = Instant::now();
            write_single_frame(
                &mut frames_writer,
                &mut meta,
                &meta_file,
                &rec_dir,
                &mut total_frames,
                &mut last_meta_flush,
                &clock,
                &mut current_url,
                &mut current_title,
                pf.data,
                pf.url,
                pf.title,
            )
            .await;
        }

        // 2. If handoff.html is actively streaming (frames received in last 2 seconds),
        // wait for more proxy frames without competing on background screencast!
        if last_proxy_frame.elapsed() < Duration::from_secs(2) {
            match tokio::time::timeout(Duration::from_millis(500), frame_rx.recv()).await {
                Ok(Some(pf)) => {
                    last_proxy_frame = Instant::now();
                    write_single_frame(
                        &mut frames_writer,
                        &mut meta,
                        &meta_file,
                        &rec_dir,
                        &mut total_frames,
                        &mut last_meta_flush,
                        &clock,
                        &mut current_url,
                        &mut current_title,
                        pf.data,
                        pf.url,
                        pf.title,
                    )
                    .await;
                    continue;
                }
                Ok(None) => {
                    // Channel closed (session terminating)
                    break;
                }
                Err(_) => {
                    continue;
                }
            }
        }

        // 3. Otherwise, use background CDP screencast loop for headless sessions
        let targets_url = format!("http://127.0.0.1:{}/json/list", port);
        let active_page_target = match http_client.get(&targets_url).send().await {
            Ok(res) => {
                if let Ok(targets) = res.json::<Vec<serde_json::Value>>().await {
                    let mut page_targets: Vec<serde_json::Value> = targets
                        .into_iter()
                        .filter(|t| {
                            t.get("type").and_then(|v| v.as_str()) == Some("page")
                                && t.get("webSocketDebuggerUrl").is_some()
                        })
                        .collect();

                    // Prioritize targets: non-about:blank first
                    page_targets.sort_by_key(|t| {
                        let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        if url.is_empty()
                            || url == "about:blank"
                            || url.starts_with("chrome-extension://")
                        {
                            1
                        } else {
                            0
                        }
                    });

                    page_targets.into_iter().next()
                } else {
                    None
                }
            }
            Err(_) => {
                // Port unreachable, Chromium probably died
                debug!(session_id = %session_id, "Session port unreachable, exiting recorder");
                break;
            }
        };

        let target = match active_page_target {
            Some(t) => t,
            None => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };

        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let ws_url = match target.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
            Some(u) => u.to_string(),
            None => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };

        let current_target_id = target_id.clone();
        let current_target_is_blank = target
            .get("url")
            .and_then(|v| v.as_str())
            .map_or(true, |u| u == "about:blank" || u.is_empty());

        if let Some(u) = target.get("url").and_then(|v| v.as_str()) {
            if !u.is_empty() && u != "about:blank" {
                if meta.initial_url.is_none() {
                    meta.initial_url = Some(u.to_string());
                }
                if meta.favicon_url.is_none() {
                    if let Ok(parsed) = url::Url::parse(u) {
                        if let Some(host) = parsed.host_str() {
                            if !host.is_empty() && host != "localhost" && !host.starts_with("127.")
                            {
                                meta.favicon_url =
                                    Some(format!("{}://{}/favicon.ico", parsed.scheme(), host));
                            }
                        }
                    }
                }
                current_url = Some(u.to_string());
            }
        }
        if let Some(t) = target.get("title").and_then(|v| v.as_str()) {
            if is_real_title(t) {
                current_title = Some(t.to_string());
            }
        }

        debug!(recording_id = %recording_id, ws_url = %ws_url, "Connecting recorder to page WebSocket");
        let (ws_stream, _) = match tokio_tungstenite::connect_async(&ws_url).await {
            Ok(conn) => conn,
            Err(e) => {
                debug!(recording_id = %recording_id, error = %e, "Failed to connect to page WebSocket");
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
        };

        let (mut write, mut read) = ws_stream.split();

        // Enable Page domain and start screencast
        let enable_cmd = serde_json::json!({
            "id": 1,
            "method": "Page.enable"
        });
        let _ = write
            .send(Message::Text(enable_cmd.to_string().into()))
            .await;

        let start_screencast = serde_json::json!({
            "id": 2,
            "method": "Page.startScreencast",
            "params": {
                "format": "jpeg",
                "quality": 65,
                "maxWidth": 1280,
                "maxHeight": 800,
                "everyNthFrame": 1
            }
        });
        let _ = write
            .send(Message::Text(start_screencast.to_string().into()))
            .await;

        let mut next_ack_id = 100u64;
        let mut last_bg_frame_time = Instant::now();
        let mut next_title_id = TITLE_REQUEST_ID;
        // The target's title above may predate a navigation; ask right away.
        let mut last_title_request = Instant::now() - TITLE_REFRESH;

        // Inner frame loop for this page
        while !stop_flag.load(Ordering::SeqCst) {
            tokio::select! {
                // If proxy frame arrives, handoff.html was connected!
                proxy_frame = frame_rx.recv() => {
                    match proxy_frame {
                        Some(pf) => {
                            last_proxy_frame = Instant::now();
                            write_single_frame(
                                &mut frames_writer,
                                &mut meta,
                                &meta_file,
                                &rec_dir,
                                &mut total_frames,
                                &mut last_meta_flush,
                                &clock,
                                &mut current_url,
                                &mut current_title,
                                pf.data,
                                pf.url,
                                pf.title,
                            ).await;
                            // Yield background connection to handoff
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
                msg_res = tokio::time::timeout(Duration::from_millis(600), read.next()) => {
                    match msg_res {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                let method = val.get("method").and_then(|v| v.as_str()).unwrap_or("");
                                if val.get("id").and_then(|v| v.as_u64()).is_some_and(|id| id >= TITLE_REQUEST_ID) {
                                    if let Some(t) = val.pointer("/result/result/value").and_then(|v| v.as_str()) {
                                        if is_real_title(t) {
                                            current_title = Some(t.to_string());
                                        }
                                    }
                                }
                                let navigated = method == "Page.loadEventFired"
                                    || method == "Page.navigatedWithinDocument"
                                    || (method == "Page.frameNavigated"
                                        && val.pointer("/params/frame/parentId").is_none());
                                if navigated || last_title_request.elapsed() >= TITLE_REFRESH {
                                    next_title_id += 1;
                                    let title_cmd = serde_json::json!({
                                        "id": next_title_id,
                                        "method": "Runtime.evaluate",
                                        "params": { "expression": "document.title", "returnByValue": true }
                                    });
                                    let _ = write.send(Message::Text(title_cmd.to_string().into())).await;
                                    last_title_request = Instant::now();
                                }
                                if method == "Page.screencastFrame" {
                                    if let Some(params) = val.get("params") {
                                        let ack_session_id = params.get("sessionId").and_then(|v| v.as_u64());
                                        let data = params.get("data").and_then(|v| v.as_str());

                                        if let (Some(frame_data), Some(ack_id)) = (data, ack_session_id) {
                                            write_single_frame(
                                                &mut frames_writer,
                                                &mut meta,
                                                &meta_file,
                                                &rec_dir,
                                                &mut total_frames,
                                                &mut last_meta_flush,
                                                &clock,
                                                &mut current_url,
                                                &mut current_title,
                                                frame_data.to_string(),
                                                None,
                                                None,
                                            ).await;
                                            last_bg_frame_time = Instant::now();

                                            // Send frame ACK
                                            next_ack_id += 1;
                                            let ack_msg = serde_json::json!({
                                                "id": next_ack_id,
                                                "method": "Page.screencastFrameAck",
                                                "params": {
                                                    "sessionId": ack_id
                                                }
                                            });
                                            let _ = write.send(Message::Text(ack_msg.to_string().into())).await;
                                        }
                                    }
                                } else if method == "Page.frameNavigated" {
                                    if let Some(frame) = val.pointer("/params/frame") {
                                        if frame.get("parentId").is_none() {
                                            if let Some(u) = frame.get("url").and_then(|v| v.as_str()) {
                                                if !u.is_empty() && u != "about:blank" {
                                                    current_url = Some(u.to_string());
                                                    if meta.initial_url.is_none() {
                                                        meta.initial_url = Some(u.to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                            // Page closed
                            debug!(recording_id = %recording_id, "Page connection closed");
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            debug!(recording_id = %recording_id, error = %e, "WebSocket error");
                            break;
                        }
                        Err(_) => {
                            // Timeout hit (no messages for 600ms)
                            if last_bg_frame_time.elapsed() > Duration::from_secs(2) {
                                let check_url = format!("http://127.0.0.1:{}/json/list", port);
                                if let Ok(res) = http_client.get(&check_url).send().await {
                                    if let Ok(targets) = res.json::<Vec<serde_json::Value>>().await {
                                        let still_exists = targets.iter().any(|t| {
                                            t.get("id").and_then(|v| v.as_str()) == Some(current_target_id.as_str())
                                        });
                                        let has_better_target = targets.iter().any(|t| {
                                            t.get("type").and_then(|v| v.as_str()) == Some("page")
                                                && t.get("id").and_then(|v| v.as_str()) != Some(current_target_id.as_str())
                                                && t.get("url").and_then(|v| v.as_str()).map_or(false, |u| {
                                                    u != "about:blank" && !u.is_empty() && !u.starts_with("chrome-extension://")
                                                })
                                        });
                                        if !still_exists || (current_target_is_blank && has_better_target) {
                                            debug!(recording_id = %recording_id, "Target closed or better target appeared, switching");
                                            break;
                                        }

                                        // Re-issue startScreencast in case it was interrupted
                                        let _ = write.send(Message::Text(start_screencast.to_string().into())).await;
                                        last_bg_frame_time = Instant::now();
                                    }
                                } else {
                                    // Port died
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let _ = frames_writer.flush().await;
    drop(frames_writer);

    let ended_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let duration_seconds = clock.elapsed().as_secs();
    let size_bytes = compute_rec_dir_size(&rec_dir).await;

    meta.ended_at_secs = Some(ended_at_secs);
    meta.duration_seconds = duration_seconds;
    meta.frame_count = total_frames;
    meta.size_bytes = size_bytes;
    meta.final_url = current_url;
    meta.title = current_title;
    meta.status = "completed".to_string();

    let _ = write_meta(&meta_file, &meta).await;

    info!(
        recording_id = %recording_id,
        frames = total_frames,
        duration = duration_seconds,
        size_bytes = size_bytes,
        "Finalized session recording"
    );

    // If no frames were captured at all, remove the empty directory to save disk clutter
    if total_frames == 0 && duration_seconds < 2 {
        let _ = fs::remove_dir_all(&rec_dir).await;
        debug!(recording_id = %recording_id, "Removed empty recording directory");
    } else {
        // Trigger auto-vacuum
        vacuum_recordings(&config).await;
    }
}

async fn write_meta(path: &Path, meta: &RecordingMeta) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(meta)?;
    fs::write(path, json).await
}

async fn compute_rec_dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(mut entries) = fs::read_dir(path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(m) = entry.metadata().await {
                if m.is_file() {
                    total += m.len();
                }
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Auto-Vacuum & Query Functions
// ---------------------------------------------------------------------------

pub async fn list_recordings(config: &RecorderConfig) -> Vec<RecordingMeta> {
    let mut list = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&config.recordings_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                let meta_file = entry.path().join("meta.json");
                if let Ok(content) = fs::read_to_string(&meta_file).await {
                    if let Ok(meta) = serde_json::from_str::<RecordingMeta>(&content) {
                        list.push(meta);
                    }
                }
            }
        }
    }

    list.sort_by(|a, b| b.started_at_secs.cmp(&a.started_at_secs));
    list
}

/// Recordings still marked "recording" at startup belong to a previous
/// daemon process that died mid-recording.
pub async fn mark_orphaned_recordings(config: &RecorderConfig) -> usize {
    let mut count = 0;
    for mut meta in list_recordings(config).await {
        if meta.status != "recording" {
            continue;
        }
        meta.status = "interrupted".to_string();
        meta.ended_at_secs = Some(meta.started_at_secs + meta.duration_seconds);
        let meta_file = config
            .recordings_dir
            .join(meta.id.to_string())
            .join("meta.json");
        if write_meta(&meta_file, &meta).await.is_ok() {
            count += 1;
        }
    }
    count
}

pub async fn get_recording(config: &RecorderConfig, id: Uuid) -> Option<RecordingMeta> {
    let meta_file = config.recordings_dir.join(id.to_string()).join("meta.json");
    if let Ok(content) = fs::read_to_string(&meta_file).await {
        serde_json::from_str::<RecordingMeta>(&content).ok()
    } else {
        None
    }
}

pub async fn delete_recording(config: &RecorderConfig, id: Uuid) -> bool {
    let rec_dir = config.recordings_dir.join(id.to_string());
    if rec_dir.exists() {
        fs::remove_dir_all(&rec_dir).await.is_ok()
    } else {
        false
    }
}

#[derive(Serialize, Debug)]
pub struct VacuumStats {
    pub vacuumed_count: usize,
    pub freed_bytes: u64,
    pub current_storage_bytes: u64,
    pub max_storage_bytes: u64,
}

pub async fn vacuum_recordings(config: &RecorderConfig) -> VacuumStats {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    struct RecItem {
        _id: Uuid,
        path: PathBuf,
        started_at: u64,
        size_bytes: u64,
        is_active: bool,
    }

    let mut items: Vec<RecItem> = Vec::new();
    let mut current_total_bytes = 0u64;

    if let Ok(mut entries) = fs::read_dir(&config.recordings_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                let rec_path = entry.path();
                let meta_file = rec_path.join("meta.json");
                if let Ok(content) = fs::read_to_string(&meta_file).await {
                    if let Ok(meta) = serde_json::from_str::<RecordingMeta>(&content) {
                        let size = compute_rec_dir_size(&rec_path).await;
                        current_total_bytes += size;
                        let is_active = meta.status == "recording"
                            && now.saturating_sub(meta.started_at_secs) < 86400;
                        items.push(RecItem {
                            _id: meta.id,
                            path: rec_path,
                            started_at: meta.started_at_secs,
                            size_bytes: size,
                            is_active,
                        });
                    }
                }
            }
        }
    }

    let mut to_delete = Vec::new();

    // 1. Prune recordings older than max_age_secs (skip active recordings)
    items.retain(|item| {
        if !item.is_active && now.saturating_sub(item.started_at) > config.max_age_secs {
            to_delete.push((item.path.clone(), item.size_bytes));
            false
        } else {
            true
        }
    });

    // 2. Sort remaining non-active by age (oldest first)
    items.sort_by_key(|i| i.started_at);

    // 3. Prune if total storage or count exceeds quota
    let mut remaining_storage: u64 = items.iter().map(|i| i.size_bytes).sum();
    for item in items {
        let over_size = remaining_storage > config.max_storage_bytes;
        let over_count = to_delete.len() + 1 > config.max_count;
        if (over_size || over_count) && !item.is_active {
            remaining_storage = remaining_storage.saturating_sub(item.size_bytes);
            to_delete.push((item.path, item.size_bytes));
        }
    }

    let mut vacuumed_count = 0;
    let mut freed_bytes = 0;

    for (p, s) in to_delete {
        if let Ok(_) = fs::remove_dir_all(&p).await {
            vacuumed_count += 1;
            freed_bytes += s;
        }
    }

    if vacuumed_count > 0 {
        info!(
            vacuumed = vacuumed_count,
            freed_mb = freed_bytes / 1024 / 1024,
            current_mb = (current_total_bytes.saturating_sub(freed_bytes)) / 1024 / 1024,
            "Auto-vacuum completed"
        );
    }

    VacuumStats {
        vacuumed_count,
        freed_bytes,
        current_storage_bytes: current_total_bytes.saturating_sub(freed_bytes),
        max_storage_bytes: config.max_storage_bytes,
    }
}
