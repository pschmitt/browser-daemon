//! Provision short-lived nodes through operator-provided executables.

use super::*;
use futures_util::{SinkExt, StreamExt};
use std::process::Stdio;

const DEFAULT_NODE_TTL: u64 = 7200;
const MAX_NODE_TTL: u64 = 86400;

fn validate_ttl(ttl: u64) -> Result<u64, ApiError> {
    if !(60..=MAX_NODE_TTL).contains(&ttl) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Node lifetime must be between 1 minute and 24 hours",
        ));
    }
    Ok(ttl)
}

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    id: String,
    url: String,
    #[serde(default)]
    session: Option<Uuid>,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    seen_client: bool,
    #[serde(default)]
    standalone: bool,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    session_created: Option<u64>,
}

/// A provider-owned worker, without exposing its private URL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EphemeralNode {
    pub lease_id: Uuid,
    pub name: String,
    pub session_id: Option<Uuid>,
    pub status: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

pub(crate) async fn nodes(state: &AppState) -> Vec<EphemeralNode> {
    if state.ephemeral_up.is_none() {
        return Vec::new();
    }
    let Ok(mut entries) = fs::read_dir(lease_dir(state)).await else {
        return Vec::new();
    };
    let mut leases = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        if let Ok(data) = fs::read(path).await {
            if let Ok(lease) = serde_json::from_slice::<Lease>(&data) {
                leases.push((id, lease));
            }
        }
    }
    futures_util::future::join_all(leases.into_iter().map(|(id, lease)| async move {
        let info = if lease.url.is_empty() {
            None
        } else {
            match state
                .peer_request(state.http_client.get(format!(
                    "{}/api/mesh/node?without_ephemeral=1",
                    lease.url.trim_end_matches('/')
                )))
                .timeout(Duration::from_millis(1000))
                .send()
                .await
            {
                Ok(res) if res.status().is_success() => res.json::<NodeInfo>().await.ok(),
                _ => None,
            }
        };
        let status = if lease.url.is_empty() || (lease.session.is_none() && !lease.standalone) {
            "starting"
        } else if info.is_some() && lease.session.is_none() {
            "ready"
        } else if info.is_some() {
            "online"
        } else {
            "unreachable"
        };
        EphemeralNode {
            lease_id: id,
            name: info
                .map(|n| n.name)
                .unwrap_or_else(|| format!("ephemeral-{}", &id.to_string()[..8])),
            session_id: lease.session,
            status: status.into(),
            created_at: lease.created,
            expires_at: lease.expires_at,
        }
    }))
    .await
}

fn lease_dir(state: &AppState) -> PathBuf {
    state.config.state_dir().join("ephemeral-nodes")
}

async fn ui_lease(state: &AppState, id: Uuid) -> Option<Lease> {
    let data = fs::read(lease_dir(state).join(format!("{id}.json")))
        .await
        .ok()?;
    let lease: Lease = serde_json::from_slice(&data).ok()?;
    lease.session?;
    Some(lease)
}

fn ui_path_allowed(method: &axum::http::Method, path: &str, session: Uuid) -> bool {
    if !matches!(
        *method,
        axum::http::Method::GET
            | axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    ) {
        return false;
    }
    if path
        .split('/')
        .any(|s| s == ".." || s == "." || s.contains(['\\', '\0']))
        || crate::auth::secret_only(method, &format!("/{path}"))
    {
        return false;
    }
    let session_prefix = format!("api/sessions/{session}");
    if path == session_prefix || path.starts_with(&format!("{session_prefix}/")) {
        return !["/run", "/agent-passkey", "/passkey", "/login", "/autofill"]
            .iter()
            .any(|p| path.contains(p));
    }
    matches!(
        path,
        "json/list" | "json/new" | "api/settings" | "api/proxies" | "api/favicon" | "api/qr"
    ) || path.starts_with("json/close/")
        || path.starts_with("json/activate/")
}

pub(crate) async fn ui_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: axum::http::Method,
    AxumPath((id, path)): AxumPath<(Uuid, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Body,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(lease) = ui_lease(&state, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !ui_path_allowed(&method, &path, lease.session.unwrap_or_default()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut url = format!("{}/{}", lease.url.trim_end_matches('/'), path);
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    let mut req = state
        .peer_request(state.peer_client.request(method, url))
        .timeout(Duration::from_secs(120));
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct.clone());
    }
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range.clone());
    }
    req = req.body(reqwest::Body::wrap_stream(crate::peers::capped_chunks(
        body,
        MAX_UPLOAD_BYTES,
    )));
    match req.send().await {
        Ok(res) => {
            let mut out = Response::builder().status(res.status());
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_DISPOSITION,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
            ] {
                if let Some(v) = res.headers().get(&name) {
                    out = out.header(name, v);
                }
            }
            out.body(axum::body::Body::from_stream(res.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub(crate) async fn ui_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    AxumPath((id, path)): AxumPath<(Uuid, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(lease) = ui_lease(&state, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !(path.starts_with("devtools/page/")
        || matches!(path.as_str(), "firefox" | "video" | "audio" | "viewers"))
        || path.contains("..")
        || path.contains('\\')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let base = lease
        .url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let mut url = format!("{}/{}", base.trim_end_matches('/'), path);
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let Ok(mut request) = url.into_client_request() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if let Some(secret) = &state.cluster_secret {
        let Ok(value) = HeaderValue::from_str(secret) else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        request
            .headers_mut()
            .insert("x-browser-cluster-secret", value);
    }
    // Large CDP messages (files set on an input) pass, as on a node's own CDP route.
    let Ok((upstream, _)) = tokio_tungstenite::connect_async_with_config(
        request,
        Some(crate::cdp_ws::cdp_ws_config()),
        true,
    )
    .await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    // Hold a normal automation connection for the life of the live view. This
    // also supports older worker images where observers do not count as clients.
    let hold_path = if path.starts_with("devtools/page/") {
        Some("chromium")
    } else if path == "firefox" {
        Some("firefox")
    } else {
        None
    };
    let hold = if let Some(hold_path) = hold_path {
        let hold_url = format!(
            "{}/{hold_path}?session_id={}&client=dashboard",
            base.trim_end_matches('/'),
            lease.session.unwrap_or_default()
        );
        let Ok(mut req) = hold_url.into_client_request() else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        if let Some(secret) = &state.cluster_secret {
            let Ok(value) = HeaderValue::from_str(secret) else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            req.headers_mut().insert("x-browser-cluster-secret", value);
        }
        let Ok((socket, _)) = tokio_tungstenite::connect_async(req).await else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        Some(socket)
    } else {
        None
    };
    ws.max_message_size(crate::cdp_ws::CDP_MAX_MESSAGE)
        .max_frame_size(crate::cdp_ws::CDP_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _hold = hold;
            use axum::extract::ws::Message as A;
            use tokio_tungstenite::tungstenite::Message as T;
            let (mut a_tx, mut a_rx) = socket.split();
            let (mut b_tx, mut b_rx) = upstream.split();
            let to_upstream = async {
                while let Some(Ok(msg)) = a_rx.next().await {
                    let msg = match msg {
                        A::Text(v) => T::Text(v.to_string().into()),
                        A::Binary(v) => T::Binary(v),
                        A::Ping(v) => T::Ping(v),
                        A::Pong(v) => T::Pong(v),
                        A::Close(_) => break,
                    };
                    if b_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            };
            let to_client = async {
                while let Some(Ok(msg)) = b_rx.next().await {
                    let msg = match msg {
                        T::Text(v) => A::Text(v.to_string().into()),
                        T::Binary(v) => A::Binary(v),
                        T::Ping(v) => A::Ping(v),
                        T::Pong(v) => A::Pong(v),
                        T::Close(_) => break,
                        T::Frame(_) => continue,
                    };
                    if a_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! { _ = to_upstream => {}, _ = to_client => {} }
        })
}

async fn save(state: &AppState, key: Uuid, lease: &Lease) -> std::io::Result<()> {
    let dir = lease_dir(state);
    fs::create_dir_all(&dir).await?;
    let data = serde_json::to_vec(lease).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{key}.tmp"));
    fs::write(&tmp, data).await?;
    fs::rename(tmp, dir.join(format!("{key}.json"))).await?;
    state.overview.invalidate();
    Ok(())
}

async fn destroy(state: &AppState, key: Uuid, lease: &Lease) {
    let Some(cmd) = &state.ephemeral_down else {
        return;
    };
    loop {
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            tokio::process::Command::new(cmd)
                .arg(&lease.id)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        if matches!(result, Ok(Ok(status)) if status.success()) {
            let _ = fs::remove_file(lease_dir(state).join(format!("{key}.json"))).await;
            state.overview.invalidate();
            return;
        }
        warn!(node = %lease.id, "Ephemeral node teardown failed; retrying");
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn lease_lock(state: &AppState, key: Uuid) -> Arc<Mutex<()>> {
    state
        .ephemeral_locks
        .lock()
        .await
        .entry(key)
        .or_default()
        .clone()
}

async fn load_lease(state: &AppState, key: Uuid) -> Option<Lease> {
    let data = fs::read(lease_dir(state).join(format!("{key}.json")))
        .await
        .ok()?;
    serde_json::from_slice(&data).ok()
}

async fn destroy_owned(state: &AppState, key: Uuid) {
    let lock = lease_lock(state, key).await;
    let guard = lock.lock().await;
    if let Some(lease) = load_lease(state, key).await {
        destroy(state, key, &lease).await;
    }
    drop(guard);
    let mut locks = state.ephemeral_locks.lock().await;
    if Arc::strong_count(&lock) == 2 {
        locks.remove(&key);
    }
}

async fn watch(state: AppState, key: Uuid, initial: Lease) {
    if initial.url.is_empty() || (!initial.standalone && initial.session.is_none()) {
        destroy_owned(&state, key).await;
        return;
    }
    loop {
        let Some(lease) = load_lease(&state, key).await else {
            return;
        };
        if lease.expires_at.is_some_and(|at| clients::now_secs() >= at) {
            break;
        }
        let Some(id) = lease.session else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        let url = format!("{}/api/sessions/{id}", lease.url.trim_end_matches('/'));
        let response = state.peer_request(state.http_client.get(&url)).send().await;
        match response {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => break,
            Ok(response) if response.status().is_success() => {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if body["session"]["client_count"].as_u64().unwrap_or(0) > 0 {
                        if !lease.seen_client {
                            let lock = lease_lock(&state, key).await;
                            let _guard = lock.lock().await;
                            if let Some(mut current) = load_lease(&state, key).await {
                                current.seen_client = true;
                                if let Err(e) = save(&state, key, &current).await {
                                    warn!(error = %e, "Could not save ephemeral node state");
                                }
                            }
                        }
                    } else if !lease.seen_client
                        && clients::now_secs()
                            .saturating_sub(lease.session_created.unwrap_or(lease.created))
                            > 120
                        && state
                            .peer_request(
                                state.peer_client.post(format!("{url}/expire-unattached")),
                            )
                            .timeout(Duration::from_secs(60))
                            .send()
                            .await
                            .is_ok_and(|r| {
                                r.status().is_success() || r.status() == StatusCode::NOT_FOUND
                            })
                    {
                        break;
                    }
                }
            }
            _ => warn!(node = %lease.id, "Cannot inspect ephemeral session; will retry"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    destroy_owned(&state, key).await;
}

pub(crate) async fn recover(state: &AppState) {
    let Ok(mut entries) = fs::read_dir(lease_dir(state)).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(key) = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(data) = fs::read(entry.path()).await {
            if let Ok(lease) = serde_json::from_slice::<Lease>(&data) {
                tokio::spawn(watch(state.clone(), key, lease));
            }
        }
    }
}

async fn provision(
    state: &AppState,
    up: PathBuf,
    ttl: Option<u64>,
    standalone: bool,
) -> Result<(Uuid, Lease), ApiError> {
    let key = Uuid::new_v4();
    let now = clients::now_secs();
    let mut lease = Lease {
        id: key.to_string(),
        url: String::new(),
        session: None,
        created: now,
        seen_client: false,
        standalone,
        expires_at: ttl.map(|seconds| now.saturating_add(seconds)),
        session_created: None,
    };
    save(state, key, &lease).await.map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save ephemeral node lease",
        )
    })?;
    let mut command = tokio::process::Command::new(up);
    command
        .arg(&lease.id)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(ttl) = ttl {
        command.env("BROWSER_DAEMON_EPHEMERAL_TTL_SECONDS", ttl.to_string());
    }
    let output = tokio::time::timeout(
        Duration::from_secs(ttl.unwrap_or(1500).min(1500)),
        command.output(),
    )
    .await;
    let output = match output {
        Ok(Ok(output)) if output.status.success() => output,
        _ => {
            let state = state.clone();
            tokio::spawn(async move {
                destroy_owned(&state, key).await;
            });
            return Err(api_error(
                StatusCode::BAD_GATEWAY,
                "Ephemeral provisioning failed or timed out",
            ));
        }
    };
    let descriptor = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
    let url = descriptor.as_ref().and_then(|d| d["url"].as_str());
    let valid = url.and_then(|s| url::Url::parse(s).ok()).is_some_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    });
    if !valid || lease.expires_at.is_some_and(|at| clients::now_secs() >= at) {
        let state = state.clone();
        tokio::spawn(async move {
            destroy_owned(&state, key).await;
        });
        return Err(api_error(
            StatusCode::BAD_GATEWAY,
            "Invalid or expired ephemeral node descriptor",
        ));
    }
    lease.url = url.unwrap_or_default().to_string();
    if save(state, key, &lease).await.is_err() {
        let state = state.clone();
        tokio::spawn(async move {
            destroy_owned(&state, key).await;
        });
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save ephemeral node lease",
        ));
    }
    Ok((key, lease))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CreateNodeRequest {
    #[serde(default = "default_node_ttl")]
    ttl_seconds: u64,
}

fn default_node_ttl() -> u64 {
    DEFAULT_NODE_TTL
}

pub(crate) async fn create_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateNodeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.check_auth(&headers)?;
    validate_ttl(req.ttl_seconds)?;
    let Some(up) = state.ephemeral_up.clone() else {
        let overview = fetch_mesh_overview(&state).await;
        let peer = overview
            .peers
            .iter()
            .filter(|p| p.status == "online" && p.ephemeral_provider)
            .min_by(|a, b| a.name.cmp(&b.name))
            .ok_or_else(|| {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "No ephemeral node provider is online",
                )
            })?;
        let response = state
            .peer_request(state.peer_client.post(format!(
                "{}/api/ephemeral/nodes",
                peer.url.trim_end_matches('/')
            )))
            .timeout(Duration::from_secs(1650))
            .json(&req)
            .send()
            .await
            .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Ephemeral provider request failed"))?;
        if !response.status().is_success() {
            return Err(api_error(
                response.status(),
                "Ephemeral provider could not create a node",
            ));
        }
        return response.json().await.map(Json).map_err(|_| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "Invalid ephemeral provider response",
            )
        });
    };
    tokio::spawn(async move {
        let (key, lease) = provision(&state, up, Some(req.ttl_seconds), true).await?;
        let node_name = state.node_name.clone();
        let expires_at = lease.expires_at;
        tokio::spawn(watch(state, key, lease));
        Ok(Json(
            serde_json::json!({"lease_id": key, "node_name": node_name, "expires_at": expires_at}),
        ))
    })
    .await
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Ephemeral provisioning task failed",
        )
    })?
}

pub(crate) async fn create_node_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(key): AxumPath<Uuid>,
    Json(mut req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.check_auth(&headers)?;
    if req.profile.is_some() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Saved profiles cannot be placed on ephemeral nodes",
        ));
    }
    req.persistent = false;
    req.ephemeral_ttl_seconds = None;
    tokio::spawn(async move {
        let lock = lease_lock(&state, key).await;
        let guard = lock.lock().await;
        let mut lease = load_lease(&state, key).await.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Ephemeral node not found"))?;
        if !lease.standalone || lease.session.is_some() || lease.url.is_empty() {
            return Err(api_error(StatusCode::CONFLICT, "Ephemeral node is not available for a new session"));
        }
        if lease.expires_at.is_some_and(|at| clients::now_secs() >= at) {
            return Err(api_error(StatusCode::GONE, "Ephemeral node has expired"));
        }
        let response = state.peer_request(state.peer_client.post(format!("{}/api/sessions", lease.url.trim_end_matches('/'))))
            .json(&req).timeout(Duration::from_secs(120)).send().await;
        let session = match response { Ok(r) if r.status().is_success() => r.json::<SessionSummary>().await.ok(), _ => None };
        let Some(session) = session else {
            drop(guard);
            let state = state.clone();
            tokio::spawn(async move { destroy_owned(&state, key).await; });
            return Err(api_error(StatusCode::BAD_GATEWAY, "Could not create session on ephemeral node"));
        };
        lease.session = Some(session.id);
        lease.session_created = Some(clients::now_secs());
        if save(&state, key, &lease).await.is_err() {
            drop(guard);
            let state = state.clone();
            tokio::spawn(async move { destroy_owned(&state, key).await; });
            return Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save ephemeral session lease"));
        }
        Ok(Json(serde_json::json!({"node_url": lease.url, "node_name": state.node_name, "lease_id": key, "session": session})))
    }).await.map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "Ephemeral session task failed"))?
}

pub(crate) async fn delete_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(key): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    state.check_auth(&headers)?;
    let lease = load_lease(&state, key)
        .await
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Ephemeral node not found"))?;
    if lease.url.is_empty() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "Ephemeral node is still starting",
        ));
    }
    tokio::spawn(async move {
        destroy_owned(&state, key).await;
    });
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.check_auth(&headers)?;
    if req.profile.is_some() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Saved profiles cannot be placed on ephemeral nodes",
        ));
    }
    if let Some(ttl) = req.ephemeral_ttl_seconds {
        validate_ttl(ttl)?;
    }
    req.persistent = false;
    let Some(up) = state.ephemeral_up.clone() else {
        let overview = fetch_mesh_overview(&state).await;
        let peer = overview
            .peers
            .iter()
            .filter(|peer| peer.status == "online" && peer.ephemeral_provider)
            .min_by(|a, b| a.name.cmp(&b.name))
            .ok_or_else(|| {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "No ephemeral node provider is online",
                )
            })?;
        let response = state
            .peer_request(state.peer_client.post(format!(
                "{}/api/ephemeral/sessions",
                peer.url.trim_end_matches('/')
            )))
            .timeout(Duration::from_secs(1650))
            .json(&req)
            .send()
            .await
            .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Ephemeral provider request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(api_error(
                status,
                "Ephemeral provider could not create a session",
            ));
        }
        let result = response.json().await.map_err(|_| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "Invalid ephemeral provider response",
            )
        })?;
        return Ok(Json(result));
    };
    // Keep provisioning alive if the HTTP caller disconnects; the unused-session
    // timeout will destroy the resulting node.
    let task = tokio::spawn(async move {
        let (key, mut lease) = provision(&state, up, req.ephemeral_ttl_seconds, false).await?;
        let response = state
            .peer_request(
                state
                    .peer_client
                    .post(format!("{}/api/sessions", lease.url.trim_end_matches('/'))),
            )
            .json(&req)
            .timeout(Duration::from_secs(120))
            .send()
            .await;
        let session = match response {
            Ok(r) if r.status().is_success() => r.json::<SessionSummary>().await.ok(),
            _ => None,
        };
        let Some(session) = session else {
            tokio::spawn(watch(state, key, lease));
            return Err(api_error(
                StatusCode::BAD_GATEWAY,
                "Could not create session on ephemeral node",
            ));
        };
        lease.session = Some(session.id);
        lease.session_created = Some(clients::now_secs());
        if save(&state, key, &lease).await.is_err() {
            tokio::spawn(watch(
                state,
                key,
                Lease {
                    session: None,
                    ..lease
                },
            ));
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save ephemeral session lease",
            ));
        }
        let url = lease.url.clone();
        let node_name = state.node_name.clone();
        tokio::spawn(watch(state, key, lease));
        Ok(Json(
            serde_json::json!({ "node_url": url, "node_name": node_name, "lease_id": key, "session": session }),
        ))
    });
    task.await.map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Ephemeral provisioning task failed",
        )
    })?
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[test]
    fn relay_is_scoped_to_the_lease_and_excludes_secrets() {
        let id = Uuid::new_v4();
        assert!(ui_path_allowed(
            &axum::http::Method::GET,
            &format!("api/sessions/{id}"),
            id
        ));
        assert!(!ui_path_allowed(
            &axum::http::Method::GET,
            &format!("api/sessions/{}", Uuid::new_v4()),
            id
        ));
        assert!(!ui_path_allowed(
            &axum::http::Method::POST,
            &format!("api/sessions/{id}/agent-passkey"),
            id
        ));
        assert!(!ui_path_allowed(
            &axum::http::Method::GET,
            "api/secret-agent/next",
            id
        ));
        assert!(!ui_path_allowed(
            &axum::http::Method::GET,
            "json/../api/peers",
            id
        ));
    }
}
