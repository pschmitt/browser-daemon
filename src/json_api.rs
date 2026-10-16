//! The DevTools-compatible /json/* endpoints.

use super::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DevToolsTarget {
    pub id: String,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub target_type: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devtools_frontend_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_socket_debugger_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<BrowserEngine>,
    /// Playing sound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audible: bool,
}

pub(crate) async fn json_version() -> impl IntoResponse {
    Json(serde_json::json!({
        "Browser": "Chrome/153.0.0.0",
        "Protocol-Version": "1.3",
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36",
        "V8-Version": "15.3.76.4",
        "WebKit-Version": "537.36",
        "webSocketDebuggerUrl": "ws://localhost:3001/chromium"
    }))
}

/// The tabs one session shows, as a DevTools target list.
pub(crate) async fn session_targets(
    state: &AppState,
    session: &ManagedSession,
) -> Vec<DevToolsTarget> {
    if let Some(ref hub) = session.bidi {
        return hub
            .contexts()
            .await
            .into_iter()
            .map(|ctx| firefox_target(session, ctx))
            .collect();
    }
    let url = format!("http://127.0.0.1:{}/json/list", session.port);
    let Ok(res) = state.http_client.get(&url).send().await else {
        return Vec::new();
    };
    let mut targets = res.json::<Vec<DevToolsTarget>>().await.unwrap_or_default();
    for target in &mut targets {
        target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
        target.session_id = Some(session.id);
        target.profile = session.profile.clone();
        target.engine = Some(BrowserEngine::Chromium);
    }
    targets
}

pub(crate) async fn snapshot_sessions(state: &AppState) -> Vec<ManagedSession> {
    state.sessions.read().await.values().cloned().collect()
}

pub(crate) async fn get_local_targets(state: &AppState) -> Vec<DevToolsTarget> {
    let sessions = snapshot_sessions(state).await;
    // All sessions at once: a hung one costs one wait, not one per session.
    let mut all_targets: Vec<DevToolsTarget> = futures_util::future::join_all(
        sessions
            .iter()
            .map(|session| session_targets(state, session)),
    )
    .await
    .into_iter()
    .flatten()
    .collect();

    if let Ok(mut order) = state.tab_order.lock() {
        order.sort(&mut all_targets);
    }
    if let Ok(audible) = state.audible_pages.read() {
        for t in &mut all_targets {
            t.audible = audible.contains(&t.id);
        }
    }
    all_targets
}

/// The session showing a tab, and the tab. A page id that more than one
/// session shows is refused: it cannot be told whose it is.
pub(crate) async fn find_page(
    state: &AppState,
    page_id: &str,
) -> Option<(ManagedSession, DevToolsTarget)> {
    let sessions = snapshot_sessions(state).await;
    let found = futures_util::future::join_all(sessions.into_iter().map(|session| async move {
        let target = session_targets(state, &session)
            .await
            .into_iter()
            .find(|t| t.id == page_id)?;
        Some((session, target))
    }))
    .await;
    let mut found = found.into_iter().flatten();
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// The one page `page_id` names, in `session` when given. No page, or a page id
/// several sessions show (when no session is named), is an error.
pub(crate) fn resolve_page<'a>(
    targets: &'a [DevToolsTarget],
    page_id: &str,
    session: Option<Uuid>,
) -> Result<&'a DevToolsTarget, &'static str> {
    let mut hits = targets
        .iter()
        .filter(|t| t.id == page_id && session.is_none_or(|s| t.session_id == Some(s)));
    let first = hits.next().ok_or("Page not found")?;
    if hits.next().is_some() {
        return Err("The page id is not unique across sessions: name the session");
    }
    Ok(first)
}

pub(crate) fn firefox_target(session: &ManagedSession, ctx: bidi::BidiContext) -> DevToolsTarget {
    DevToolsTarget {
        id: ctx.id,
        title: ctx.title,
        target_type: "page".to_string(),
        url: ctx.url,
        devtools_frontend_url: None,
        web_socket_debugger_url: Some(format!("/firefox?session_id={}", session.id)),
        session_id: Some(session.id),
        profile: session.profile.clone(),
        engine: Some(BrowserEngine::Firefox),
        audible: false,
    }
}

pub(crate) async fn json_list(State(state): State<AppState>) -> impl IntoResponse {
    Json(get_local_targets(&state).await)
}

/// The error reply of the DevTools-style routes.
pub(crate) fn json_err(status: StatusCode, msg: impl ToString) -> ApiError {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// Opens a tab. It belongs to the session named by `session_id`, or the one
/// showing the tab `page`; neither given, a session of its own is started. Never
/// another session's: that could be somebody's logged-in profile.
pub(crate) async fn json_new(
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let raw = raw_query.unwrap_or_default();
    let mut target_url: Option<String> = None;
    let mut session_id: Option<Uuid> = None;
    let mut page: Option<String> = None;

    if !raw.is_empty() {
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            match k.as_ref() {
                "url" => target_url = Some(v.into_owned()),
                "session_id" => session_id = Uuid::parse_str(&v).ok(),
                "page" => page = Some(v.into_owned()),
                _ => {}
            }
        }
        if target_url.is_none() && !raw.starts_with("session_id=") && !raw.starts_with("page=") {
            let decoded = url::form_urlencoded::parse(raw.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_else(|| raw.clone());
            target_url = Some(decoded);
        }
    }
    let target_url = target_url.unwrap_or_else(|| "about:blank".to_string());

    let session = if let Some(sid) = session_id {
        state
            .sessions
            .read()
            .await
            .get(&sid)
            .cloned()
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "Session not found"))?
    } else if let Some(ref pid) = page {
        find_page(&state, pid)
            .await
            .map(|(session, _)| session)
            .ok_or_else(|| json_err(StatusCode::NOT_FOUND, "Page not found"))?
    } else {
        state
            .spawn_session(
                BrowserEngine::Chromium,
                None,
                false,
                None,
                None,
                Some(state.recorder_config.enabled),
                None,
                None,
                None,
                None,
                None,
                true,
                true,
                Vec::new(),
            )
            .await
            .map_err(|(st, err)| json_err(st, err))?
    };

    session.activity.touch();
    if let Some(ref hub) = session.bidi {
        // Like Chromium below: an untouched about:blank start tab gives way to the new one.
        let start_tab =
            if target_url != "about:blank" && session.client_count.load(Ordering::SeqCst) == 0 {
                match hub.contexts().await.as_slice() {
                    [only] if only.url == "about:blank" => Some(only.id.clone()),
                    _ => None,
                }
            } else {
                None
            };
        let ctx = hub
            .create_tab(&target_url)
            .await
            .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;
        if let Some(blank) = start_tab.filter(|id| *id != ctx.id) {
            if let Err(e) = hub
                .call(
                    "browsingContext.close",
                    serde_json::json!({ "context": blank }),
                )
                .await
            {
                warn!(session_id = %session.id, error = %e, "Could not close the blank start tab");
            }
        }
        if let Ok(mut order) = state.tab_order.lock() {
            order.rank(&ctx.id);
        }
        return Ok(Json(firefox_target(&session, ctx)));
    }

    // Chromium starts every session on an about:blank tab. When that is still the only
    // page and nothing is attached that might be about to use it, it is replaced by the
    // new tab instead of lingering as an empty second tab.
    let start_tab =
        if target_url != "about:blank" && session.client_count.load(Ordering::SeqCst) == 0 {
            let pages: Vec<_> = session_targets(&state, &session)
                .await
                .into_iter()
                .filter(|t| t.target_type == "page")
                .collect();
            match pages.as_slice() {
                [only] if only.url == "about:blank" => Some(only.id.clone()),
                _ => None,
            }
        } else {
            None
        };

    let mut dest_url = url::Url::parse(&format!("http://127.0.0.1:{}/json/new", session.port))
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    dest_url.set_query(Some(&target_url));

    let res = state
        .http_client
        .put(dest_url)
        .send()
        .await
        .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;

    let mut target = res
        .json::<DevToolsTarget>()
        .await
        .map_err(|e| json_err(StatusCode::BAD_GATEWAY, e))?;

    if let Some(blank) = start_tab.filter(|id| *id != target.id) {
        let close_url = format!("http://127.0.0.1:{}/json/close/{}", session.port, blank);
        if let Err(e) = state.http_client.get(&close_url).send().await {
            warn!(session_id = %session.id, error = %e, "Could not close the blank start tab");
        }
    }

    target.web_socket_debugger_url = Some(format!("/devtools/page/{}", target.id));
    target.session_id = Some(session.id);
    target.profile = session.profile.clone();
    target.engine = Some(BrowserEngine::Chromium);
    if let Ok(mut order) = state.tab_order.lock() {
        order.rank(&target.id);
    }

    Ok(Json(target))
}

pub(crate) async fn json_close(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some((session, _)) = find_page(&state, &id).await else {
        return (StatusCode::NOT_FOUND, "Target not found").into_response();
    };
    if let Some(ref hub) = session.bidi {
        return match hub.close_tab(&id).await {
            Ok(()) => (StatusCode::OK, "Target is closing").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        };
    }
    let req_url = format!("http://127.0.0.1:{}/json/close/{}", session.port, id);
    match state.http_client.put(&req_url).send().await {
        Ok(res) if res.status().is_success() => {
            (StatusCode::OK, "Target is closing").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Target not found").into_response(),
    }
}

pub(crate) async fn json_activate(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some((session, _)) = find_page(&state, &id).await else {
        return (StatusCode::NOT_FOUND, "Target not found").into_response();
    };
    if let Some(ref hub) = session.bidi {
        return match hub.activate_tab(&id).await {
            Ok(()) => (StatusCode::OK, "Target activated").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        };
    }
    let req_url = format!("http://127.0.0.1:{}/json/activate/{}", session.port, id);
    match state.http_client.put(&req_url).send().await {
        Ok(res) if res.status().is_success() => {
            (StatusCode::OK, "Target activated").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "Target not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, session: Uuid) -> DevToolsTarget {
        DevToolsTarget {
            id: id.into(),
            title: None,
            target_type: "page".into(),
            url: "https://x.test/".into(),
            devtools_frontend_url: None,
            web_socket_debugger_url: None,
            session_id: Some(session),
            profile: None,
            engine: None,
            audible: false,
        }
    }

    #[test]
    fn a_page_is_resolved_within_its_session() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let targets = vec![page("p1", a), page("p2", b), page("dup", a), page("dup", b)];
        assert_eq!(
            resolve_page(&targets, "p1", None).unwrap().session_id,
            Some(a)
        );
        assert!(
            resolve_page(&targets, "p1", Some(b)).is_err(),
            "other session"
        );
        assert!(resolve_page(&targets, "dup", None).is_err(), "ambiguous");
        assert_eq!(
            resolve_page(&targets, "dup", Some(b)).unwrap().session_id,
            Some(b)
        );
        assert!(resolve_page(&targets, "nope", None).is_err());
    }
}
