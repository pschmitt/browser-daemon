//! The shared mesh overview and its WebSocket feed.

use super::*;

/// The latest mesh overview, as the JSON everybody is sent.
pub struct OverviewSnapshot {
    pub(crate) at: Instant,
    pub(crate) json: String,
}

#[derive(Default)]
pub struct OverviewHub {
    pub(crate) latest: tokio::sync::watch::Sender<Option<Arc<OverviewSnapshot>>>,
    /// One computation at a time; the ones waiting reuse its result.
    pub(crate) compute: Mutex<()>,
    /// Something changed since the latest one: the next reader computes anew, so
    /// `session new` followed by `session open` finds the session.
    pub(crate) stale: AtomicBool,
}

impl OverviewHub {
    pub(crate) fn invalidate(&self) {
        self.stale.store(true, Ordering::Relaxed);
    }
}

/// Marks the overview stale after every request that may change something.
pub(crate) async fn invalidate_after_changes(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let changes = !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let res = next.run(req).await;
    if changes {
        state.overview.invalidate();
    }
    res
}

impl AppState {
    /// The mesh overview, at most `max_age` old: dashboards, the CLI and the
    /// TUI share one computation instead of each walking every node.
    pub async fn overview_snapshot(&self, max_age: Duration) -> Arc<OverviewSnapshot> {
        let recent = |hub: &OverviewHub| {
            hub.latest
                .borrow()
                .clone()
                .filter(|s| !hub.stale.load(Ordering::Relaxed) && s.at.elapsed() <= max_age)
        };
        if let Some(s) = recent(&self.overview) {
            return s;
        }
        let _computing = self.overview.compute.lock().await;
        if let Some(s) = recent(&self.overview) {
            return s;
        }
        // Changes from here on are not in this one: they mark it stale again.
        self.overview.stale.store(false, Ordering::Relaxed);
        let at = Instant::now();
        let overview = fetch_mesh_overview(self).await;
        let snapshot = Arc::new(OverviewSnapshot {
            at,
            json: serde_json::to_string(&overview).unwrap_or_default(),
        });
        // Whoever listens (the overview sockets) is woken.
        self.overview.latest.send_replace(Some(snapshot.clone()));
        snapshot
    }
}

pub(crate) async fn handle_overview_ws(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }

    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        // Listen first, then send the current one: nothing falls in between.
        let mut updates = state.overview.latest.subscribe();
        let snapshot = state.overview_snapshot(Duration::from_secs(2)).await;
        updates.borrow_and_update();
        if socket
            .send(Message::Text(snapshot.json.clone().into()))
            .await
            .is_err()
        {
            return;
        }

        loop {
            tokio::select! {
                // The overview ticker publishes a new one every two seconds for as
                // long as somebody listens: one computation for all of them.
                changed = updates.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let Some(snapshot) = updates.borrow_and_update().clone() else {
                        continue;
                    };
                    if socket.send(Message::Text(snapshot.json.clone().into())).await.is_err() {
                        break;
                    }
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Text(txt))) => {
                            if txt.as_str() == "refresh" {
                                let snapshot = state.overview_snapshot(Duration::from_millis(500)).await;
                                updates.borrow_and_update();
                                let _ = socket.send(Message::Text(snapshot.json.clone().into())).await;
                            } else if txt.as_str() == "ping" {
                                let _ = socket.send(Message::Text("pong".into())).await;
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if socket.send(Message::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    })
}
