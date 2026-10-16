//! Per-session actions: screenshots, history, naming, input, proxy and uploads.

use super::*;

/// A small JPEG of the tab the session is showing (the dashboard's cards).
pub(crate) async fn api_session_screenshot(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(session) = state.sessions.read().await.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let jpeg = match state.thumbnails.fresh(id) {
        Some(jpeg) => Some(jpeg),
        None => {
            let shot = match session.bidi {
                Some(ref hub) => match hub.active_context().await {
                    Some(ctx) => hub.screenshot_jpeg(&ctx).await.ok().and_then(|b64| {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.decode(b64).ok()
                    }),
                    None => None,
                },
                None => thumbnail::chromium(&state.http_client, session.port).await,
            };
            if let Some(ref jpeg) = shot {
                state.thumbnails.store(id, jpeg.clone());
            }
            shot.or_else(|| state.thumbnails.stale(id))
        }
    };
    match jpeg {
        Some(jpeg) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            jpeg,
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Whose history a session's pages belong to: its profile's, else its own.
pub(crate) fn history_key(profile_id: Option<Uuid>, session_id: Uuid) -> String {
    profile_id.map_or_else(
        || format!("{}{session_id}", history::SESSION_PREFIX),
        |p| p.to_string(),
    )
}

#[derive(Deserialize)]
pub(crate) struct HistoryParams {
    pub(crate) q: Option<String>,
    pub(crate) limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct Suggestion {
    pub(crate) url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// "tab": one of the profile's default tabs; "history": a page visited before.
    pub(crate) kind: &'static str,
}

/// Addresses to suggest in a session's address bar: the profile's default tabs,
/// then the pages visited in the profile (or the session).
pub(crate) async fn api_session_history(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<HistoryParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile_id = {
        let guard = state.sessions.read().await;
        guard
            .get(&id)
            .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?
            .profile_id
    };
    let query = params.q.unwrap_or_default().trim().to_lowercase();
    let limit = params.limit.unwrap_or(8).clamp(1, 50);
    let mut out: Vec<Suggestion> = Vec::new();
    if let Some(pid) = profile_id {
        if let Some(profile) = profiles::find(&state.config.profiles_dir, &pid.to_string()).await {
            for tab in profile.meta.tabs {
                if query.is_empty() || tab.to_lowercase().contains(&query) {
                    out.push(Suggestion {
                        url: tab,
                        title: None,
                        kind: "tab",
                    });
                }
            }
        }
    }
    for e in state
        .history
        .suggest(&history_key(profile_id, id), &query, limit)
    {
        match out.iter_mut().find(|o| o.url == e.url) {
            Some(known) => known.title = e.title,
            None => out.push(Suggestion {
                url: e.url,
                title: e.title,
                kind: "history",
            }),
        }
    }
    out.truncate(limit);
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(crate) struct IdleTimeoutParams {
    pub(crate) minutes: u64,
}

/// Changes how long a session may sit unused (0 = never stop it) and
/// restarts its idle clock.
pub(crate) async fn api_session_idle_timeout(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<IdleTimeoutParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let guard = state.sessions.read().await;
    let session = guard.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Session not found" })),
    ))?;
    let secs = idle::minutes_to_secs(params.minutes);
    session.activity.set_timeout_secs(secs);
    Ok(Json(serde_json::json!({
        "session_id": id,
        "idle_timeout_seconds": secs
    })))
}

#[derive(Deserialize)]
pub(crate) struct AgentTopic {
    pub(crate) conversation: String,
    pub(crate) topic: String,
}

/// An agent labels its browser connections with what it is doing (its
/// `browser-agent-topic` command). No cluster secret: only a conversation that is
/// connected can be labelled, and the label is all that changes.
pub(crate) async fn api_agent_topic(
    State(state): State<AppState>,
    Json(req): Json<AgentTopic>,
) -> Result<impl IntoResponse, ApiError> {
    let topic = clients::clean(Some(&req.topic));
    let conversation = req.conversation.trim();
    if conversation.len() < 8 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Missing conversation id",
        ));
    }
    let sessions: Vec<ManagedSession> = state.sessions.read().await.values().cloned().collect();
    let labelled: usize = sessions
        .iter()
        .map(|s| s.clients.set_topic(conversation, topic.as_deref()))
        .sum();
    Ok(Json(serde_json::json!({ "clients": labelled })))
}

#[derive(Deserialize)]
pub(crate) struct RenameRequest {
    /// Empty: back to the generated name.
    pub(crate) name: String,
}

pub(crate) async fn api_session_rename(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RenameRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    let name = names::clean(&req.name).unwrap_or_else(|| names::generate(id));
    *session.name.write().unwrap() = name.clone();
    Ok(Json(serde_json::json!({ "name": name })))
}

/// The session's display and its size, with the xdotool to drive it.
pub(crate) async fn session_x(
    state: &AppState,
    id: Uuid,
) -> Result<(PathBuf, String, (u32, u32)), ApiError> {
    let xdotool = state
        .config
        .session_displays
        .as_ref()
        .and_then(|d| d.xdotool.clone())
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "This node has no xdotool"))?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    match (session.display.clone(), session.display_size) {
        (Some(display), Some(size)) => Ok((xdotool, display, size)),
        _ => Err(api_error(
            StatusCode::NOT_FOUND,
            "No display of its own for this session",
        )),
    }
}

/// Native popups open on the session's display (context menu, `<select>` list).
pub(crate) async fn api_session_popups(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let (xdotool, display, size) = session_x(&state, id).await?;
    let popups = display::popups(&xdotool, &display, size).await;
    Ok(Json(serde_json::json!({ "popups": popups })))
}

/// Mouse or key input for the session's display, for a view while a popup is open.
pub(crate) async fn api_session_x_input(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<display::XInput>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let (xdotool, display, size) = session_x(&state, id).await?;
    if !display::send_input(&xdotool, &display, size, &input).await {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input refused"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct InsertTextRequest {
    pub(crate) text: String,
    /// The sites the text may go to (the vault entry's URIs): the page, as the
    /// browser's own page list has it, and the focused frame must be on one,
    /// so a navigation in between cannot send the text elsewhere.
    #[serde(default)]
    pub(crate) uris: Vec<secrets::Uri>,
    /// Press Enter afterwards.
    #[serde(default)]
    pub(crate) enter: bool,
    /// The session the page must belong to (the caller's own).
    #[serde(default)]
    pub(crate) session_id: Option<Uuid>,
}

/// Types text into the focused element of a Chromium page (see page_input.rs).
/// The text is never logged.
pub(crate) async fn api_page_insert_text(
    AxumPath(page_id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<InsertTextRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page = resolve_page(&targets, &page_id, req.session_id)
        .map_err(|e| api_error(StatusCode::NOT_FOUND, e))?;
    let session = match page.session_id {
        Some(sid) => state.sessions.read().await.get(&sid).cloned(),
        None => None,
    }
    .filter(|s| s.bidi.is_none())
    .ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "Page not found (Chromium sessions only)",
        )
    })?;
    let page_url = page.url.clone();
    if !secrets::url_matches(&page_url, &req.uris) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "The page is not on a site the entry is saved for",
        ));
    }
    session.activity.touch();
    let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, page_id);
    let uris = req.uris;
    page_input::insert_text(&ws_url, &req.text, req.enter, move |u| {
        secrets::url_matches(u, &uris)
    })
    .await
    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct SetProxyRequest {
    /// Proxy id, or "none" for direct connections.
    pub(crate) proxy: String,
}

/// Switch a running session's proxy. Open connections are dropped, so pages
/// reconnect through the new route; nothing is restarted.
pub(crate) async fn api_session_set_proxy(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetProxyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let proxy = state
        .check_proxy(&req.proxy)?
        .and_then(|id| state.config.proxies.iter().find(|p| p.id == id).cloned());
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    session.relay.set(proxy);
    let rec = state.active_recordings.read().await.get(&id).cloned();
    Ok(Json(
        session.to_summary(&state.http_client, rec.as_ref()).await,
    ))
}

/// Largest file handoff can drop into a page.
pub(crate) const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

/// Files dropped into a session's pages, removed when the session stops.
pub(crate) fn upload_dir(session: &Uuid) -> PathBuf {
    std::env::temp_dir()
        .join("browser-daemon-uploads")
        .join(session.to_string())
}

/// A file name that is safe as a single path component, keeping it readable.
pub(crate) fn upload_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_string();
    if clean.is_empty() {
        "upload".to_string()
    } else {
        clean.chars().take(200).collect()
    }
}

#[derive(Deserialize)]
pub(crate) struct UploadParams {
    pub(crate) name: String,
}

/// Store a file on this node so a page of the session can receive it (handoff drops it
/// with CDP drag events, which take local paths). Returns the file's path.
pub(crate) async fn api_session_upload_file(
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<UploadParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<impl IntoResponse, ApiError> {
    use futures_util::StreamExt;
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    // One directory per upload: the same name can be dropped twice.
    let dir = upload_dir(&id).join(Uuid::new_v4().to_string());
    let path = dir.join(upload_file_name(&params.name));
    let io = |e: std::io::Error| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    fs::create_dir_all(&dir).await.map_err(io)?;
    // Dropped files can be private: keep them away from other users of the host.
    let root = upload_dir(&id);
    let root = root.parent().unwrap_or(&root);
    fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(io)?;
    // Written as it arrives, not held in memory.
    let mut file = fs::File::create(&path).await.map_err(io)?;
    let mut chunks = Box::pin(capped_chunks(body, MAX_UPLOAD_BYTES));
    let mut size = 0u64;
    let stored: Result<(), ApiError> = async {
        while let Some(chunk) = chunks.next().await {
            let chunk =
                chunk.map_err(|e| api_error(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()))?;
            size += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(io)?;
        }
        file.flush().await.map_err(io)
    }
    .await;
    if let Err(e) = stored {
        let _ = fs::remove_dir_all(&dir).await;
        return Err(e);
    }
    info!(session_id = %id, path = %path.display(), bytes = size, "Stored dropped file");
    Ok(Json(serde_json::json!({
        "path": path,
        "size_bytes": size,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_names_stay_in_their_directory() {
        assert_eq!(upload_file_name("report.pdf"), "report.pdf");
        assert_eq!(upload_file_name("../../etc/passwd"), "passwd");
        assert_eq!(upload_file_name("C:\\Users\\me\\a b.txt"), "a b.txt");
        assert_eq!(upload_file_name(".."), "upload");
        assert_eq!(upload_file_name(".bashrc"), "bashrc");
        assert_eq!(upload_file_name("a\nb"), "ab");
    }
}
