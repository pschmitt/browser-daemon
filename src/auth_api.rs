//! Handlers for a page's login (see auth_export.rs).

use super::*;

#[derive(Deserialize)]
pub(crate) struct AuthRequest {
    /// The session the page is in (needed when the page id is not unique).
    #[serde(default)]
    pub(crate) session_id: Option<Uuid>,
    /// More addresses to collect cookies for, besides the page's own.
    #[serde(default)]
    pub(crate) urls: Vec<String>,
    /// Also read the page's local and session storage.
    #[serde(default)]
    pub(crate) storage: bool,
}

/// The cookies (and storage) of a session's page. Never logged.
pub(crate) async fn api_page_auth(
    AxumPath(page_id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AuthRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page = resolve_page(&targets, &page_id, req.session_id)
        .map_err(|e| api_error(StatusCode::NOT_FOUND, e))?;
    let session = match page.session_id {
        Some(sid) => state.sessions.read().await.get(&sid).cloned(),
        None => None,
    }
    .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found"))?;
    let urls = auth_export::urls(&page.url, &req.urls)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    session.activity.touch();
    info!(session_id = %session.id, cookies_for = urls.len(), storage = req.storage, "Login read for an agent");
    let out = match session.bidi {
        Some(ref hub) => auth_export::firefox(hub, &page_id, &page.url, &urls, req.storage).await,
        None => {
            let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, page_id);
            auth_export::chromium(&ws_url, &page.url, &urls, req.storage).await
        }
    }
    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
    Ok(([("cache-control", "no-store")], Json(out)))
}

/// The cookies (and storage) of a shared tab its owner lets agents control.
pub(crate) async fn api_extension_auth(
    AxumPath((id, tab)): AxumPath<(Uuid, u32)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AuthRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let shared = crate::extension::controlled(&state, id, tab)?;
    let urls = auth_export::urls(&shared.url, &req.urls)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    info!(browser = %id, cookies_for = urls.len(), storage = req.storage, "Login of a shared tab read for an agent");
    let out = shared.auth(&urls, req.storage).await?;
    Ok(([("cache-control", "no-store")], Json(out)))
}
