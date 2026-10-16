//! Live view WebSockets: video, audio and the viewer list.

use super::*;

#[derive(Deserialize)]
pub(crate) struct VideoParams {
    pub(crate) session_id: Uuid,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// Stream a region of a session's own display as H.264, one access unit per
/// binary message: a flag byte (1 = keyframe) followed by the Annex B data. One
/// encoder per viewer; reconnect to change the region. Access control as for the
/// page WebSocket (unguessable session id, mesh or Authelia in front).
pub(crate) async fn handle_video_ws(
    ws: WebSocketUpgrade,
    Query(p): Query<VideoParams>,
    State(state): State<AppState>,
) -> Response {
    let Some(cfg) = state.config.video.clone() else {
        return (StatusCode::NOT_FOUND, "This node does not stream video").into_response();
    };
    let found = state
        .sessions
        .read()
        .await
        .get(&p.session_id)
        .and_then(|s| Some((s.display.clone()?, s.display_size?)));
    let Some((xdisplay, (dw, dh))) = found else {
        return (
            StatusCode::NOT_FOUND,
            "No display of its own for this session",
        )
            .into_response();
    };
    let region = video::Region {
        x: p.x,
        y: p.y,
        w: p.w,
        h: p.h,
    };
    let Some(region) = region.clamp(dw, dh) else {
        return (StatusCode::BAD_REQUEST, "Region outside the display").into_response();
    };
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let mut child = match video::encoder(&cfg, &xdisplay, region) {
            Ok(c) => c,
            Err(e) => {
                warn!(xdisplay = %xdisplay, error = %e, "Could not start the video encoder");
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        if let Some(err) = child.stderr.take() {
            let xdisplay = xdisplay.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(xdisplay = %xdisplay, "ffmpeg: {line}");
                }
            });
        }
        info!(session_id = %p.session_id, region = ?region, "Video viewer attached");
        let mut units = video::AccessUnits::default();
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending = false;
        'stream: loop {
            tokio::select! {
                // Encoder quiet for a moment: the buffered frame is complete. ffmpeg
                // writes a frame in back-to-back chunks, so a few ms is plenty.
                _ = tokio::time::sleep(std::time::Duration::from_millis(4)), if pending => {
                    pending = false;
                    if let Some((key, au)) = units.flush() {
                        let mut msg = Vec::with_capacity(au.len() + 1);
                        msg.push(u8::from(key));
                        msg.extend_from_slice(&au);
                        if socket.send(Message::Binary(msg.into())).await.is_err() {
                            break 'stream;
                        }
                    }
                }
                n = out.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending = true;
                        for (key, au) in units.push(&buf[..n]) {
                            let mut msg = Vec::with_capacity(au.len() + 1);
                            msg.push(u8::from(key));
                            msg.extend_from_slice(&au);
                            if socket.send(Message::Binary(msg.into())).await.is_err() {
                                break 'stream;
                            }
                        }
                    }
                },
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        let _ = child.kill().await;
        info!(session_id = %p.session_id, "Video viewer left");
    })
}

#[derive(Deserialize)]
pub(crate) struct AudioParams {
    pub(crate) session_id: Uuid,
}

/// Stream a session's audio as Opus in WebM, one encoder per listener. Like the
/// page WebSocket, the unguessable session id plus the mesh or Authelia in
/// front are the access control.
pub(crate) async fn handle_audio_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<AudioParams>,
    State(state): State<AppState>,
) -> Response {
    let Some(cfg) = state.config.audio.clone() else {
        return (StatusCode::NOT_FOUND, "This node has no audio").into_response();
    };
    let sink = state
        .sessions
        .read()
        .await
        .get(&params.session_id)
        .and_then(|s| s.audio_sink.clone());
    let Some(sink) = sink else {
        return (StatusCode::NOT_FOUND, "No audio for this session").into_response();
    };
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::io::AsyncReadExt;
        let mut child = match audio::encoder(&cfg, &sink) {
            Ok(c) => c,
            Err(e) => {
                warn!(sink = %sink, error = %e, "Could not start the audio encoder");
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        info!(session_id = %params.session_id, "Audio listener attached");
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            tokio::select! {
                n = out.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if socket.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                },
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        let _ = child.kill().await;
        info!(session_id = %params.session_id, "Audio listener left");
    })
}

#[derive(Deserialize)]
pub(crate) struct ViewerReport {
    /// [width, height] while in Native/Phone layout, null otherwise.
    pub(crate) native: Option<(u32, u32)>,
    /// This viewer's size wins over the others' (and over automation clients).
    #[serde(default)]
    pub(crate) force: bool,
}

/// One live view of a session: it reports its size in Native layout and gets
/// the size all Native viewers of the session share.
pub(crate) async fn handle_viewers_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<AudioParams>,
    State(state): State<AppState>,
) -> Response {
    if !state.sessions.read().await.contains_key(&params.session_id) {
        return (StatusCode::NOT_FOUND, "No such session").into_response();
    }
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let session = params.session_id;
        let (id, mut rx) = state.viewers.join(session);
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(r) = serde_json::from_str::<ViewerReport>(&t) {
                            state.viewers.set(session, id, r.native, r.force);
                        }
                    }
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
        state.viewers.leave(session, id);
    })
}
