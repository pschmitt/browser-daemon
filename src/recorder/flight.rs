//! Flight recorder: the last seconds of a session's video, kept in memory and
//! saved as an ordinary recording on request.

use super::*;
use std::collections::VecDeque;

/// The window kept when none is asked for.
pub const FLIGHT_DEFAULT_SECS: u64 = 60;
/// The longest window that can be asked for.
pub const FLIGHT_MAX_SECS: u64 = 300;
/// Memory one session's buffer may use, whatever the window.
pub const FLIGHT_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Title prefix of a saved buffer, so that it can be told from a normal recording.
pub const FLIGHT_TITLE: &str = "Flight recorder";

/// Frames of the last `window_ms`, bounded in bytes too; the oldest go first.
pub struct FlightRing {
    window_ms: u64,
    cap_bytes: usize,
    frames: VecDeque<RecordingFrame>,
    bytes: usize,
    /// Everything before this was dropped to stay within the byte cap.
    floor_ms: u64,
}

/// What a save writes: frames with the times counted from its start.
pub struct FlightSnapshot {
    pub frames: Vec<RecordingFrame>,
    pub duration_ms: u64,
}

fn frame_bytes(f: &RecordingFrame) -> usize {
    f.data.len()
        + f.url.as_ref().map_or(0, String::len)
        + f.title.as_ref().map_or(0, String::len)
        + 64
}

impl FlightRing {
    pub fn new(window_secs: u64, cap_bytes: usize) -> Self {
        Self {
            window_ms: window_secs.saturating_mul(1000),
            cap_bytes,
            frames: VecDeque::new(),
            bytes: 0,
            floor_ms: 0,
        }
    }

    pub fn set_window_secs(&mut self, secs: u64) {
        self.window_ms = secs.saturating_mul(1000);
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Where the kept window starts (ms on the recorder's clock).
    fn start(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.window_ms).max(self.floor_ms)
    }

    /// Add a frame stamped `frame.t`; `now_ms` is the clock now.
    pub fn push(&mut self, frame: RecordingFrame, now_ms: u64) {
        self.bytes += frame_bytes(&frame);
        self.frames.push_back(frame);
        self.evict(now_ms);
    }

    /// Drop what fell out of the window or over the cap. A frame at or before the
    /// window's start stays: it is what the screen showed when the window began.
    pub fn evict(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        while self.frames.len() >= 2 && self.frames[1].t <= cutoff {
            self.pop_front();
        }
        while self.bytes > self.cap_bytes && self.frames.len() > 1 {
            self.pop_front();
            if let Some(first) = self.frames.front() {
                self.floor_ms = self.floor_ms.max(first.t);
            }
        }
    }

    fn pop_front(&mut self) {
        if let Some(f) = self.frames.pop_front() {
            self.bytes = self.bytes.saturating_sub(frame_bytes(&f));
        }
    }

    /// Milliseconds of video held.
    pub fn buffered_ms(&self, now_ms: u64) -> u64 {
        if self.frames.is_empty() {
            0
        } else {
            now_ms.saturating_sub(self.start(now_ms))
        }
    }

    /// The window as a recording's frames; None while there is none.
    pub fn snapshot(&mut self, now_ms: u64) -> Option<FlightSnapshot> {
        self.evict(now_ms);
        if self.frames.is_empty() {
            return None;
        }
        let start = self.start(now_ms);
        let frames: Vec<RecordingFrame> = self
            .frames
            .iter()
            .map(|f| RecordingFrame {
                t: f.t.saturating_sub(start),
                ..f.clone()
            })
            .collect();
        let last = frames.last().map_or(0, |f| f.t);
        Some(FlightSnapshot {
            duration_ms: now_ms.saturating_sub(start).max(last),
            frames,
        })
    }
}

struct Flight {
    handle: SessionRecorderHandle,
    ring: Arc<Mutex<FlightRing>>,
    seconds: u64,
    profile: Option<String>,
    node: String,
    last_saved: Option<Uuid>,
}

impl Flight {
    fn running(&self) -> bool {
        !self.handle.stop_flag.load(Ordering::SeqCst)
    }
}

static FLIGHTS: LazyLock<Mutex<HashMap<Uuid, Flight>>> = LazyLock::new(Mutex::default);

fn flights() -> std::sync::MutexGuard<'static, HashMap<Uuid, Flight>> {
    FLIGHTS.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_ring(ring: &Mutex<FlightRing>) -> std::sync::MutexGuard<'_, FlightRing> {
    ring.lock().unwrap_or_else(|e| e.into_inner())
}

/// The state the HTTP API and the CLI report.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FlightStatus {
    pub active: bool,
    /// The window kept (0 while off).
    pub seconds: u64,
    pub buffered_secs: f64,
    /// The recording the last save made.
    pub recording_id: Option<Uuid>,
}

/// What a session's summary says about it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FlightSummary {
    pub active: bool,
    pub seconds: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FlightError {
    NotActive,
    Empty,
    Io(String),
}

fn status_of(f: Option<&Flight>) -> FlightStatus {
    match f.filter(|f| f.running()) {
        Some(f) => {
            let now = duration_ms(f.handle.clock.elapsed());
            FlightStatus {
                active: true,
                seconds: f.seconds,
                buffered_secs: lock_ring(&f.ring).buffered_ms(now) as f64 / 1000.0,
                recording_id: f.last_saved,
            }
        }
        None => FlightStatus {
            active: false,
            seconds: 0,
            buffered_secs: 0.0,
            recording_id: None,
        },
    }
}

pub fn flight_status(session_id: Uuid) -> FlightStatus {
    let mut all = flights();
    if all.get(&session_id).is_some_and(|f| !f.running()) {
        all.remove(&session_id);
    }
    status_of(all.get(&session_id))
}

pub fn flight_summary(session_id: Uuid) -> Option<FlightSummary> {
    let s = flight_status(session_id);
    s.active.then_some(FlightSummary {
        active: true,
        seconds: s.seconds,
    })
}

/// Start buffering. The handle comes back only when a recorder was started now (the
/// caller feeds it BiDi screenshots for Firefox); on a running one only the window
/// changes.
pub fn flight_start(
    session_id: Uuid,
    port: Option<u16>,
    profile: Option<String>,
    node_name: String,
    config: RecorderConfig,
    grab: Option<DisplayGrab>,
    seconds: u64,
) -> (FlightStatus, Option<SessionRecorderHandle>) {
    let mut all = flights();
    if let Some(f) = all.get_mut(&session_id).filter(|f| f.running()) {
        f.seconds = seconds;
        lock_ring(&f.ring).set_window_secs(seconds);
        return (status_of(Some(f)), None);
    }
    let ring = Arc::new(Mutex::new(FlightRing::new(seconds, FLIGHT_MAX_BYTES)));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(RecordingClock::new());
    let (tx, frame_rx) = tokio::sync::mpsc::channel::<StampedFrame>(FRAME_QUEUE);
    let handle = SessionRecorderHandle {
        recording_id: Uuid::new_v4(),
        stop_flag: stop_flag.clone(),
        clock: clock.clone(),
        frame_tx: FrameSender {
            tx,
            clock: clock.clone(),
            last: Arc::default(),
        },
        sound: None,
    };
    let task_ring = ring.clone();
    let task_profile = profile.clone();
    let task_node = node_name.clone();
    let task_id = handle.recording_id;
    tokio::spawn(async move {
        record_session_loop(
            task_id,
            session_id,
            port,
            task_profile,
            task_node,
            config,
            stop_flag.clone(),
            clock,
            frame_rx,
            grab,
            None,
            None,
            Some(task_ring),
        )
        .await;
        // Ended on its own (the session went away): the entry is stale.
        stop_flag.store(true, Ordering::SeqCst);
    });
    let flight = Flight {
        handle: handle.clone(),
        ring,
        seconds,
        profile,
        node: node_name,
        last_saved: None,
    };
    let status = status_of(Some(&flight));
    all.insert(session_id, flight);
    (status, Some(handle))
}

/// Stop buffering and free the buffer; false when it was off.
pub fn flight_stop(session_id: Uuid) -> bool {
    match flights().remove(&session_id) {
        Some(f) => {
            let was_running = f.running();
            f.handle.stop();
            was_running
        }
        None => false,
    }
}

/// The recording's metadata for a snapshot written at `now_secs`.
fn flight_meta(
    id: Uuid,
    session_id: Uuid,
    profile: Option<String>,
    node: String,
    snap: &FlightSnapshot,
    size_bytes: u64,
    now_secs: u64,
) -> RecordingMeta {
    let title = snap.frames.iter().rev().find_map(|f| f.title.clone());
    RecordingMeta {
        id,
        session_id,
        profile,
        node,
        started_at_secs: now_secs.saturating_sub(snap.duration_ms / 1000),
        ended_at_secs: Some(now_secs),
        duration_seconds: snap.duration_ms / 1000,
        duration_ms: snap.duration_ms,
        frame_count: snap.frames.len() as u64,
        size_bytes,
        initial_url: snap.frames.iter().find_map(|f| f.url.clone()),
        final_url: snap.frames.iter().rev().find_map(|f| f.url.clone()),
        title: Some(match title {
            Some(t) => format!("{FLIGHT_TITLE}: {t}"),
            None => FLIGHT_TITLE.to_string(),
        }),
        favicon_url: None,
        favicon_type: None,
        keep: false,
        status: "completed".to_string(),
        audio: false,
        merged_from: Vec::new(),
        merged_at_secs: None,
    }
}

/// Write the snapshot as a recording; its id.
pub(super) async fn write_flight_recording(
    config: &RecorderConfig,
    session_id: Uuid,
    profile: Option<String>,
    node: String,
    snap: &FlightSnapshot,
) -> std::io::Result<Uuid> {
    let id = Uuid::new_v4();
    let dir = rec_dir_of(config, id);
    make_rec_dir(config, &dir).await?;
    let mut lines = Vec::new();
    for f in &snap.frames {
        serde_json::to_writer(&mut lines, f)?;
        lines.push(b'\n');
    }
    let mut file = crate::persist::private_append(&dir.join("frames.jsonl")).await?;
    file.write_all(&lines).await?;
    file.flush().await?;
    drop(file);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let meta = flight_meta(
        id,
        session_id,
        profile,
        node,
        snap,
        lines.len() as u64,
        now_secs,
    );
    write_meta(&dir.join("meta.json"), &meta).await?;
    Ok(id)
}

/// Save the buffered frames as a recording; buffering goes on.
pub async fn flight_save(config: &RecorderConfig, session_id: Uuid) -> Result<Uuid, FlightError> {
    let (snap, profile, node) = {
        let all = flights();
        let f = all
            .get(&session_id)
            .filter(|f| f.running())
            .ok_or(FlightError::NotActive)?;
        let now = duration_ms(f.handle.clock.elapsed());
        let snap = lock_ring(&f.ring).snapshot(now).ok_or(FlightError::Empty)?;
        (snap, f.profile.clone(), f.node.clone())
    };
    let id = write_flight_recording(config, session_id, profile, node, &snap)
        .await
        .map_err(|e| FlightError::Io(e.to_string()))?;
    if let Some(f) = flights().get_mut(&session_id) {
        f.last_saved = Some(id);
    }
    info!(
        recording_id = %id,
        session_id = %session_id,
        frames = snap.frames.len(),
        "Saved the flight recorder"
    );
    vacuum_recordings(config).await;
    Ok(id)
}
