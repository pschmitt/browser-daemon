use super::*;
use serde_json::json;

#[tokio::test]
async fn the_cleanup_never_removes_a_kept_recording() {
    let dir = std::env::temp_dir().join(format!("bd-keep-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = RecorderConfig {
        enabled: true,
        recordings_dir: dir.clone(),
        max_storage_bytes: 0,
        max_age_secs: 60,
        max_count: 1000,
        audio: false,
        ffmpeg: None,
    };
    // Two old recordings, one of them kept; both are over the age and storage limits.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let id = Uuid::new_v4();
        let rec = dir.join(id.to_string());
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(rec.join("frames.jsonl"), b"x".repeat(100)).unwrap();
        let meta = serde_json::json!({
            "id": id, "session_id": Uuid::new_v4(), "profile": null, "node": "n",
            "started_at_secs": 1000, "ended_at_secs": 1010, "duration_seconds": 10,
            "frame_count": 1, "size_bytes": 100, "initial_url": null, "final_url": null,
            "title": null, "status": "completed"
        });
        std::fs::write(rec.join("meta.json"), meta.to_string()).unwrap();
        ids.push(id);
    }
    assert!(
        !list_recordings(&config).await.iter().any(|m| m.keep),
        "nothing is kept by default"
    );
    assert!(set_keep(&config, ids[0], true).await);
    assert!(!set_keep(&config, Uuid::new_v4(), true).await);
    assert_eq!(
        list_recordings(&config)
            .await
            .iter()
            .filter(|m| m.keep)
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![ids[0]]
    );
    let plan = vacuum_recordings_with(&config, true).await;
    assert_eq!(plan.ids, vec![ids[1]], "a dry run names what would go");
    assert!(dir.join(ids[1].to_string()).exists(), "and removes nothing");
    let stats = vacuum_recordings(&config).await;
    assert_eq!(stats.vacuumed_count, 1);
    assert_eq!(stats.ids, vec![ids[1]]);
    assert!(dir.join(ids[0].to_string()).exists(), "the kept one stays");
    assert!(!dir.join(ids[1].to_string()).exists(), "the other one goes");
    // Not kept any more: the next cleanup takes it.
    assert!(set_keep(&config, ids[0], false).await);
    vacuum_recordings(&config).await;
    assert!(!dir.join(ids[0].to_string()).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_count_limit_keeps_the_newest_and_the_kept_ones() {
    let dir = std::env::temp_dir().join(format!("bd-count-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let config = RecorderConfig {
        enabled: true,
        recordings_dir: dir.clone(),
        max_storage_bytes: u64::MAX,
        max_age_secs: 86400 * 365,
        max_count: 3,
        audio: false,
        ffmpeg: None,
    };
    let mut ids = Vec::new();
    for i in 0..6u64 {
        let id = Uuid::new_v4();
        let rec = dir.join(id.to_string());
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(rec.join("frames.jsonl"), b"x").unwrap();
        let meta = serde_json::json!({
            "id": id, "session_id": Uuid::new_v4(), "profile": null, "node": "n",
            "started_at_secs": now - 1000 + i, "ended_at_secs": now - 900 + i, "duration_seconds": 10,
            "frame_count": 1, "size_bytes": 1, "initial_url": null, "final_url": null,
            "title": null, "status": "completed"
        });
        std::fs::write(rec.join("meta.json"), meta.to_string()).unwrap();
        ids.push(id);
    }
    assert!(set_keep(&config, ids[0], true).await);
    vacuum_recordings(&config).await;
    let left: Vec<bool> = ids
        .iter()
        .map(|i| dir.join(i.to_string()).exists())
        .collect();
    // Limit 3: the kept one does not count, so the oldest two of the other five go.
    assert_eq!(left, [true, false, false, true, true, true]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_pages_come_first_and_extension_pages_are_not_recorded_first() {
    let pages = page_candidates(vec![
        json!({"type": "page", "url": "about:blank", "webSocketDebuggerUrl": "ws://x/1"}),
        json!({"type": "service_worker", "url": "https://a.test/sw.js", "webSocketDebuggerUrl": "ws://x/2"}),
        json!({"type": "page", "url": "https://a.test/", "webSocketDebuggerUrl": "ws://x/3"}),
    ]);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0]["url"], "https://a.test/");
}

#[test]
fn data_urls_become_icon_bytes() {
    let (mime, bytes) = parse_data_url("data:image/png;base64,AAEC").unwrap();
    assert_eq!((mime.as_str(), bytes), ("image/png", vec![0, 1, 2]));
    assert_eq!(
        parse_data_url("data:;base64,AAEC").unwrap().0,
        "image/x-icon"
    );
    assert!(parse_data_url("data:image/png;base64,").is_none());
    assert!(parse_data_url("https://a.test/i.png").is_none());
}

#[test]
fn only_plain_icons_are_stored() {
    assert_eq!(
        real_favicon(&json!({"faviconUrl": "https://a.test/i.png"})).as_deref(),
        Some("https://a.test/i.png")
    );
    assert!(real_favicon(&json!({"faviconUrl": ""})).is_none());
    assert!(real_favicon(&json!({"faviconUrl": "data:image/png;base64,AAAA"})).is_none());
    assert!(real_favicon(&json!({})).is_none());
}

#[tokio::test]
async fn merging_puts_recordings_one_after_the_other() {
    let dir = std::env::temp_dir().join(format!("bd-merge-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = RecorderConfig {
        enabled: true,
        recordings_dir: dir.clone(),
        max_storage_bytes: u64::MAX,
        max_age_secs: 86400,
        max_count: 100,
        audio: false,
        ffmpeg: None,
    };
    let mut ids = Vec::new();
    for (i, ts) in [[0u64, 1000], [0, 3000]].iter().enumerate() {
        let id = Uuid::new_v4();
        let rec = dir.join(id.to_string());
        std::fs::create_dir_all(&rec).unwrap();
        let frames: String = ts
            .iter()
            .map(|t| format!("{{\"t\":{t},\"data\":\"x{i}\"}}\n"))
            .collect();
        std::fs::write(rec.join("frames.jsonl"), frames).unwrap();
        let meta = json!({ "id": id, "session_id": id, "profile": null, "node": "n",
                "started_at_secs": 100 + i as u64, "ended_at_secs": 200, "duration_seconds": 2,
                "frame_count": 2, "size_bytes": 1, "initial_url": format!("https://{i}.test"),
                "final_url": null, "title": format!("t{i}"), "status": "completed" });
        std::fs::write(rec.join("meta.json"), meta.to_string()).unwrap();
        ids.push(id);
    }
    // Second one first; one of them is kept.
    std::fs::write(dir.join(ids[0].to_string()).join(KEEP_FILE), b"").unwrap();
    let order = [ids[1], ids[0]];
    let m = merge_recordings(&config, &order, true).await.unwrap();
    assert_eq!((m.duration_ms, m.duration_seconds), (5500, 5));
    assert!(m.keep && dir.join(m.id.to_string()).join(KEEP_FILE).exists());
    assert!(m.merged_at_secs.is_some());
    assert!(!dir.join(m.id.to_string()).join(MERGING_FILE).exists());
    assert_eq!(m.merged_from, order);
    assert_eq!((m.frame_count, m.title.as_deref()), (4, Some("t1")));
    assert_eq!(m.initial_url.as_deref(), Some("https://1.test"));
    let frames = std::fs::read_to_string(dir.join(m.id.to_string()).join("frames.jsonl")).unwrap();
    let ts: Vec<u64> = frames
        .lines()
        .map(|l| serde_json::from_str::<RecordingFrame>(l).unwrap().t)
        .collect();
    // 3 s of the first, then a gap of MERGE_GAP_MS.
    assert_eq!(ts, [0, 3000, 3500, 4500]);
    assert!(
        ids.iter().all(|id| !dir.join(id.to_string()).exists()),
        "originals deleted"
    );
    assert!(merge_recordings(&config, &[m.id], false).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

fn cand(age: u64, size: u64) -> Candidate {
    Candidate {
        age_ref: age,
        size_bytes: size,
        active: false,
        keep: false,
        busy: false,
    }
}

#[test]
fn kept_recordings_do_not_eat_the_quota() {
    // 5 GB kept, quota 2 GB: the new recording is well within it.
    let gb = 1u64 << 30;
    let mut items = vec![cand(100, 5 * gb), cand(200, gb / 2), cand(300, gb / 2)];
    items[0].keep = true;
    assert!(select_for_removal(&items, 400, 10_000, 2 * gb, 100).is_empty());
    // An active recording is out of it too.
    items[2].active = true;
    items[2].size_bytes = 9 * gb;
    assert!(select_for_removal(&items, 400, 10_000, 2 * gb, 100).is_empty());
    // The others still go, oldest first, when they alone are over.
    items[1].size_bytes = 3 * gb;
    items.push(cand(350, gb));
    assert_eq!(select_for_removal(&items, 400, 10_000, 2 * gb, 100), [1]);
}

#[test]
fn age_and_count_limits_skip_kept_active_and_busy_ones() {
    let mut items = vec![cand(10, 1), cand(20, 1), cand(30, 1), cand(1000, 1)];
    items[0].keep = true;
    items[1].busy = true;
    // Older than 500 s at now=1100: 0 (kept) and 1 (busy) stay, 2 goes.
    assert_eq!(select_for_removal(&items, 1100, 500, u64::MAX, 100), [2]);
    // Count limit 2 with a busy oldest: the busy one is skipped, the next goes.
    let mut items = vec![cand(10, 1), cand(20, 1), cand(30, 1)];
    items[0].busy = true;
    assert_eq!(select_for_removal(&items, 40, 1000, u64::MAX, 2), [1]);
    // Limit 1: the busy one stays whatever, the others make room.
    assert_eq!(select_for_removal(&items, 40, 1000, u64::MAX, 1), [1, 2]);
}

#[test]
fn merged_recordings_start_after_each_other_with_a_gap() {
    assert_eq!(merge_plan(&[3000, 2500]), (vec![0, 3500], 6000));
    assert_eq!(merge_plan(&[1234]), (vec![0], 1234));
    assert_eq!(merge_plan(&[1500, 0, 700]), (vec![0, 2000, 2500], 3200));
}

#[test]
fn a_recordings_length_is_the_longer_of_its_clock_and_its_last_frame() {
    let mut m = serde_json::from_value::<RecordingMeta>(json!({
        "id": Uuid::nil(), "session_id": Uuid::nil(), "node": "n", "started_at_secs": 1,
        "duration_seconds": 2, "frame_count": 0, "size_bytes": 0, "status": "completed"
    }))
    .unwrap();
    // Recordings of older versions have whole seconds only.
    assert_eq!(m.length_ms(), 2000);
    m.duration_ms = 2750;
    assert_eq!(m.length_ms(), 2750);
    assert_eq!(merge_length(3100, &m), 3100);
    assert_eq!(merge_length(100, &m), 2750);
}

#[test]
fn frame_times_never_go_back() {
    assert_eq!(frame_time(900, 1000), 1000);
    assert_eq!(frame_time(1200, 1000), 1200);
}

#[tokio::test]
async fn a_torn_meta_is_rebuilt_from_the_frames() {
    let dir = std::env::temp_dir().join(format!("bd-meta-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = RecorderConfig {
        enabled: true,
        recordings_dir: dir.clone(),
        max_storage_bytes: u64::MAX,
        max_age_secs: u64::MAX,
        max_count: 100,
        audio: false,
        ffmpeg: None,
    };
    let id = Uuid::new_v4();
    let rec = dir.join(id.to_string());
    std::fs::create_dir_all(&rec).unwrap();
    // The last frame line is cut off by the crash.
    let frames = "{\"t\":0,\"url\":\"about:blank\",\"data\":\"a\"}\n\
            {\"t\":800,\"url\":\"https://a.test/\",\"title\":\"A\",\"data\":\"b\"}\n\
            {\"t\":2600,\"url\":\"https://a.test/x\",\"title\":\"B\",\"data\":\"c\"}\n\
            {\"t\":3000,\"da";
    std::fs::write(rec.join("frames.jsonl"), frames).unwrap();
    std::fs::write(rec.join("meta.json"), "{\"id\": \"torn").unwrap();
    let m = get_recording(&config, id).await.expect("rebuilt");
    assert_eq!((m.id, m.frame_count, m.duration_ms), (id, 3, 2600));
    assert_eq!(m.duration_seconds, 2);
    assert_eq!(m.status, "interrupted");
    assert_eq!(m.initial_url.as_deref(), Some("https://a.test/"));
    assert_eq!(m.final_url.as_deref(), Some("https://a.test/x"));
    assert_eq!(m.title.as_deref(), Some("B"));
    // Written back, and listed.
    assert_eq!(list_recordings(&config).await.len(), 1);
    let again: RecordingMeta =
        serde_json::from_slice(&std::fs::read(rec.join("meta.json")).unwrap()).unwrap();
    assert_eq!(again.frame_count, 3);
    // No frames, no recording.
    let empty = dir.join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(list_recordings(&config).await.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn metadata_is_written_all_at_once() {
    let dir = std::env::temp_dir().join(format!("bd-atomic-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::from_value::<RecordingMeta>(json!({
        "id": Uuid::nil(), "session_id": Uuid::nil(), "node": "n", "started_at_secs": 1,
        "duration_seconds": 2, "frame_count": 0, "size_bytes": 0, "status": "completed"
    }))
    .unwrap();
    let path = dir.join("meta.json");
    write_meta(&path, &meta).await.unwrap();
    write_meta(&path, &meta).await.unwrap();
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, ["meta.json"]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn busy_recordings_are_not_deleted_and_running_ones_not_merged() {
    let dir = std::env::temp_dir().join(format!("bd-busy-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = RecorderConfig {
        enabled: true,
        recordings_dir: dir.clone(),
        max_storage_bytes: u64::MAX,
        max_age_secs: u64::MAX,
        max_count: 100,
        audio: false,
        ffmpeg: None,
    };
    let id = Uuid::new_v4();
    let rec = dir.join(id.to_string());
    std::fs::create_dir_all(&rec).unwrap();
    std::fs::write(rec.join("frames.jsonl"), "{\"t\":0,\"data\":\"a\"}\n").unwrap();
    let busy = mark_busy(&[id]).unwrap();
    assert!(is_busy(id));
    assert!(
        !delete_recording(&config, id).await,
        "an export is reading it"
    );
    drop(busy);
    assert!(!is_busy(id));
    assert!(delete_recording(&config, id).await);

    // A running recorder: not busy-able, not mergeable.
    let session = Uuid::new_v4();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let first = ActiveGuard::new(a, session, Arc::new(AtomicBool::new(false)));
    assert!(is_active(a) && mark_busy(&[a]).is_none());
    assert!(merge_recordings(&config, &[a, b], false).await.is_err());
    // A second recorder for the session stops the first.
    let second = ActiveGuard::new(b, session, Arc::new(AtomicBool::new(false)));
    assert!(first.stop.load(Ordering::SeqCst));
    assert!(!second.stop.load(Ordering::SeqCst));
    drop(first);
    assert!(
        is_active(b),
        "the first one going does not unregister the second"
    );
    drop(second);
    assert!(!is_active(a) && !is_active(b));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn proxied_frames_are_stamped_thinned_and_bounded() {
    let clock = Arc::new(RecordingClock::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(FRAME_QUEUE);
    let sender = FrameSender {
        tx,
        clock: clock.clone(),
        last: Arc::default(),
    };
    let frame = |d: &str| ProxyRecordingFrame {
        url: None,
        title: None,
        data: d.to_string(),
    };
    assert!(sender.send(frame("a")).is_ok());
    // Too soon after the last one, and unchanged later: dropped.
    assert!(sender.send(frame("b")).is_ok());
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err());
    // While paused nothing is queued.
    clock.set_paused(true);
    assert!(sender.send(frame("c")).is_ok());
    assert!(rx.try_recv().is_err());
    clock.set_paused(false);
    // A full queue drops, it does not block or grow.
    for i in 0..(FRAME_QUEUE * 2) {
        *sender.last.lock().unwrap() = None;
        assert!(sender.send(frame(&format!("f{i}"))).is_ok());
    }
    let mut queued = 0;
    while rx.try_recv().is_ok() {
        queued += 1;
    }
    assert_eq!(queued, FRAME_QUEUE);
    // Once the recorder is gone the sender says so.
    drop(rx);
    *sender.last.lock().unwrap() = None;
    assert!(sender.send(frame("z")).is_err());
}
