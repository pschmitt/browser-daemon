//! WebSocket proxying between automation clients and the browsers.

use super::*;

/// Registers a connecting automation client with its session; `client_gone` with
/// the returned id when it disconnects.
pub(crate) async fn client_arrived(
    state: &AppState,
    session: &ManagedSession,
    headers: &HeaderMap,
    params: &LaunchParams,
) -> u64 {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (kind, agent) = clients::kind_of(ua.as_deref());
    // This node's own address too: a client may come in over it (the mesh URL).
    let mut peers: Vec<(String, String)> =
        vec![(state.node_name.clone(), state.public_url.clone())];
    peers.extend(
        state
            .peers
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    let addr = clients::remote_addr(headers);
    let from = clients::describe_origin(addr, &state.node_name, &peers).await;
    info!(session_id = %session.id, kind, from = %from, "Automation client connected");
    session.clients.add(clients::ClientInfo {
        kind: kind.into(),
        agent,
        from,
        addr: addr.map(|a| a.to_string()),
        user_agent: ua,
        since: clients::now_secs(),
        agent_name: clients::clean(params.agent.as_deref()),
        conversation: clients::clean(params.conversation.as_deref()),
        cwd: clients::clean(params.cwd.as_deref()),
        machine: clients::clean(params.machine.as_deref()),
        topic: None,
    })
}

/// An automation client's place on a session. Taken before the WebSocket upgrade
/// and given back however it ends, so a client that vanishes during the upgrade
/// (or a failed upgrade) cannot leave a non-persistent session running unused.
pub(crate) struct ClientSlot {
    pub(crate) state: AppState,
    pub(crate) session: ManagedSession,
    /// Counts as a client (a live view's observer does not).
    pub(crate) counted: bool,
    pub(crate) client: Option<u64>,
}

impl ClientSlot {
    pub(crate) fn new(state: &AppState, session: &ManagedSession, counted: bool) -> Self {
        if counted {
            session.client_count.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            state: state.clone(),
            session: session.clone(),
            counted,
            client: None,
        }
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.session.clients.remove(client);
        }
        if !self.counted {
            return;
        }
        let remaining = self.session.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if !self.session.persistent && remaining == 0 {
            let state = self.state.clone();
            let id = self.session.id;
            tokio::spawn(async move {
                info!(session_id = %id, "Auto-terminating non-persistent session");
                state.terminate_session(&id).await;
            });
        }
    }
}

pub(crate) async fn handle_chromium_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    // Playwright never takes over tabs it didn't open: it opens its own and the start
    // tab would just sit there. Other clients (Puppeteer scripts take the first tab)
    // keep it.
    let agent_opens_own_tab = clients::kind_of(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .0 == "Playwright";
    let session = if let Some(sid) = params.session_id {
        let guard = state.sessions.read().await;
        match guard.get(&sid).cloned() {
            Some(s) => s,
            None => {
                return (StatusCode::NOT_FOUND, "Requested session ID not found").into_response();
            }
        }
    } else {
        let persistent = params.persistent.unwrap_or(false);
        match state
            .spawn_session(
                params.engine.unwrap_or(BrowserEngine::Chromium),
                params.profile.as_deref(),
                persistent,
                params.width,
                params.height,
                params.record,
                params.max_memory_mb,
                params.idle_timeout_minutes,
                params.adblock,
                params.proxy.as_deref(),
                params.color_scheme,
                !agent_opens_own_tab,
                // Playwright ignores tabs it did not open: the default tabs would just
                // sit there. It can still ask (`default_tabs=true`).
                params.default_tabs.unwrap_or(!agent_opens_own_tab),
                Vec::new(),
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    let mut slot = ClientSlot::new(&state, &session, true);
    ws.on_upgrade(move |socket| async move {
        slot.client = Some(client_arrived(&state, &session, &headers, &params).await);
        session.activity.touch();

        let ws_path = session.ws_path.trim_start_matches('/');
        let backend_ws_url = format!("ws://127.0.0.1:{}/{}", session.port, ws_path);
        info!(session_id = %session.id, url = %backend_ws_url, "Connecting client to Chromium DevTools");

        let frame_tx = {
            let recs = state.active_recordings.read().await;
            recs.get(&session.id).map(|h| h.frame_tx.clone())
        };

        match tokio_tungstenite::connect_async(&backend_ws_url).await {
            Ok((server_ws, _)) => {
                let activity = Some((session.activity.clone(), false));
                bridge_websockets(
                    socket,
                    server_ws,
                    frame_tx,
                    proxy::FrameSource {
                        port: session.port,
                        page: None,
                    },
                    activity,
                    None,
                    None,
                    true,
                    session.profile_id.is_some(),
                )
                .await;
            }
            Err(e) => {
                error!(session_id = %session.id, error = %e, "Failed to connect to Chromium backend");
            }
        }

        drop(slot);
    })
}

pub(crate) async fn handle_firefox_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if state.config.firefox_bin.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Firefox engine is not configured on this host",
        )
            .into_response();
    }

    let session = if let Some(sid) = params.session_id {
        let guard = state.sessions.read().await;
        match guard.get(&sid).cloned() {
            Some(s) => s,
            None => {
                return (StatusCode::NOT_FOUND, "Requested session ID not found").into_response();
            }
        }
    } else {
        let persistent = params.persistent.unwrap_or(false);
        match state
            .spawn_session(
                BrowserEngine::Firefox,
                params.profile.as_deref(),
                persistent,
                params.width,
                params.height,
                params.record,
                params.max_memory_mb,
                params.idle_timeout_minutes,
                params.adblock,
                params.proxy.as_deref(),
                params.color_scheme,
                true,
                params.default_tabs.unwrap_or(true),
                Vec::new(),
            )
            .await
        {
            Ok(s) => s,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    };

    let Some(hub) = session.bidi.clone() else {
        return (StatusCode::BAD_REQUEST, "Not a Firefox session").into_response();
    };
    // The handoff page attaches as an observer: it must not keep sessions alive.
    let counted = !params.observer.unwrap_or(false);

    let mut slot = ClientSlot::new(&state, &session, counted);
    ws.on_upgrade(move |socket| async move {
        if counted {
            slot.client = Some(client_arrived(&state, &session, &headers, &params).await);
        }
        info!(session_id = %session.id, observer = !counted, "Client attached to Firefox WebDriver BiDi");

        session.activity.touch();
        hub.serve_client(socket, session.activity.clone(), !counted)
            .await;
        drop(slot);
    })
}

pub(crate) async fn handle_page_ws(
    AxumPath(page_id): AxumPath<String>,
    ws: WebSocketUpgrade,
    Query(params): Query<LaunchParams>,
    State(state): State<AppState>,
) -> Response {
    let found = find_page(&state, &page_id)
        .await
        .filter(|(session, _)| session.bidi.is_none());
    let Some((session, target)) = found else {
        warn!(page_id = %page_id, "Page not found across active sessions");
        return (StatusCode::NOT_FOUND, "Target page not found").into_response();
    };
    let (port, session_id, activity) = (session.port, session.id, session.activity.clone());
    let (initial_title, initial_url) = (target.title, Some(target.url));

    let frame_tx = {
        let recs = state.active_recordings.read().await;
        recs.get(&session_id).map(|h| h.frame_tx.clone())
    };

    // The handoff page attaches as an observer: watching alone is not activity.
    let observer = params.observer.unwrap_or(false);
    if !observer {
        activity.touch();
    }
    let backend_url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, page_id);
    ws.on_upgrade(move |socket| async move {
        match tokio_tungstenite::connect_async(&backend_url).await {
            Ok((server_ws, _)) => {
                let activity = Some((activity, observer));
                bridge_websockets(
                    socket,
                    server_ws,
                    frame_tx,
                    proxy::FrameSource {
                        port,
                        page: Some(page_id.clone()),
                    },
                    activity,
                    initial_url,
                    initial_title,
                    false,
                    false,
                )
                .await;
            }
            Err(e) => {
                error!(page_id = %page_id, error = %e, "Failed to bridge to target page");
            }
        }
    })
}

// ---------------------------------------------------------------------------
// REST API Handlers
// ---------------------------------------------------------------------------

/// Chromium's own DevTools frontend for a session (bundled with the browser, served
/// on its debugging port), for the live view's DevTools button. It connects to a page
/// through /devtools/page/{id} like the view does.
pub(crate) async fn devtools_frontend(
    AxumPath((session_id, path)): AxumPath<(Uuid, String)>,
    State(state): State<AppState>,
) -> Response {
    let port = state
        .sessions
        .read()
        .await
        .get(&session_id)
        .filter(|s| s.engine == BrowserEngine::Chromium)
        .map(|s| s.port);
    let Some(port) = port else {
        return (StatusCode::NOT_FOUND, "No Chromium session with this id").into_response();
    };
    if path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let res = match state
        .http_client
        .get(format!("http://127.0.0.1:{port}/devtools/{path}"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
    let body = match res.bytes().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let mut resp = (status, body).into_response();
    if let Some(ct) = content_type {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    resp
}
