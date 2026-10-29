//! Playwright and other CDP clients against a shared browser (`connectOverCDP`):
//! a browser-level CDP endpoint whose targets are the tabs shared with control.
//! Each attached target is a relay channel to the extension, as for live views,
//! with the automation methods (`automation_allowed`) on top of the view's. The
//! browser's own context only: no new contexts, downloads or browser-wide state.
//! Chromium only (Firefox's extension emulates just the live view's subset).

use super::*;

const PRODUCT: &str = "Chrome/153.0.0.0";
const CONTEXT: &str = "shared-browser";

fn node_for(state: &AppState, id: Uuid) -> Result<Arc<Node>, ApiError> {
    let node = state
        .extensions
        .nodes
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Browser disconnected"))?;
    if node.deadline.is_some_and(|d| d <= Instant::now()) {
        return Err(failure(StatusCode::GONE, "Browser grant expired"));
    }
    let info = node.info.lock().unwrap();
    if info.browser == "firefox" {
        return Err(failure(
            StatusCode::NOT_IMPLEMENTED,
            "CDP clients need a Chromium browser (Firefox's extension serves live views only)",
        ));
    }
    if !version_at_least(&info.version, (0, 3, 10)) {
        return Err(failure(
            StatusCode::UPGRADE_REQUIRED,
            "Update the browser extension (0.3.10 or later) for CDP clients",
        ));
    }
    drop(info);
    Ok(node)
}

fn version_at_least(version: &str, want: (u64, u64, u64)) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let have = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    have >= want
}

/// `GET .../json/version`, so `connectOverCDP("https://<daemon>/api/extension/nodes/<id>")`
/// finds the socket. It is addressed as the client addressed this request.
pub async fn version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    node_for(&state, id)?;
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let scheme = if secure { "wss" } else { "ws" };
    Ok(Json(json!({
        "Browser": PRODUCT,
        "Protocol-Version": "1.3",
        "webSocketDebuggerUrl": format!("{scheme}://{host}/api/extension/nodes/{id}/cdp"),
    }))
    .into_response())
}

/// The browser-level CDP socket.
pub async fn socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    let node = node_for(&state, id)?;
    if node.channels.lock().unwrap().len() >= MAX_CHANNELS {
        return Err(failure(StatusCode::TOO_MANY_REQUESTS, "Too many clients"));
    }
    tracing::info!(browser = %id, "CDP client connected to a shared browser");
    Ok(ws
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |mut ws| async move {
            Facade::new(node).run(&mut ws).await;
            let _ = ws.close().await;
        }))
}

struct Session {
    tab: u32,
    channel: Uuid,
}

struct Facade {
    node: Arc<Node>,
    sessions: HashMap<String, Session>,
    known: HashMap<u32, Tab>,
    // Target ids: a tab's main frame id, as in Chrome (Playwright relies on it).
    ids: HashMap<u32, String>,
    // Tabs missing from the shared list since then: a navigating tab drops out for
    // a moment, and is only gone when it stays away.
    missing: HashMap<u32, Instant>,
    browser_sessions: std::collections::HashSet<String>,
    discover: bool,
    auto_attach: bool,
    seen: u64,
    // Each session's relay channel feeds this, tagged with the session.
    tagged: mpsc::Sender<(String, String)>,
    incoming: mpsc::Receiver<(String, String)>,
}

impl Facade {
    fn new(node: Arc<Node>) -> Self {
        let (tagged, incoming) = mpsc::channel(512);
        Self {
            node,
            sessions: HashMap::new(),
            known: HashMap::new(),
            ids: HashMap::new(),
            missing: HashMap::new(),
            browser_sessions: Default::default(),
            discover: false,
            auto_attach: false,
            seen: u64::MAX,
            tagged,
            incoming,
        }
    }

    fn target_id(&self, tab: u32) -> String {
        self.ids
            .get(&tab)
            .cloned()
            .unwrap_or_else(|| format!("tab-{tab}"))
    }
    fn tab_of(&self, target: &str) -> Option<u32> {
        self.ids
            .iter()
            .find(|(_, id)| id.as_str() == target)
            .map(|(tab, _)| *tab)
            .or_else(|| target.strip_prefix("tab-")?.parse().ok())
    }
    fn target_info(&self, t: &Tab, attached: bool) -> Value {
        json!({"targetId": self.target_id(t.id), "type": "page", "title": t.title, "url": t.url,
            "attached": attached, "canAccessOpener": false, "browserContextId": CONTEXT})
    }
    /// The tab's main frame id, from the browser (once per tab).
    async fn resolve(&mut self, tab: u32) {
        if self.ids.contains_key(&tab) {
            return;
        }
        if let Ok(answer) = ask(&self.node, tab, "frames", json!({})).await {
            if let Some(frame) = answer["result"][0]["frame"].as_str() {
                self.ids.insert(tab, frame.to_string());
            }
        }
    }

    fn controlled(&self) -> Vec<Tab> {
        let info = self.node.info.lock().unwrap();
        info.tabs.iter().filter(|t| t.control).cloned().collect()
    }

    /// The browser is still connected (its socket's loop holds the receiver) and its grant valid.
    fn alive(&self) -> bool {
        !self.node.outbound.is_closed() && self.node.deadline.is_none_or(|d| d > Instant::now())
    }

    async fn run(mut self, ws: &mut WebSocket) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut out: Vec<Value> = Vec::new();
        self.known = self.controlled().into_iter().map(|t| (t.id, t)).collect();
        let tabs: Vec<u32> = self.known.keys().copied().collect();
        for tab in tabs {
            self.resolve(tab).await;
        }
        self.seen = self.node.revision.load(std::sync::atomic::Ordering::SeqCst);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // A gone or replaced browser ends the connection (its sessions close with it).
                    if !self.alive() { break; }
                    let revision = self.node.revision.load(std::sync::atomic::Ordering::SeqCst);
                    if revision != self.seen || !self.missing.is_empty() {
                        self.seen = revision;
                        self.sync(&mut out).await;
                    }
                }
                tagged = self.incoming.recv() => {
                    let Some((session, text)) = tagged else { break };
                    let Ok(mut value) = serde_json::from_str::<Value>(&text) else { continue };
                    value["sessionId"] = json!(session);
                    out.push(value);
                }
                incoming = ws.recv() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        _ => break,
                    };
                    let Ok(request) = serde_json::from_str::<Value>(&text) else { break };
                    let Some(id) = request["id"].as_u64() else { break };
                    let method = request["method"].as_str().unwrap_or_default().to_string();
                    let params = if request["params"].is_object() { request["params"].clone() } else { json!({}) };
                    let answer = match request["sessionId"].as_str() {
                        // A browser session (Target.attachToBrowserTarget) is the browser again.
                        Some(session) if self.browser_sessions.contains(session) => {
                            self.on_browser(Some(session), &method, params, &mut out).await
                        }
                        Some(session) => self.on_session(session, id, &method, params, &mut out).await,
                        None => self.on_browser(None, &method, params, &mut out).await,
                    };
                    match answer {
                        Some(Ok(result)) => {
                            let mut reply = json!({"id": id, "result": result});
                            if let Some(s) = request["sessionId"].as_str() { reply["sessionId"] = json!(s); }
                            out.push(reply);
                        }
                        Some(Err(message)) => {
                            let mut reply = json!({"id": id, "error": {"code": -32000, "message": message}});
                            if let Some(s) = request["sessionId"].as_str() { reply["sessionId"] = json!(s); }
                            out.push(reply);
                        }
                        None => {} // forwarded: the extension answers on the session's channel
                    }
                }
            }
            for value in out.drain(..) {
                if ws
                    .send(Message::Text(value.to_string().into()))
                    .await
                    .is_err()
                {
                    self.close_all();
                    return;
                }
            }
        }
        self.close_all();
    }

    /// Shared tabs came, went or changed: targets follow, as Chrome reports them.
    async fn sync(&mut self, out: &mut Vec<Value>) {
        let now = self.controlled();
        let absent: Vec<u32> = self
            .known
            .keys()
            .filter(|id| !now.iter().any(|t| t.id == **id))
            .copied()
            .collect();
        self.missing.retain(|tab, _| absent.contains(tab));
        let mut gone = Vec::new();
        for tab in absent {
            let since = *self.missing.entry(tab).or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(1500) {
                self.missing.remove(&tab);
                gone.push(tab);
            }
        }
        for tab in gone {
            self.known.remove(&tab);
            let target = self.target_id(tab);
            self.ids.remove(&tab);
            let sessions: Vec<String> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.tab == tab)
                .map(|(k, _)| k.clone())
                .collect();
            for session in sessions {
                self.close_session(&session);
                out.push(json!({"method": "Target.detachedFromTarget", "params": {"sessionId": session, "targetId": target}}));
            }
            if self.discover {
                out.push(
                    json!({"method": "Target.targetDestroyed", "params": {"targetId": target}}),
                );
            }
        }
        for t in now {
            match self.known.get(&t.id) {
                None => {
                    self.resolve(t.id).await;
                    if self.discover {
                        out.push(json!({"method": "Target.targetCreated", "params": {"targetInfo": self.target_info(&t, false)}}));
                    }
                    if self.auto_attach {
                        if let Err(error) = self.attach(&t, None, out) {
                            tracing::warn!(%error, tab = t.id, "Could not auto-attach");
                        }
                    }
                    self.known.insert(t.id, t);
                }
                Some(old) if old.url != t.url || old.title != t.title => {
                    let attached = self.sessions.values().any(|s| s.tab == t.id);
                    out.push(json!({"method": "Target.targetInfoChanged", "params": {"targetInfo": self.target_info(&t, attached)}}));
                    self.known.insert(t.id, t);
                }
                Some(_) => {}
            }
        }
    }

    /// A session on the tab; `via`: the browser session that asked (its events go there).
    fn attach(
        &mut self,
        t: &Tab,
        via: Option<&str>,
        out: &mut Vec<Value>,
    ) -> Result<String, String> {
        let session = Uuid::new_v4().simple().to_string().to_uppercase();
        let channel = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<String>(1024);
        if !self.node.insert_channel(
            channel,
            Channel {
                tab: t.id,
                tx,
                client: true,
                automation: true,
            },
        ) {
            return Err("Too many channels".into());
        }
        self.node.notify_client_presence();
        let tagged = self.tagged.clone();
        let name = session.clone();
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if tagged.send((name.clone(), text)).await.is_err() {
                    break;
                }
            }
        });
        self.sessions
            .insert(session.clone(), Session { tab: t.id, channel });
        let mut event = json!({"method": "Target.attachedToTarget", "params": {
            "sessionId": session, "targetInfo": self.target_info(t, true), "waitingForDebugger": false}});
        if let Some(via) = via {
            event["sessionId"] = json!(via);
        }
        out.push(event);
        Ok(session)
    }

    fn close_session(&mut self, session: &str) {
        if let Some(s) = self.sessions.remove(session) {
            self.node.channels.lock().unwrap().remove(&s.channel);
            self.node.notify_client_presence();
            let _ = self
                .node
                .outbound
                .try_send(json!({"type": "cdp_close", "channel": s.channel, "tab": s.tab}));
        }
    }

    fn close_all(&mut self) {
        let all: Vec<String> = self.sessions.keys().cloned().collect();
        for session in all {
            self.close_session(&session);
        }
    }

    /// Browser-level methods, answered here: the shared tabs as page targets.
    async fn on_browser(
        &mut self,
        via: Option<&str>,
        method: &str,
        params: Value,
        out: &mut Vec<Value>,
    ) -> Option<Result<Value, String>> {
        let tab_param = |params: &Value| params["targetId"].as_str().and_then(|t| self.tab_of(t));
        Some(match method {
            "Browser.getVersion" => Ok(json!({"protocolVersion": "1.3", "product": PRODUCT,
                "revision": "", "userAgent": "", "jsVersion": ""})),
            // The owner's browser: its downloads, permissions and windows stay theirs.
            "Browser.setDownloadBehavior"
            | "Browser.grantPermissions"
            | "Browser.resetPermissions"
            | "Browser.setWindowBounds"
            | "Browser.close"
            | "Target.activateTarget" => Ok(json!({})),
            "Browser.getWindowForTarget" => {
                Ok(json!({"windowId": 1, "bounds": {"windowState": "normal"}}))
            }
            "Target.getBrowserContexts" => Ok(json!({"browserContextIds": []})),
            "Target.attachToBrowserTarget" => {
                let session = format!("BROWSER-{}", Uuid::new_v4().simple());
                self.browser_sessions.insert(session.clone());
                Ok(json!({"sessionId": session}))
            }
            "Target.createBrowserContext" => {
                Err("A shared browser has only its own context: use browser.contexts()[0]".into())
            }
            "Target.setDiscoverTargets" => {
                self.discover = params["discover"].as_bool().unwrap_or(false);
                if self.discover {
                    for t in self.known.values() {
                        let attached = self.sessions.values().any(|s| s.tab == t.id);
                        out.push(json!({"method": "Target.targetCreated", "params": {"targetInfo": self.target_info(t, attached)}}));
                    }
                }
                Ok(json!({}))
            }
            "Target.setAutoAttach" => {
                self.auto_attach = params["autoAttach"].as_bool().unwrap_or(false);
                if self.auto_attach {
                    let tabs: Vec<Tab> = self
                        .known
                        .values()
                        .filter(|t| !self.sessions.values().any(|s| s.tab == t.id))
                        .cloned()
                        .collect();
                    for t in tabs {
                        if let Err(error) = self.attach(&t, None, out) {
                            tracing::warn!(%error, tab = t.id, "Could not auto-attach");
                        }
                    }
                }
                Ok(json!({}))
            }
            "Target.getTargets" => Ok(json!({"targetInfos": self.known.values().map(|t| {
                self.target_info(t, self.sessions.values().any(|s| s.tab == t.id))
            }).collect::<Vec<_>>()})),
            "Target.getTargetInfo" => match tab_param(&params) {
                None if params["targetId"].is_null() => Ok(json!({"targetInfo": {
                    "targetId": "browser", "type": "browser", "title": "", "url": "", "attached": true}})),
                tab => match tab.and_then(|t| self.known.get(&t)) {
                    Some(t) => Ok(
                        json!({"targetInfo": self.target_info(t, self.sessions.values().any(|s| s.tab == t.id))}),
                    ),
                    None => Err("No target with given id found".into()),
                },
            },
            "Target.attachToTarget" => {
                match tab_param(&params).and_then(|t| self.known.get(&t).cloned()) {
                    Some(t) => self.attach(&t, via, out).map(|id| json!({"sessionId": id})),
                    None => Err("No target with given id found".into()),
                }
            }
            "Target.detachFromTarget" => {
                let session = params["sessionId"].as_str().unwrap_or_default().to_string();
                match self.sessions.get(&session).map(|s| s.tab) {
                    Some(tab) => {
                        self.close_session(&session);
                        out.push(json!({"method": "Target.detachedFromTarget", "params": {"sessionId": session, "targetId": self.target_id(tab)}}));
                        Ok(json!({}))
                    }
                    None => Err("No session with given id".into()),
                }
            }
            "Target.createTarget" => {
                self.create(params["url"].as_str().unwrap_or("about:blank"), out)
                    .await
            }
            "Target.closeTarget" => match tab_param(&params).filter(|t| self.known.contains_key(t))
            {
                Some(tab) => match ask(&self.node, tab, "close", json!({})).await {
                    Ok(v) if v["error"].is_null() => Ok(json!({"success": true})),
                    Ok(v) => Err(v["error"]
                        .as_str()
                        .unwrap_or("The browser did not close the tab")
                        .to_string()),
                    Err(_) => Err("The browser did not answer".into()),
                },
                None => Err("No target with given id found".into()),
            },
            _ => Err(format!("{method} is not available for a shared browser")),
        })
    }

    /// Opens a tab (shared with control, as the owner allows) and waits for it to
    /// be reported, so it is a target (and attached) before the answer.
    async fn create(&mut self, url: &str, out: &mut Vec<Value>) -> Result<Value, String> {
        if !page_url(url) {
            return Err("Only web pages and about:blank can be opened".into());
        }
        let from = {
            let info = self.node.info.lock().unwrap();
            if info.can_open {
                Some(0)
            } else {
                info.tabs.iter().find(|t| t.control).map(|t| t.id)
            }
        }
        .ok_or("The browser's owner does not let viewers open tabs")?;
        let answer = ask(&self.node, from, "open", json!({"url": url}))
            .await
            .map_err(|_| "The browser did not answer".to_string())?;
        let tab = answer["result"]["tab"]
            .as_u64()
            .and_then(|t| u32::try_from(t).ok())
            .ok_or_else(|| {
                answer["error"]
                    .as_str()
                    .unwrap_or("The browser did not open the tab")
                    .to_string()
            })?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.controlled().iter().any(|t| t.id == tab) {
                self.seen = self.node.revision.load(std::sync::atomic::Ordering::SeqCst);
                self.sync(out).await;
                return Ok(json!({"targetId": self.target_id(tab)}));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("The new tab was not shared in time".into())
    }

    /// Session-level methods: relayed to the tab (checked here and in the browser),
    /// except the few a single tab without child targets answers itself.
    async fn on_session(
        &mut self,
        session: &str,
        id: u64,
        method: &str,
        params: Value,
        _out: &mut Vec<Value>,
    ) -> Option<Result<Value, String>> {
        let Some(s) = self.sessions.get(session) else {
            return Some(Err(format!("Session with given id not found: {session}")));
        };
        let (tab, channel) = (s.tab, s.channel);
        match method {
            // No child targets (workers, out-of-process frames) and nothing waits.
            "Target.setAutoAttach" | "Runtime.runIfWaitingForDebugger" => {
                return Some(Ok(json!({})))
            }
            "Target.getTargetInfo" => {
                return Some(match self.known.get(&tab) {
                    Some(t) => Ok(json!({"targetInfo": self.target_info(t, true)})),
                    None => Err("No target with given id found".into()),
                })
            }
            _ => {}
        }
        if self.node.control(tab) != Some(true) {
            return Some(Err("The tab is no longer shared with control".into()));
        }
        if !(relay_allowed(method, &params, true) || automation_allowed(method, &params)) {
            return Some(Err(format!("{method} is not permitted in a shared tab")));
        }
        let forward = json!({"type": "cdp", "channel": channel, "tab": tab, "id": id,
            "method": method, "params": params, "automation": true});
        match tokio::time::timeout(Duration::from_secs(5), self.node.outbound.send(forward)).await {
            Ok(Ok(())) => None,
            _ => Some(Err("The browser is busy or gone".into())),
        }
    }
}
