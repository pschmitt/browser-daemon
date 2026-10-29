use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

mod playback;
pub use playback::{playback_window, RecordingFrame, RecordingMeta};

#[derive(Clone, Debug)]
pub struct RecorderConfig {
    pub enabled: bool,
    pub recordings_dir: PathBuf,
    pub max_storage_bytes: u64,
    pub max_age_secs: u64,
    pub max_count: usize,
    /// Record the session's sound too, unless a recording is started without.
    pub audio: bool,
    /// ffmpeg, to join and merge recordings' audio.
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProxyRecordingFrame {
    pub url: Option<String>,
    pub title: Option<String>,
    pub data: String, // base64 JPEG
}

/// A proxied frame with the recording time it arrived at.
struct StampedFrame {
    t: u64,
    frame: ProxyRecordingFrame,
}

/// Frames waiting for the recorder: a slow disk drops frames, it does not grow memory.
const FRAME_QUEUE: usize = 8;

/// Where the handoff view's frames (and Firefox's BiDi screenshots) go: stamped
/// with the recording's clock when they arrive here, thinned to the recording's
/// frame rate, dropped when the recorder cannot keep up.
#[derive(Clone)]
pub struct FrameSender {
    tx: tokio::sync::mpsc::Sender<StampedFrame>,
    clock: Arc<RecordingClock>,
    last: Arc<Mutex<Option<(Instant, u64)>>>,
}

/// The recorder is gone.
#[derive(Debug)]
pub struct RecorderClosed;

impl FrameSender {
    /// Offer a frame. Ok also when it was dropped on purpose (paused, too soon after
    /// the last one, unchanged, queue full); Err once the recording has ended.
    pub fn send(&self, frame: ProxyRecordingFrame) -> Result<(), RecorderClosed> {
        if self.tx.is_closed() {
            return Err(RecorderClosed);
        }
        if self.clock.is_paused() {
            return Ok(());
        }
        let fingerprint = frame_fingerprint(&frame.data);
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if let Some((at, fp)) = *last {
                if now.duration_since(at) < RECORD_FRAME_INTERVAL || fp == fingerprint {
                    return Ok(());
                }
            }
            *last = Some((now, fingerprint));
        }
        let stamped = StampedFrame {
            t: duration_ms(self.clock.elapsed()),
            frame,
        };
        match self.tx.try_send(stamped) {
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(RecorderClosed),
            _ => Ok(()),
        }
    }
}

/// Cheap identity of a frame's data, to skip a picture shown twice.
fn frame_fingerprint(data: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.len().hash(&mut h);
    data.hash(&mut h);
    h.finish()
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone)]
pub struct SessionRecorderHandle {
    pub recording_id: Uuid,
    pub stop_flag: Arc<AtomicBool>,
    pub clock: Arc<RecordingClock>,
    pub frame_tx: FrameSender,
    /// Whether the sound is being recorded; None when the session has none.
    pub sound: Option<Arc<AtomicBool>>,
}

impl SessionRecorderHandle {
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }

    /// Recording the session's sound right now.
    pub fn audio_on(&self) -> bool {
        self.sound
            .as_ref()
            .is_some_and(|s| s.load(Ordering::SeqCst))
    }

    /// Switch the sound on or off while recording; false when there is none.
    pub fn set_audio(&self, on: bool) -> bool {
        let Some(ref sound) = self.sound else {
            return false;
        };
        sound.store(on, Ordering::SeqCst);
        self.clock.wake();
        true
    }
}

// ---------------------------------------------------------------------------
// What this process is working on
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    /// Recordings being written: recording id -> session.
    active: HashMap<Uuid, Uuid>,
    /// The stop flag of each session's recorder, so a second one replaces the first.
    stops: HashMap<Uuid, (Uuid, Arc<AtomicBool>)>,
    /// Recordings an export, merge or salvage is reading: how many each.
    busy: HashMap<Uuid, usize>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Is this recording being written by a recorder of this process right now?
/// (A `status: recording` in `meta.json` alone proves nothing: a crash leaves it.)
pub fn is_active(id: Uuid) -> bool {
    registry().active.contains_key(&id)
}

/// Is an export, merge or salvage working on this recording? Then the cleanup,
/// a delete and a merge leave it alone.
pub fn is_busy(id: Uuid) -> bool {
    registry().busy.contains_key(&id)
}

/// Marks recordings busy until dropped.
pub struct BusyGuard(Vec<Uuid>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let mut reg = registry();
        for id in &self.0 {
            if let Some(n) = reg.busy.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    reg.busy.remove(id);
                }
            }
        }
    }
}

/// Mark `ids` busy. None when one of them is being recorded.
pub fn mark_busy(ids: &[Uuid]) -> Option<BusyGuard> {
    let mut reg = registry();
    if ids.iter().any(|id| reg.active.contains_key(id)) {
        return None;
    }
    for id in ids {
        *reg.busy.entry(*id).or_default() += 1;
    }
    Some(BusyGuard(ids.to_vec()))
}

/// Registered while a recorder runs.
struct ActiveGuard {
    id: Uuid,
    session: Uuid,
    stop: Arc<AtomicBool>,
}

impl ActiveGuard {
    /// Registers the recording; the session's earlier recorder, if any, is told to stop.
    fn new(id: Uuid, session: Uuid, stop: Arc<AtomicBool>) -> Self {
        let mut reg = registry();
        if let Some((_, old)) = reg.stops.insert(session, (id, stop.clone())) {
            old.store(true, Ordering::SeqCst);
        }
        reg.active.insert(id, session);
        Self { id, session, stop }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut reg = registry();
        reg.active.remove(&self.id);
        if reg
            .stops
            .get(&self.session)
            .is_some_and(|(id, _)| *id == self.id)
        {
            reg.stops.remove(&self.session);
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Recording time that stands still while the recording is paused, so a
/// pause leaves no frozen gap in the playback.
pub struct RecordingClock {
    start: Instant,
    state: std::sync::Mutex<PauseState>,
    /// Woken on every pause and resume (and by the recorder when it ends), so the
    /// sound follows the clock without polling it.
    changed: tokio::sync::Notify,
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
            changed: tokio::sync::Notify::new(),
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
        {
            let mut st = self.lock();
            match (paused, st.paused_since) {
                (true, None) => st.paused_since = Some(Instant::now()),
                (false, Some(since)) => {
                    st.paused_total += since.elapsed();
                    st.paused_since = None;
                }
                _ => return false,
            }
        }
        self.changed.notify_waiters();
        true
    }

    pub fn elapsed(&self) -> Duration {
        let st = self.lock();
        let paused = st.paused_total + st.paused_since.map_or(Duration::ZERO, |s| s.elapsed());
        self.start.elapsed().saturating_sub(paused)
    }

    /// Completes at the next pause, resume or [`wake`](Self::wake). Create it
    /// before looking at the state, so a change in between is not missed.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }

    pub fn wake(&self) {
        self.changed.notify_waiters();
    }
}

pub fn start_recording(
    session_id: Uuid,
    // The browser's CDP port for recording while nobody watches; None: only the
    // frames sent to `frame_tx` (a shared browser's tab, see `extension`).
    port: Option<u16>,
    profile: Option<String>,
    node_name: String,
    config: RecorderConfig,
    grab: Option<DisplayGrab>,
    // The session's sound, and whether to record it from the start (it can be
    // switched later).
    audio: Option<(crate::rec_audio::Source, bool)>,
) -> SessionRecorderHandle {
    let (audio, sound) = match audio {
        Some((src, on)) => (Some(src), Some(Arc::new(AtomicBool::new(on)))),
        None => (None, None),
    };
    let sound_task = sound.clone();
    let recording_id = Uuid::new_v4();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let clock = Arc::new(RecordingClock::new());
    let clock_clone = clock.clone();
    let (tx, frame_rx) = tokio::sync::mpsc::channel::<StampedFrame>(FRAME_QUEUE);
    let frame_tx = FrameSender {
        tx,
        clock: clock.clone(),
        last: Arc::default(),
    };
    // Registered here, not in the task: the next call must already see this recorder.
    let active = ActiveGuard::new(recording_id, session_id, stop_flag.clone());

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
            grab,
            audio.zip(sound_task),
            active,
        )
        .await;
    });

    SessionRecorderHandle {
        recording_id,
        stop_flag,
        clock,
        frame_tx,
        sound,
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_single_frame(
    writer: &mut tokio::fs::File,
    meta: &mut RecordingMeta,
    meta_file: &Path,
    rec_dir: &Path,
    total_frames: &mut u64,
    last_meta_flush: &mut Instant,
    clock: &RecordingClock,
    last_t: &mut u64,
    current_url: &mut Option<String>,
    current_title: &mut Option<String>,
    data: String,
    url: Option<String>,
    title: Option<String>,
    // The recording time the frame arrived at, if it was stamped on the way here.
    stamp: Option<u64>,
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

    // A frame stamped before a pause still belongs to the time before it.
    if stamp.is_none() && clock.is_paused() {
        return;
    }
    let elapsed_ms = frame_time(
        stamp.unwrap_or_else(|| duration_ms(clock.elapsed())),
        *last_t,
    );
    *last_t = elapsed_ms;
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
    if last_meta_flush.elapsed() > Duration::from_secs(4) || total_frames.is_multiple_of(30) {
        let _ = writer.flush().await;
        let elapsed = clock.elapsed();
        meta.duration_seconds = elapsed.as_secs();
        meta.duration_ms = duration_ms(elapsed);
        meta.frame_count = *total_frames;
        meta.size_bytes = compute_rec_dir_size(rec_dir).await;
        meta.final_url = current_url.clone();
        meta.title = current_title.clone();
        let _ = write_meta(meta_file, meta).await;
        *last_meta_flush = Instant::now();
    }
}

/// Frame times never go back, whatever order frames from different sources arrive in.
fn frame_time(t: u64, last: u64) -> u64 {
    t.max(last)
}

/// Chromium reports a blank tab's URL as its title; that is no title.
fn is_real_title(title: &str) -> bool {
    !title.is_empty() && title != "about:blank"
}

// CDP request ids must fit an int32 (Chromium rejects larger ones), so the ranges
// for our own requests sit high but below 2^31.
/// CDP request ids from here up to LAYOUT_REQUEST_ID ask for `document.title`.
const TITLE_REQUEST_ID: u64 = 1 << 28;
/// Re-read the title this often: single-page apps retitle without navigating.
const TITLE_REFRESH: Duration = Duration::from_secs(5);
/// Recordings are for replaying what happened, not for smooth video: 4 frames a second.
/// They are screenshots, not a screencast: an active screencast makes Chromium capture
/// every rendered frame, ~70% of a core on an animated page however few frames it
/// delivers; four screenshots a second cost ~15%.
const RECORD_FRAME_INTERVAL: Duration = Duration::from_millis(250);
const LAYOUT_REQUEST_ID: u64 = 1 << 29;
/// From here up to TITLE_REQUEST_ID: the page's size and toolbar height, for
/// grabbing the page's area from the display (see DisplayGrab).
const GEOMETRY_REQUEST_ID: u64 = 1 << 27;
/// CDP request id of the fetch of the page's icon (below GEOMETRY_REQUEST_ID).
const FAVICON_REQUEST_ID: u64 = 1 << 26;
/// Larger icons are not worth keeping.
const FAVICON_MAX_BYTES: usize = 128 * 1024;
/// The page's size on screen (scrollbars included; a zoomed-out mobile page scaled
/// down) and the toolbars' height (null under emulation, where outer and inner sizes
/// don't belong together; then the last one stands).
const GEOMETRY_EXPR: &str = "(() => { const s = Math.min(visualViewport.scale, 1); return [Math.round(innerWidth * s), Math.round(innerHeight * s), outerWidth === innerWidth ? outerHeight - innerHeight : null]; })()";
const SHOT_REQUEST_ID: u64 = 1 << 30;
/// Recorded frames are scaled to fit this box.
const RECORD_MAX: (f64, f64) = (1024.0, 640.0);

#[allow(clippy::too_many_arguments)]
async fn record_session_loop(
    recording_id: Uuid,
    session_id: Uuid,
    port: Option<u16>,
    profile: Option<String>,
    node_name: String,
    config: RecorderConfig,
    stop_flag: Arc<AtomicBool>,
    clock: Arc<RecordingClock>,
    mut frame_rx: tokio::sync::mpsc::Receiver<StampedFrame>,
    grab: Option<DisplayGrab>,
    audio: Option<(crate::rec_audio::Source, Arc<AtomicBool>)>,
    // Registered until the recording is final.
    active: ActiveGuard,
) {
    let rec_dir = config.recordings_dir.join(recording_id.to_string());
    if let Err(e) = make_rec_dir(&config, &rec_dir).await {
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
        duration_ms: 0,
        frame_count: 0,
        size_bytes: 0,
        initial_url: None,
        final_url: None,
        title: None,
        favicon_url: None,
        favicon_type: None,
        keep: false,
        status: "recording".to_string(),
        audio: false,
        merged_from: Vec::new(),
        merged_at_secs: None,
    };

    let _ = write_meta(&meta_file, &meta).await;

    // The sound, on the same clock; stopped once the frames are done (the loop
    // also ends without the stop flag, when the session goes away).
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_task = audio.map(|(src, sound)| {
        crate::rec_audio::spawn(
            src,
            rec_dir.clone(),
            clock.clone(),
            audio_stop.clone(),
            sound,
        )
    });

    info!(
        recording_id = %recording_id,
        session_id = %session_id,
        port = ?port,
        "Started session recording"
    );

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    // Open frames file
    let mut frames_writer = match crate::persist::private_append(&frames_file).await {
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
    // The icon URL last asked the page for, so each is tried once.
    let mut favicon_fetched: Option<String> = None;
    let mut total_frames: u64 = 0;
    // When the handoff view last sent a frame (none yet).
    let mut last_proxy_frame: Option<Instant> = None;
    // The proxy channel closed: nobody can send any more (the session is going away).
    let mut channel_closed = false;
    let mut last_t: u64 = 0;
    // With a display of its own: frames grabbed from it (DisplayGrab), the newest
    // one picked up per tick; (toolbar height, width, height) of the page on it.
    let mut grabber: Option<Grabber> = None;
    let mut last_grab: Vec<u8> = Vec::new();
    let mut geometry: Option<(u32, u32, u32)> = None;
    let mut chrome_h: u32 = 87;
    let mut last_meta_flush = Instant::now();

    // Main recording loop
    while !stop_flag.load(Ordering::SeqCst) {
        // 1. Drain any proxy frames that arrived from handoff.html
        while let Ok(pf) = frame_rx.try_recv() {
            last_proxy_frame = Some(Instant::now());
            write_single_frame(
                &mut frames_writer,
                &mut meta,
                &meta_file,
                &rec_dir,
                &mut total_frames,
                &mut last_meta_flush,
                &clock,
                &mut last_t,
                &mut current_url,
                &mut current_title,
                pf.frame.data,
                pf.frame.url,
                pf.frame.title,
                Some(pf.t),
            )
            .await;
        }

        // 2. If handoff.html is actively streaming (frames received in last 2 seconds),
        // wait for more proxy frames without competing on background screencast!
        if port.is_none()
            || last_proxy_frame.is_some_and(|at| at.elapsed() < Duration::from_secs(2))
        {
            match tokio::time::timeout(Duration::from_millis(500), frame_rx.recv()).await {
                Ok(Some(pf)) => {
                    last_proxy_frame = Some(Instant::now());
                    write_single_frame(
                        &mut frames_writer,
                        &mut meta,
                        &meta_file,
                        &rec_dir,
                        &mut total_frames,
                        &mut last_meta_flush,
                        &clock,
                        &mut last_t,
                        &mut current_url,
                        &mut current_title,
                        pf.frame.data,
                        pf.frame.url,
                        pf.frame.title,
                        Some(pf.t),
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

        // 3. Otherwise, record with timed screenshots while nobody watches
        let Some(port) = port else { continue };
        let targets_url = format!("http://127.0.0.1:{}/json/list", port);
        let active_page_target = match http_client.get(&targets_url).send().await {
            Ok(res) => {
                if let Ok(targets) = res.json::<Vec<serde_json::Value>>().await {
                    pick_visible(page_candidates(targets)).await
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
            .is_none_or(|u| u == "about:blank" || u.is_empty());

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
        if let Some(f) = real_favicon(&target) {
            meta.favicon_url = Some(f);
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

        // Enable the Page domain (navigation events)
        let enable_cmd = serde_json::json!({
            "id": 1,
            "method": "Page.enable"
        });
        let _ = write
            .send(Message::Text(enable_cmd.to_string().into()))
            .await;

        let mut last_bg_frame_time = Instant::now();
        // The user may switch tabs: follow the visible one.
        let mut tab_check = tokio::time::interval(TAB_CHECK_INTERVAL);
        tab_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut shot_timer = tokio::time::interval(RECORD_FRAME_INTERVAL);
        shot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_shot_id = SHOT_REQUEST_ID;
        let mut shot_in_flight = false;
        let mut last_shot = String::new();
        // CSS viewport size, for scaling screenshots down to RECORD_MAX.
        let mut viewport: Option<(f64, f64)> = None;
        let mut next_layout_id = LAYOUT_REQUEST_ID;
        let _ = write
            .send(Message::Text(
                serde_json::json!({ "id": next_layout_id, "method": "Page.getLayoutMetrics" })
                    .to_string()
                    .into(),
            ))
            .await;
        let mut next_geometry_id = GEOMETRY_REQUEST_ID;
        if grab.is_some() {
            let _ = write.send(Message::Text(serde_json::json!({ "id": next_geometry_id, "method": "Runtime.evaluate", "params": { "expression": GEOMETRY_EXPR, "returnByValue": true } }).to_string().into())).await;
        }
        let mut next_title_id = TITLE_REQUEST_ID;
        // The target's title above may predate a navigation; ask right away.
        let mut last_title_request = Instant::now()
            .checked_sub(TITLE_REFRESH)
            .unwrap_or_else(Instant::now);

        // Inner frame loop for this page
        while !stop_flag.load(Ordering::SeqCst) {
            tokio::select! {
                // If proxy frame arrives, handoff.html was connected!
                proxy_frame = frame_rx.recv() => {
                    match proxy_frame {
                        Some(pf) => {
                            last_proxy_frame = Some(Instant::now());
                            write_single_frame(
                                &mut frames_writer,
                                &mut meta,
                                &meta_file,
                                &rec_dir,
                                &mut total_frames,
                                &mut last_meta_flush,
                                &clock,
                                &mut last_t,
                                &mut current_url,
                                &mut current_title,
                                pf.frame.data,
                                pf.frame.url,
                                pf.frame.title,
                                Some(pf.t),
                            ).await;
                            // Yield background connection to handoff
                            break;
                        }
                        None => {
                            channel_closed = true;
                            break;
                        }
                    }
                }
                _ = tab_check.tick() => {
                    let Ok(res) = http_client.get(&targets_url).send().await else { continue };
                    let Ok(targets) = res.json::<Vec<serde_json::Value>>().await else { continue };
                    let Some(best) = pick_visible(page_candidates(targets)).await else { continue };
                    if best.get("id").and_then(|v| v.as_str()) != Some(current_target_id.as_str()) {
                        debug!(recording_id = %recording_id, "Visible tab changed, switching");
                        break;
                    }
                    // The page declares its icon once loaded: keep a copy from the site itself.
                    if let Some(f) = real_favicon(&best) {
                        if favicon_fetched.as_deref() != Some(f.as_str()) {
                            let expr = format!(
                                "fetch({}, {{ credentials: 'include' }}).then((r) => r.ok ? r.blob() : null).then((b) => !b || b.size > {} ? null : new Promise((ok) => {{ const f = new FileReader(); f.onload = () => ok(f.result); f.readAsDataURL(b); }})).catch(() => null)",
                                serde_json::Value::String(f.clone()),
                                FAVICON_MAX_BYTES
                            );
                            let cmd = serde_json::json!({
                                "id": FAVICON_REQUEST_ID,
                                "method": "Runtime.evaluate",
                                "params": { "expression": expr, "awaitPromise": true, "returnByValue": true }
                            });
                            let _ = write.send(Message::Text(cmd.to_string().into())).await;
                            favicon_fetched = Some(f.clone());
                        }
                        meta.favicon_url = Some(f);
                    }
                }
                _ = shot_timer.tick() => {
                    if let Some(ref g) = grab {
                        // From the display: no screenshot, so no repaint in the live view.
                        let want = geometry.and_then(|(ch, w, h)| g.region(ch, w, h));
                        if want.is_some() && grabber.as_ref().map(|gr| gr.region) != want {
                            grabber = want.and_then(|r| g.start(r));
                        }
                        let frame = grabber.as_ref().and_then(|gr| gr.latest.lock().unwrap().take());
                        if let Some(jpeg) = frame.filter(|f| *f != last_grab) {
                            use base64::Engine as _;
                            let data = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                            last_grab = jpeg;
                            last_bg_frame_time = Instant::now();
                            write_single_frame(
                                &mut frames_writer,
                                &mut meta,
                                &meta_file,
                                &rec_dir,
                                &mut total_frames,
                                &mut last_meta_flush,
                                &clock,
                                &mut last_t,
                                &mut current_url,
                                &mut current_title,
                                data,
                                None,
                                None,
                                None,
                            ).await;
                        }
                    } else if !shot_in_flight {
                        next_shot_id += 1;
                        let mut params = serde_json::json!({ "format": "jpeg", "quality": 60 });
                        if let Some((w, h)) = viewport {
                            let scale = (RECORD_MAX.0 / w).min(RECORD_MAX.1 / h).min(1.0);
                            params["clip"] = serde_json::json!({ "x": 0, "y": 0, "width": w, "height": h, "scale": scale });
                        }
                        let cmd = serde_json::json!({ "id": next_shot_id, "method": "Page.captureScreenshot", "params": params });
                        shot_in_flight = write.send(Message::Text(cmd.to_string().into())).await.is_ok();
                    }
                }
                msg_res = tokio::time::timeout(Duration::from_millis(600), read.next()) => {
                    match msg_res {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                let method = val.get("method").and_then(|v| v.as_str()).unwrap_or("");
                                let reply_id = val.get("id").and_then(|v| v.as_u64());
                                if reply_id.is_some_and(|id| id >= SHOT_REQUEST_ID) {
                                    shot_in_flight = false;
                                    last_bg_frame_time = Instant::now();
                                    // Unchanged picture: nothing to record.
                                    if let Some(data) = val.pointer("/result/data").and_then(|v| v.as_str()) {
                                        if data != last_shot {
                                            last_shot = data.to_string();
                                            write_single_frame(
                                                &mut frames_writer,
                                                &mut meta,
                                                &meta_file,
                                                &rec_dir,
                                                &mut total_frames,
                                                &mut last_meta_flush,
                                                &clock,
                                                &mut last_t,
                                                &mut current_url,
                                                &mut current_title,
                                                last_shot.clone(),
                                                None,
                                                None,
                                                None,
                                            ).await;
                                        }
                                    }
                                    continue;
                                }
                                if reply_id == Some(FAVICON_REQUEST_ID) {
                                    if let Some((mime, bytes)) = val
                                        .pointer("/result/result/value")
                                        .and_then(|v| v.as_str())
                                        .and_then(parse_data_url)
                                    {
                                        if fs::write(rec_dir.join("favicon.bin"), &bytes).await.is_ok() {
                                            meta.favicon_type = Some(mime);
                                        }
                                    }
                                    continue;
                                }
                                if reply_id.is_some_and(|id| (GEOMETRY_REQUEST_ID..TITLE_REQUEST_ID).contains(&id)) {
                                    if let Some(v) = val.pointer("/result/result/value").and_then(|v| v.as_array()) {
                                        let n = |i: usize| v.get(i).and_then(serde_json::Value::as_u64).and_then(|x| u32::try_from(x).ok());
                                        if let Some(ch) = n(2).filter(|ch| (1..400).contains(ch)) {
                                            chrome_h = ch;
                                        }
                                        if let (Some(w), Some(h)) = (n(0), n(1)) {
                                            geometry = Some((chrome_h, w, h));
                                        }
                                    }
                                    continue;
                                }
                                if reply_id.is_some_and(|id| (LAYOUT_REQUEST_ID..SHOT_REQUEST_ID).contains(&id)) {
                                    let vp = val.pointer("/result/cssLayoutViewport");
                                    let w = vp.and_then(|v| v.get("clientWidth")).and_then(|v| v.as_f64());
                                    let h = vp.and_then(|v| v.get("clientHeight")).and_then(|v| v.as_f64());
                                    if let (Some(w), Some(h)) = (w, h) {
                                        if w > 0.0 && h > 0.0 {
                                            viewport = Some((w, h));
                                        }
                                    }
                                    continue;
                                }
                                if reply_id.is_some_and(|id| (TITLE_REQUEST_ID..LAYOUT_REQUEST_ID).contains(&id)) {
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
                                // A resize (window, emulation) changes what the screenshots clip.
                                let resized = method == "Page.frameResized";
                                if navigated || resized || last_title_request.elapsed() >= TITLE_REFRESH {
                                    next_title_id += 1;
                                    let title_cmd = serde_json::json!({
                                        "id": next_title_id,
                                        "method": "Runtime.evaluate",
                                        "params": { "expression": "document.title", "returnByValue": true }
                                    });
                                    let _ = write.send(Message::Text(title_cmd.to_string().into())).await;
                                    last_title_request = Instant::now();
                                    // The viewport may have changed too (resize, emulation).
                                    next_layout_id += 1;
                                    let layout_cmd = serde_json::json!({ "id": next_layout_id, "method": "Page.getLayoutMetrics" });
                                    let _ = write.send(Message::Text(layout_cmd.to_string().into())).await;
                                    if grab.is_some() {
                                        next_geometry_id += 1;
                                        let _ = write.send(Message::Text(serde_json::json!({ "id": next_geometry_id, "method": "Runtime.evaluate", "params": { "expression": GEOMETRY_EXPR, "returnByValue": true } }).to_string().into())).await;
                                    }
                                }
                                if method == "Page.frameNavigated" {
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
                        // Timeout hit (no messages for 600ms)
                        Err(_) if last_bg_frame_time.elapsed() > Duration::from_secs(2) => {
                            let check_url = format!("http://127.0.0.1:{}/json/list", port);
                            if let Ok(res) = http_client.get(&check_url).send().await {
                                if let Ok(targets) = res.json::<Vec<serde_json::Value>>().await {
                                    let still_exists = targets.iter().any(|t| {
                                        t.get("id").and_then(|v| v.as_str()) == Some(current_target_id.as_str())
                                    });
                                    let has_better_target = targets.iter().any(|t| {
                                        t.get("type").and_then(|v| v.as_str()) == Some("page")
                                            && t.get("id").and_then(|v| v.as_str()) != Some(current_target_id.as_str())
                                            && t.get("url").and_then(|v| v.as_str()).is_some_and(|u| {
                                                u != "about:blank" && !u.is_empty() && !u.starts_with("chrome-extension://")
                                            })
                                    });
                                    if !still_exists || (current_target_is_blank && has_better_target) {
                                        debug!(recording_id = %recording_id, "Target closed or better target appeared, switching");
                                        break;
                                    }

                                    // A screenshot got lost (navigation): ask again.
                                    shot_in_flight = false;
                                    last_bg_frame_time = Instant::now();
                                }
                            } else {
                                // Port died
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if channel_closed {
            break;
        }
    }

    let _ = frames_writer.flush().await;
    drop(frames_writer);

    // Before the audio is joined, which takes a moment of the clock.
    let length = clock.elapsed();
    let duration_seconds = length.as_secs();
    audio_stop.store(true, Ordering::SeqCst);
    clock.wake();
    if let Some(task) = audio_task {
        meta.audio = task.await.unwrap_or(false);
    }

    let ended_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let size_bytes = compute_rec_dir_size(&rec_dir).await;

    meta.ended_at_secs = Some(ended_at_secs);
    meta.duration_seconds = duration_seconds;
    meta.duration_ms = duration_ms(length);
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

    // Final: from here on the cleanup may treat it like any other recording.
    drop(active);

    // If no frames were captured at all, remove the empty directory to save disk clutter
    if total_frames == 0 && duration_seconds < 2 {
        let _ = fs::remove_dir_all(&rec_dir).await;
        debug!(recording_id = %recording_id, "Removed empty recording directory");
    } else {
        // Trigger auto-vacuum
        vacuum_recordings(&config).await;
    }
}

/// Content type and bytes of a `data:<type>;base64,<data>` URL (an image, within the size limit).
fn parse_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    use base64::Engine as _;
    let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
    let mime = head.strip_suffix(";base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    if bytes.is_empty() || bytes.len() > FAVICON_MAX_BYTES {
        return None;
    }
    let mime = if mime.starts_with("image/") {
        mime
    } else {
        "image/x-icon"
    };
    Some((mime.to_string(), bytes))
}

/// How often the recorder checks which tab the user is looking at.
const TAB_CHECK_INTERVAL: Duration = Duration::from_millis(1500);

/// The page targets worth recording, real pages first.
pub(crate) fn page_candidates(targets: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut pages: Vec<serde_json::Value> = targets
        .into_iter()
        .filter(|t| {
            t.get("type").and_then(|v| v.as_str()) == Some("page")
                && t.get("webSocketDebuggerUrl").is_some()
        })
        .collect();
    pages.sort_by_key(|t| u8::from(is_blank_target(t)));
    pages
}

fn is_blank_target(t: &serde_json::Value) -> bool {
    let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
    url.is_empty() || url == "about:blank" || url.starts_with("chrome-extension://")
}

/// The target's own icon (`faviconUrl` in the target list), when it has a plain one.
fn real_favicon(t: &serde_json::Value) -> Option<String> {
    let f = t.get("faviconUrl")?.as_str()?;
    (!f.is_empty() && f.len() <= 2048 && !f.starts_with("data:")).then(|| f.to_string())
}

/// The tab the user is looking at. Chromium lists targets in no useful order, so
/// with several real pages each is asked whether its document is visible; without
/// an answer the first one stands.
pub(crate) async fn pick_visible(pages: Vec<serde_json::Value>) -> Option<serde_json::Value> {
    let real = pages.iter().take_while(|t| !is_blank_target(t)).count();
    if real > 1 {
        for t in pages.iter().take(real) {
            if let Some(ws) = t.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
                if is_visible(ws).await {
                    return Some(t.clone());
                }
            }
        }
    }
    pages.into_iter().next()
}

/// Whether the page at this DevTools URL reports `document.visibilityState` visible.
pub(crate) async fn is_visible(ws_url: &str) -> bool {
    let probe = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.ok()?;
        let cmd = serde_json::json!({
            "id": 1,
            "method": "Runtime.evaluate",
            "params": { "expression": "document.visibilityState", "returnByValue": true }
        });
        ws.send(Message::Text(cmd.to_string().into())).await.ok()?;
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                let val: serde_json::Value = serde_json::from_str(&text).ok()?;
                if val.get("id").and_then(|v| v.as_u64()) == Some(1) {
                    return Some(
                        val.pointer("/result/result/value").and_then(|v| v.as_str())
                            == Some("visible"),
                    );
                }
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_millis(500), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

async fn write_meta(path: &Path, meta: &RecordingMeta) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(meta)?;
    crate::persist::write_atomic(path, &json).await
}

/// Size of everything in a recording's directory: frames, sound, the parts of a
/// sound still being recorded and the work files of an export.
async fn compute_rec_dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut dirs = vec![path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(m) = entry.metadata().await else {
                continue;
            };
            if m.is_dir() {
                dirs.push(entry.path());
            } else if m.is_file() {
                total += m.len();
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Auto-Vacuum & Query Functions
// ---------------------------------------------------------------------------

/// Marker file of a recording that is kept.
const KEEP_FILE: &str = "keep";
/// Present in a merge's directory until the merge is complete.
const MERGING_FILE: &str = ".merging";

/// A recording's directory, and the recordings directory above it, for the
/// daemon's user only: the frames show whatever the sessions showed.
async fn make_rec_dir(config: &RecorderConfig, dir: &Path) -> std::io::Result<()> {
    crate::persist::private_dir(&config.recordings_dir).await?;
    crate::persist::private_dir(dir).await
}

fn rec_dir_of(config: &RecorderConfig, id: Uuid) -> PathBuf {
    config.recordings_dir.join(id.to_string())
}

/// Mark a recording to be kept (or not): the cleanup skips it. False when it does not exist.
pub async fn set_keep(config: &RecorderConfig, id: Uuid, keep: bool) -> bool {
    let rec_dir = rec_dir_of(config, id);
    if !fs::try_exists(rec_dir.join("meta.json"))
        .await
        .unwrap_or(false)
    {
        return false;
    }
    let marker = rec_dir.join(KEEP_FILE);
    if keep {
        crate::persist::private_file(&marker).await.is_ok()
    } else {
        match fs::remove_file(&marker).await {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    }
}

/// A recording's metadata rebuilt from its frames, for one whose `meta.json` is
/// gone or unreadable: the frames are the recording, the metadata only describes
/// it. None without a readable frame.
async fn rebuild_meta(dir: &Path, id: Uuid) -> Option<RecordingMeta> {
    use tokio::io::AsyncBufReadExt;
    let file = fs::File::open(dir.join("frames.jsonl")).await.ok()?;
    let ended_at_secs = file
        .metadata()
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut lines = tokio::io::BufReader::new(file).lines();
    let (mut count, mut last_t) = (0u64, 0u64);
    let (mut initial_url, mut final_url, mut title) = (None, None, None);
    // The last line may be cut off by the crash: it is skipped like any other bad one.
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str::<RecordingFrame>(&line) else {
            continue;
        };
        count += 1;
        last_t = last_t.max(frame.t);
        if let Some(u) = frame.url.filter(|u| !u.is_empty() && u != "about:blank") {
            initial_url.get_or_insert_with(|| u.clone());
            final_url = Some(u);
        }
        if let Some(t) = frame.title.filter(|t| is_real_title(t)) {
            title = Some(t);
        }
    }
    if count == 0 {
        return None;
    }
    let audio = fs::try_exists(dir.join(crate::rec_audio::FILE))
        .await
        .unwrap_or(false);
    Some(RecordingMeta {
        id,
        session_id: Uuid::nil(),
        profile: None,
        node: "unknown".to_string(),
        started_at_secs: ended_at_secs.saturating_sub(last_t / 1000),
        ended_at_secs: Some(ended_at_secs),
        duration_seconds: last_t / 1000,
        duration_ms: last_t,
        frame_count: count,
        size_bytes: compute_rec_dir_size(dir).await,
        initial_url,
        final_url,
        title,
        favicon_url: None,
        favicon_type: None,
        keep: false,
        status: "interrupted".to_string(),
        audio,
        merged_from: Vec::new(),
        merged_at_secs: None,
    })
}

/// The metadata of recording `id`, rebuilt from its frames (and written back) when
/// `meta.json` is missing or unreadable and nothing is writing the recording.
async fn load_meta(config: &RecorderConfig, id: Uuid) -> Option<RecordingMeta> {
    let dir = rec_dir_of(config, id);
    let meta_file = dir.join("meta.json");
    match fs::read(&meta_file).await {
        Ok(raw) => {
            if let Ok(meta) = serde_json::from_slice::<RecordingMeta>(&raw) {
                return Some(meta);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    // Being recorded or merged: its metadata is in the making.
    if is_active(id)
        || fs::try_exists(dir.join(MERGING_FILE))
            .await
            .unwrap_or(false)
    {
        return None;
    }
    let meta = rebuild_meta(&dir, id).await?;
    warn!(recording_id = %id, frames = meta.frame_count, "Rebuilt a recording's metadata from its frames");
    let _ = write_meta(&meta_file, &meta).await;
    Some(meta)
}

async fn kept(dir: &Path) -> bool {
    fs::try_exists(dir.join(KEEP_FILE)).await.unwrap_or(false)
}

/// Directories of recordings: those named by an id.
async fn recording_ids(config: &RecorderConfig) -> Vec<Uuid> {
    let mut ids = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&config.recordings_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Ok(id) = Uuid::parse_str(&entry.file_name().to_string_lossy()) {
                ids.push(id);
            }
        }
    }
    ids
}

pub async fn list_recordings(config: &RecorderConfig) -> Vec<RecordingMeta> {
    let mut list = Vec::new();
    for id in recording_ids(config).await {
        if let Some(mut meta) = load_meta(config, id).await {
            meta.keep = kept(&rec_dir_of(config, id)).await;
            list.push(meta);
        }
    }
    list.sort_by_key(|m| std::cmp::Reverse(m.started_at_secs));
    list
}

/// Startup housekeeping in the recordings directory: owner-only permissions,
/// unfinished merges and export work files removed.
async fn tidy_recordings_dir(config: &RecorderConfig) {
    use std::os::unix::fs::PermissionsExt;
    let private = std::fs::Permissions::from_mode(0o700);
    if fs::set_permissions(&config.recordings_dir, private.clone())
        .await
        .is_err()
    {
        return;
    }
    for id in recording_ids(config).await {
        let dir = rec_dir_of(config, id);
        if fs::try_exists(dir.join(MERGING_FILE))
            .await
            .unwrap_or(false)
        {
            warn!(recording_id = %id, "Removing a merge that was never finished");
            let _ = fs::remove_dir_all(&dir).await;
            continue;
        }
        let _ = fs::set_permissions(&dir, private.clone()).await;
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(crate::rec_export::WORK_PREFIX) {
                debug!(recording_id = %id, "Removing the leftovers of an export");
                let _ = fs::remove_dir_all(entry.path()).await;
            } else if name.starts_with(".meta.json.") && name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path()).await;
            }
        }
    }
}

/// Recordings still marked "recording" at startup belong to a previous
/// daemon process that died mid-recording. Their sound is joined in the background.
pub async fn mark_orphaned_recordings(config: &RecorderConfig) -> usize {
    tidy_recordings_dir(config).await;
    let mut count = 0;
    for mut meta in list_recordings(config).await {
        if meta.status != "recording" {
            continue;
        }
        let rec_dir = rec_dir_of(config, meta.id);
        meta.status = "interrupted".to_string();
        meta.ended_at_secs = Some(meta.started_at_secs + meta.length_ms() / 1000);
        meta.audio = fs::try_exists(rec_dir.join(crate::rec_audio::FILE))
            .await
            .unwrap_or(false);
        if write_meta(&rec_dir.join("meta.json"), &meta).await.is_err() {
            continue;
        }
        count += 1;
        if let Some(ffmpeg) = config.ffmpeg.clone() {
            // Joining takes a while per recording: not before the daemon serves.
            let (config, id) = (config.clone(), meta.id);
            tokio::spawn(async move { salvage_audio(&config, &ffmpeg, id).await });
        }
    }
    count
}

/// Join the sound segments an interrupted recording left, and note the result.
async fn salvage_audio(config: &RecorderConfig, ffmpeg: &Path, id: Uuid) {
    let Some(_busy) = mark_busy(&[id]) else {
        return;
    };
    let rec_dir = rec_dir_of(config, id);
    let joined = crate::rec_audio::salvage(ffmpeg, &rec_dir).await
        || fs::try_exists(rec_dir.join(crate::rec_audio::FILE))
            .await
            .unwrap_or(false);
    let Some(mut meta) = load_meta(config, id).await else {
        return;
    };
    meta.audio = joined;
    meta.size_bytes = compute_rec_dir_size(&rec_dir).await;
    let _ = write_meta(&rec_dir.join("meta.json"), &meta).await;
}

pub async fn get_recording(config: &RecorderConfig, id: Uuid) -> Option<RecordingMeta> {
    let mut meta = load_meta(config, id).await?;
    meta.keep = kept(&rec_dir_of(config, id)).await;
    Some(meta)
}

/// False when there is no such recording, or it is being recorded, exported or merged.
pub async fn delete_recording(config: &RecorderConfig, id: Uuid) -> bool {
    if is_active(id) || is_busy(id) {
        return false;
    }
    let rec_dir = rec_dir_of(config, id);
    if fs::try_exists(&rec_dir).await.unwrap_or(false) {
        fs::remove_dir_all(&rec_dir).await.is_ok()
    } else {
        false
    }
}

/// Silence between merged recordings (ms), so the cut shows.
const MERGE_GAP_MS: u64 = 500;

/// Where each of recordings of the given lengths (ms) starts in their merge, and
/// the merge's length: one after the other, a gap between them.
fn merge_plan(lengths: &[u64]) -> (Vec<u64>, u64) {
    let mut starts = Vec::with_capacity(lengths.len());
    let mut at = 0;
    for len in lengths {
        starts.push(at);
        at += len + MERGE_GAP_MS;
    }
    (starts, at.saturating_sub(MERGE_GAP_MS))
}

/// The length of a source in a merge (ms): its recorded length, or its last
/// frame if that is later.
fn merge_length(last_frame_ms: u64, source: &RecordingMeta) -> u64 {
    last_frame_ms.max(source.length_ms())
}

/// Merges recordings, in the order given, into a new one: their frames one after
/// the other on one clock, their sound (if any) where it belongs. With `delete`,
/// the originals are removed once the merge is written (but not when sound would
/// be lost with them). The result is kept if any source was, and its age counts
/// from the merge.
pub async fn merge_recordings(
    config: &RecorderConfig,
    ids: &[Uuid],
    delete: bool,
) -> Result<RecordingMeta, String> {
    use tokio::io::AsyncBufReadExt;
    let mut seen = HashSet::new();
    if ids.len() < 2 || !ids.iter().all(|id| seen.insert(*id)) {
        return Err("Pick at least two different recordings".into());
    }
    let busy =
        mark_busy(ids).ok_or_else(|| "A recording is still running: stop it first".to_string())?;
    let mut sources = Vec::new();
    let mut any_kept = false;
    for id in ids {
        let meta = get_recording(config, *id)
            .await
            .ok_or_else(|| format!("No recording {id} on this node"))?;
        any_kept |= meta.keep;
        sources.push(meta);
    }
    let id = Uuid::new_v4();
    let new_busy = mark_busy(&[id]);
    let dir = rec_dir_of(config, id);
    make_rec_dir(config, &dir)
        .await
        .map_err(|e| e.to_string())?;
    // Until the metadata is there this is no recording: a crash leaves the marker
    // for the next start to clean up.
    let _ = crate::persist::private_file(&dir.join(MERGING_FILE)).await;
    let work = async {
        let mut out = crate::persist::private_file(&dir.join("frames.jsonl"))
            .await
            .map_err(|e| e.to_string())?;
        // Lengths first: the frames of each source go at its start.
        let mut lengths = Vec::new();
        for src in &sources {
            let mut last_t = 0;
            if let Ok(f) = fs::File::open(rec_dir_of(config, src.id).join("frames.jsonl")).await {
                let mut lines = tokio::io::BufReader::new(f).lines();
                while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                    if let Ok(frame) = serde_json::from_str::<RecordingFrame>(&line) {
                        last_t = last_t.max(frame.t);
                    }
                }
            }
            lengths.push(merge_length(last_t, src));
        }
        let (starts, total_ms) = merge_plan(&lengths);
        let mut frames = 0u64;
        let mut audio = Vec::new();
        for (src, offset) in sources.iter().zip(&starts) {
            let src_dir = rec_dir_of(config, src.id);
            if src.audio {
                audio.push((*offset, src_dir.join(crate::rec_audio::FILE)));
            }
            if let Ok(f) = fs::File::open(src_dir.join("frames.jsonl")).await {
                let mut lines = tokio::io::BufReader::new(f).lines();
                while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                    let Ok(mut frame) = serde_json::from_str::<RecordingFrame>(&line) else {
                        continue;
                    };
                    frame.t += offset;
                    let line = serde_json::to_string(&frame).map_err(|e| e.to_string())?;
                    out.write_all(line.as_bytes())
                        .await
                        .map_err(|e| e.to_string())?;
                    out.write_all(b"\n").await.map_err(|e| e.to_string())?;
                    frames += 1;
                }
            }
        }
        out.flush().await.map_err(|e| e.to_string())?;
        drop(out);
        let first = &sources[0];
        let last = &sources[sources.len() - 1];
        let mut meta = RecordingMeta {
            id,
            session_id: first.session_id,
            profile: first.profile.clone(),
            node: first.node.clone(),
            started_at_secs: first.started_at_secs,
            ended_at_secs: last.ended_at_secs,
            duration_seconds: total_ms / 1000,
            duration_ms: total_ms,
            frame_count: frames,
            size_bytes: 0,
            initial_url: first.initial_url.clone(),
            final_url: last.final_url.clone(),
            title: sources.iter().find_map(|s| s.title.clone()),
            favicon_url: None,
            favicon_type: None,
            keep: any_kept,
            status: "completed".to_string(),
            audio: false,
            merged_from: ids.to_vec(),
            merged_at_secs: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
        };
        // The first source's site icon.
        for src in &sources {
            let icon = rec_dir_of(config, src.id).join("favicon.bin");
            if src.favicon_type.is_some() && fs::copy(&icon, dir.join("favicon.bin")).await.is_ok()
            {
                meta.favicon_url = src.favicon_url.clone();
                meta.favicon_type = src.favicon_type.clone();
                break;
            }
        }
        let wanted_audio = !audio.is_empty();
        if let (true, Some(ffmpeg)) = (wanted_audio, config.ffmpeg.as_ref()) {
            meta.audio =
                crate::rec_audio::join(ffmpeg, &audio, &dir.join(crate::rec_audio::FILE)).await;
        }
        if any_kept {
            crate::persist::private_file(&dir.join(KEEP_FILE))
                .await
                .map_err(|e| e.to_string())?;
        }
        meta.size_bytes = compute_rec_dir_size(&dir).await;
        write_meta(&dir.join("meta.json"), &meta)
            .await
            .map_err(|e| e.to_string())?;
        let _ = fs::remove_file(dir.join(MERGING_FILE)).await;
        Ok::<_, String>((meta, wanted_audio))
    };
    let (meta, wanted_audio) = match work.await {
        Ok(m) => m,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir).await;
            return Err(format!("Could not merge: {e}"));
        }
    };
    info!(recording_id = %id, sources = ids.len(), frames = meta.frame_count, audio = meta.audio, "Merged recordings");
    drop(new_busy);
    drop(busy);
    if delete && wanted_audio && !meta.audio {
        warn!(recording_id = %id, "The merge lost the sound of its sources: they are kept");
    } else if delete {
        for id in ids {
            if !delete_recording(config, *id).await {
                warn!(recording_id = %id, "A merged recording could not be deleted (in use?)");
            }
        }
    }
    Ok(meta)
}

#[derive(Serialize, Debug)]
pub struct VacuumStats {
    pub vacuumed_count: usize,
    /// The recordings removed, or on a dry run the ones that would be.
    pub ids: Vec<Uuid>,
    pub freed_bytes: u64,
    pub current_storage_bytes: u64,
    pub max_storage_bytes: u64,
}

/// What the cleanup knows about a recording.
struct Candidate {
    /// Seconds the age is counted from.
    age_ref: u64,
    size_bytes: u64,
    /// Being recorded by this process.
    active: bool,
    /// Marked to be kept.
    keep: bool,
    /// An export, merge or salvage is reading it.
    busy: bool,
}

/// Which recordings (indexes) the cleanup removes: those older than `max_age_secs`,
/// then the oldest until the rest is within `max_storage_bytes` and `max_count`.
/// Kept and active recordings are never removed and do not count against the
/// limits (they would otherwise make every other recording go as soon as they
/// alone exceed a limit); a busy one stays until it is free.
fn select_for_removal(
    items: &[Candidate],
    now: u64,
    max_age_secs: u64,
    max_storage_bytes: u64,
    max_count: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    // The population the limits apply to, oldest first.
    let mut rest: Vec<usize> = Vec::new();
    for (i, c) in items.iter().enumerate() {
        if c.keep || c.active {
            continue;
        }
        if !c.busy && now.saturating_sub(c.age_ref) > max_age_secs {
            out.push(i);
        } else {
            rest.push(i);
        }
    }
    rest.sort_by_key(|&i| items[i].age_ref);
    let mut storage: u64 = rest.iter().map(|&i| items[i].size_bytes).sum();
    let mut count = rest.len();
    for i in rest {
        if storage <= max_storage_bytes && count <= max_count {
            break;
        }
        if items[i].busy {
            continue;
        }
        storage = storage.saturating_sub(items[i].size_bytes);
        count -= 1;
        out.push(i);
    }
    out
}

pub async fn vacuum_recordings(config: &RecorderConfig) -> VacuumStats {
    vacuum_recordings_with(config, false).await
}

/// The cleanup; with `dry_run` it only says what it would remove.
pub async fn vacuum_recordings_with(config: &RecorderConfig, dry_run: bool) -> VacuumStats {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut paths = Vec::new();
    let mut items: Vec<Candidate> = Vec::new();
    let mut current_total_bytes = 0u64;

    for id in recording_ids(config).await {
        let Some(meta) = load_meta(config, id).await else {
            continue;
        };
        let rec_path = rec_dir_of(config, id);
        let size = compute_rec_dir_size(&rec_path).await;
        current_total_bytes += size;
        items.push(Candidate {
            age_ref: meta.age_reference_secs(),
            size_bytes: size,
            active: is_active(id),
            keep: kept(&rec_path).await,
            busy: is_busy(id),
        });
        paths.push(rec_path);
    }

    let to_delete = select_for_removal(
        &items,
        now,
        config.max_age_secs,
        config.max_storage_bytes,
        config.max_count,
    );

    let mut vacuumed_count = 0;
    let mut freed_bytes = 0;
    let mut ids = Vec::new();

    for i in to_delete {
        // Busy (or active) since the listing: the next round.
        let Some(id) = paths[i]
            .file_name()
            .and_then(|n| Uuid::parse_str(&n.to_string_lossy()).ok())
        else {
            continue;
        };
        if is_active(id) || is_busy(id) {
            continue;
        }
        if dry_run || fs::remove_dir_all(&paths[i]).await.is_ok() {
            vacuumed_count += 1;
            freed_bytes += items[i].size_bytes;
            ids.push(id);
        }
    }

    if vacuumed_count > 0 && !dry_run {
        info!(
            vacuumed = vacuumed_count,
            freed_mb = freed_bytes / 1024 / 1024,
            current_mb = (current_total_bytes.saturating_sub(freed_bytes)) / 1024 / 1024,
            "Auto-vacuum completed"
        );
    }

    VacuumStats {
        vacuumed_count,
        ids,
        freed_bytes,
        current_storage_bytes: current_total_bytes.saturating_sub(freed_bytes),
        max_storage_bytes: config.max_storage_bytes,
    }
}

/// The session's own X display, for recording without screenshots: asking
/// Chromium for one makes it repaint the window, which the live view's video
/// (captured from the same display) shows as flicker.
#[derive(Clone, Debug)]
pub struct DisplayGrab {
    /// An ffmpeg with x11grab (the live video's).
    pub ffmpeg: PathBuf,
    pub display: String,
    /// The display's size, to keep the grabbed area on it.
    pub size: (u32, u32),
}

/// One ffmpeg grabbing an area of the display at the recording's frame rate; the
/// newest JPEG is kept for the recorder to pick up.
struct Grabber {
    region: (u32, u32, u32, u32),
    _child: tokio::process::Child,
    latest: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl DisplayGrab {
    /// The page's area on the display: below the toolbars, the page's size.
    fn region(&self, chrome_h: u32, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let (sw, sh) = self.size;
        let y = chrome_h.min(sh.saturating_sub(16));
        let w = w.min(sw) & !1;
        let h = h.min(sh - y) & !1;
        (w >= 16 && h >= 16).then_some((0, y, w, h))
    }

    fn start(&self, region: (u32, u32, u32, u32)) -> Option<Grabber> {
        let (x, y, w, h) = region;
        let fps = (1000 / RECORD_FRAME_INTERVAL.as_millis().max(1))
            .max(1)
            .to_string();
        let mut cmd = tokio::process::Command::new(&self.ffmpeg);
        crate::proc::die_with_parent(&mut cmd);
        let mut child = cmd
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-f", "x11grab", "-draw_mouse", "0", "-framerate", &fps])
            .args(["-video_size", &format!("{w}x{h}")])
            .arg("-i")
            .arg(format!("{}+{x},{y}", self.display))
            .args([
                "-vf",
                &format!(
                    "scale=w={}:h={}:force_original_aspect_ratio=decrease",
                    RECORD_MAX.0 as u32, RECORD_MAX.1 as u32
                ),
            ])
            .args(["-c:v", "mjpeg", "-q:v", "6", "-f", "image2pipe", "-"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| error!(error = %e, "Could not start the recording grabber"))
            .ok()?;
        let mut out = child.stdout.take()?;
        let latest = Arc::new(std::sync::Mutex::new(None));
        let sink = latest.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            while let Ok(n) = out.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                // ffmpeg's MJPEG frames: SOI (FFD8) .. EOI (FFD9); the scan data
                // escapes 0xFF, so EOI only appears at a frame's end.
                while let Some(end) = buf.windows(2).position(|p| p == [0xFF, 0xD9]) {
                    let frame: Vec<u8> = buf.drain(..end + 2).collect();
                    if let Some(start) = frame.windows(2).position(|p| p == [0xFF, 0xD8]) {
                        *sink.lock().unwrap() = Some(frame[start..].to_vec());
                    }
                }
            }
        });
        Some(Grabber {
            region,
            _child: child,
            latest,
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod playback_window_tests {
    use super::*;

    #[tokio::test]
    async fn seeks_before_between_and_after_frames_without_losing_the_preceding_image() {
        let dir = std::env::temp_dir().join(format!("bd-seek-{}", Uuid::new_v4()));
        fs::create_dir(&dir).await.unwrap();
        let path = dir.join("frames.jsonl");
        let mut data = Vec::new();
        for i in 0..4000u64 {
            let frame = RecordingFrame {
                t: 1000 + i * i,
                url: None,
                title: None,
                data: "x".repeat(512),
            };
            data.extend(serde_json::to_vec(&frame).unwrap());
            data.push(b'\n');
        }
        data.extend_from_slice(b"{\"t\":"); // a recording interrupted mid-frame
        fs::write(&path, data).await.unwrap();
        for at in [0, 1000, 5_000_500, u64::MAX] {
            let (frames, eof) = playback_window(&path, at).await.unwrap();
            assert!(!frames.is_empty());
            assert!(frames.len() <= 120);
            if at < 1000 {
                assert_eq!(frames[0].t, 1000);
            } else {
                assert!(frames[0].t <= at);
            }
            if let Some(next) = frames.get(1) {
                assert!(next.t > at);
            }
            if at == u64::MAX {
                assert!(eof);
                assert_eq!(frames.len(), 1);
            }
        }
        fs::remove_dir_all(dir).await.unwrap();
    }
}
