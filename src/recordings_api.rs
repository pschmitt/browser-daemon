//! Recording handlers: listing, playback data, export and the per-session controls.

use super::*;

pub(crate) async fn api_list_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let list = recorder::list_recordings(&state.recorder_config).await;
    Ok(Json(list))
}

pub(crate) async fn api_get_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let rec = recorder::get_recording(&state.recorder_config, id)
        .await
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))?;
    Ok(Json(rec))
}

/// The site's own icon, as grabbed during the recording.
pub(crate) async fn api_get_recording_favicon(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let dir = state.recorder_config.recordings_dir.join(id.to_string());
    let meta = tokio::fs::read(dir.join("meta.json"))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<recorder::RecordingMeta>(&b).ok());
    let (Some(mime), Ok(bytes)) = (
        meta.and_then(|m| m.favicon_type),
        tokio::fs::read(dir.join("favicon.bin")).await,
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct SiteFaviconParams {
    pub(crate) url: String,
}

/// The icon of the site a page belongs to, fetched by this node (see `favicon.rs`).
pub(crate) async fn api_site_favicon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SiteFaviconParams>,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    match favicon::lookup(&params.url).await {
        Some(icon) => (
            [
                (header::CONTENT_TYPE, icon.mime.to_string()),
                (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; sandbox".to_string(),
                ),
            ],
            icon.bytes,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "private, max-age=600")],
        )
            .into_response(),
    }
}

pub(crate) async fn api_get_recording_frames(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    if let Err(e) = state.check_auth(req.headers()) {
        return e.into_response();
    }
    let frames_path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join("frames.jsonl");
    if !fs::try_exists(&frames_path).await.unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Frames not found for recording" })),
        )
            .into_response();
    }
    // Streamed from disk with Range support: recordings reach hundreds of MB,
    // and clients resume interrupted downloads with `Range: bytes=N-`.
    match tower_http::services::ServeFile::new(&frames_path)
        .try_call(req)
        .await
    {
        Ok(res) => {
            let mut res = res.map(axum::body::Body::new);
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            res
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to read frames: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct KeepRequest {
    pub(crate) keep: bool,
}

/// Mark a recording to be kept (`{"keep": true}`), or not: the cleanup never removes
/// a kept one. Nothing is kept unless asked.
pub(crate) async fn api_set_recording_keep(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<KeepRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if recorder::set_keep(&state.recorder_config, id, req.keep).await {
        Ok(Json(serde_json::json!({ "id": id, "keep": req.keep })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))
    }
}

pub(crate) async fn api_delete_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if recorder::is_active(id) || recorder::is_busy(id) {
        return Err((
            StatusCode::CONFLICT,
            Json(
                serde_json::json!({ "error": "The recording is in use (being recorded, exported or merged)" }),
            ),
        ));
    }
    if recorder::delete_recording(&state.recorder_config, id).await {
        Ok(Json(serde_json::json!({ "status": "deleted", "id": id })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Recording not found" })),
        ))
    }
}

/// A recording's sound (`audio.webm`, Opus), with Range support for seeking.
pub(crate) async fn api_get_recording_audio(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    if let Err(e) = state.check_auth(req.headers()) {
        return e.into_response();
    }
    let path = state
        .recorder_config
        .recordings_dir
        .join(id.to_string())
        .join(rec_audio::FILE);
    if !fs::try_exists(&path).await.unwrap_or(false) {
        return api_error(StatusCode::NOT_FOUND, "This recording has no sound").into_response();
    }
    match tower_http::services::ServeFile::new(&path)
        .try_call(req)
        .await
    {
        Ok(res) => {
            let mut res = res.map(axum::body::Body::new);
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/webm"));
            res
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The recording as a WebM video with its sound, encoded on request (see
/// rec_export.rs); this takes a while for a long one.
pub(crate) async fn api_export_recording(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let (file, size, name) = match rec_export::export_webm(&state.recorder_config, id).await {
        Ok(x) => x,
        Err(e) => return api_error(StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    };
    let chunks = futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "video/webm")
        .header(header::CONTENT_LENGTH, size)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .body(axum::body::Body::from_stream(chunks))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Deserialize, Serialize)]
pub(crate) struct MergeRequest {
    /// The recordings to merge, in order.
    pub(crate) ids: Vec<Uuid>,
    /// Delete them once the merge is written.
    #[serde(default)]
    pub(crate) delete: bool,
}

/// Merges recordings of this node into a new one (see recorder::merge_recordings).
pub(crate) async fn api_merge_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MergeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let meta = recorder::merge_recordings(&state.recorder_config, &req.ids, req.delete)
        .await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(meta))
}

pub(crate) async fn api_vacuum_recordings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let stats = recorder::vacuum_recordings(&state.recorder_config).await;
    Ok(Json(stats))
}

#[derive(Deserialize)]
pub(crate) struct RecordParams {
    /// Take the session's sound too (default: the node's --record-audio).
    #[serde(default)]
    pub(crate) audio: Option<bool>,
}

pub(crate) async fn api_session_start_record(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<RecordParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let session = {
        let guard = state.sessions.read().await;
        guard.get(&id).cloned().ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ))?
    };

    let Some((rec_id, started)) = state.start_recording(&session, params.audio).await else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        ));
    };
    Ok(Json(if started {
        serde_json::json!({
            "status": "recording_started",
            "session_id": id,
            "recording_id": rec_id
        })
    } else {
        serde_json::json!({ "status": "already_recording", "session_id": id })
    }))
}

pub(crate) async fn api_session_stop_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    if let Some(handle) = state.active_recordings.write().await.remove(&id) {
        handle.stop();
        Ok(Json(serde_json::json!({
            "status": "recording_stopped",
            "session_id": id,
            "recording_id": handle.recording_id
        })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session is not actively recording" })),
        ))
    }
}

pub(crate) async fn api_session_pause_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    set_recording_paused(&state, &headers, id, true).await
}

pub(crate) async fn api_session_resume_record(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    set_recording_paused(&state, &headers, id, false).await
}

pub(crate) async fn set_recording_paused(
    state: &AppState,
    headers: &HeaderMap,
    id: Uuid,
    paused: bool,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(headers)?;
    let recs = state.active_recordings.read().await;
    let handle = recs.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Session is not actively recording" })),
    ))?;
    handle.clock.set_paused(paused);
    Ok(Json(serde_json::json!({
        "status": if paused { "recording_paused" } else { "recording_resumed" },
        "session_id": id,
        "recording_id": handle.recording_id
    })))
}
