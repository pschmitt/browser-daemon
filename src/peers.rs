//! Peer management and request forwarding.

use super::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerState {
    pub name: String,
    pub url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub latency_ms: Option<u64>,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    #[serde(default)]
    pub recordings_count: usize,
    #[serde(default)]
    pub storage_used_bytes: u64,
    pub uptime_seconds: u64,
    #[serde(default)]
    pub supported_engines: Vec<BrowserEngine>,
    #[serde(default)]
    pub proxies: Vec<proxies::ProxyInfo>,
    /// Its settings (absent when unreachable, or from a node without them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<settings::NodeSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct AddPeerRequest {
    pub url: String,
    pub name: Option<String>,
}

pub(crate) fn infer_peer_name(url_str: &str, self_node: &str) -> Option<String> {
    let trimmed = url_str.trim().trim_end_matches('/');
    if let Ok(parsed) = url::Url::parse(trimmed) {
        if let Some(host) = parsed.host_str() {
            // An address says nothing about the name: the peer reports it.
            if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
                return None;
            }
            if host.starts_with(&format!("browser.{}.", self_node))
                || host == format!("browser.{}", self_node)
            {
                return None;
            }
            if let Some(rest) = host.strip_prefix("browser.") {
                let name = rest.split('.').next().unwrap_or(host);
                if name == self_node {
                    return None;
                }
                return Some(name.to_string());
            }
            let name = host.split('.').next().unwrap_or(host);
            if name == self_node {
                return None;
            }
            return Some(name.to_string());
        }
    }
    None
}

/// A peer's name is guessed from its URL (or is the URL, e.g. for an IP) until
/// it answers: then it goes by the name it reports, so `--node <name>` reaches it.
pub(crate) async fn learn_peer_name(state: &AppState, key: &str, url: &str, reported: &str) {
    if key == reported || reported.is_empty() || reported == state.node_name {
        return;
    }
    let mut peers = state.peers.write().await;
    if let Some(renamed) = rekey_peer(&peers, key, url, reported) {
        peers.remove(key);
        peers.insert(renamed, url.to_string());
    }
}

/// The peer's new key: the name it reports, unless another peer has it.
fn rekey_peer(
    peers: &HashMap<String, String>,
    key: &str,
    url: &str,
    reported: &str,
) -> Option<String> {
    (key != reported
        && peers.get(key).map(String::as_str) == Some(url)
        && !peers.contains_key(reported))
    .then(|| reported.to_string())
}

/// The peer a request names: by its name or its URL, exactly (a name is
/// matched without regard to case). A name that fits several peers fits none.
pub(crate) fn find_peer<'a>(peers: &'a HashMap<String, String>, peer: &str) -> Option<&'a String> {
    let peer = peer.trim();
    if let Some(url) = peers.get(peer) {
        return Some(url);
    }
    let wanted_url = peer.trim_end_matches('/');
    let mut hits = peers.iter().filter(|(name, url)| {
        name.eq_ignore_ascii_case(peer) || url.trim_end_matches('/') == wanted_url
    });
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit.1)
}

pub(crate) async fn get_peer_url(state: &AppState, peer: &str) -> Option<String> {
    find_peer(&*state.peers.read().await, peer).cloned()
}

/// How long a peer may take to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerWait {
    /// Anything that answers from memory or does a little work.
    Short,
    /// Copies, archives, recordings: minutes to hours.
    Long,
}

impl PeerWait {
    pub(crate) fn timeout(self) -> Duration {
        match self {
            Self::Short => Duration::from_secs(120),
            Self::Long => Duration::from_secs(3600),
        }
    }
}

/// Where a request for a peer goes on that peer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PeerTarget {
    /// Path segments below `/api`, or the whole path of a DevTools route.
    pub(crate) segments: Vec<String>,
    pub(crate) devtools: bool,
    pub(crate) wait: PeerWait,
}

/// What `/api/peers/{peer}/{rest}` may reach on a peer: the management routes the
/// dashboard, CLI and TUI use on remote nodes, and the DevTools tab routes. Anything
/// else (the peer's own peers included, which would loop) is not forwarded.
pub(crate) fn peer_target(method: &axum::http::Method, rest: &str) -> Option<PeerTarget> {
    use PeerWait::{Long, Short};
    let segments: Vec<&str> = rest.split('/').collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains(['\\', '\0']))
    {
        return None;
    }
    let (devtools, wait) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["extension", "nodes"]) => (false, Short),
        ("DELETE", ["extension", "nodes", _]) => (false, Short),
        ("POST", ["extension", "nodes", _, "command"]) => (false, Short),
        ("POST", ["sessions"]) => (false, Short),
        ("DELETE", ["sessions", _]) => (false, Short),
        ("GET", ["sessions", _, "screenshot"]) => (false, Short),
        ("POST", ["sessions", _, "save-as-profile"]) => (false, Long),
        (
            "POST",
            ["sessions", _, "record" | "stop-record" | "pause-record" | "resume-record" | "record-audio"
            | "idle-timeout" | "name" | "proxy"],
        ) => (false, Short),
        ("PATCH", ["settings"]) => (false, Short),
        ("POST", ["profiles"]) => (false, Long),
        ("GET", ["profiles", _, "icon"]) => (false, Short),
        ("GET" | "PATCH" | "PUT", ["profiles", _]) => (false, Short),
        ("DELETE", ["profiles", _]) => (false, Long),
        ("POST", ["profiles", _, "clone" | "clear" | "import" | "transfer"]) => (false, Long),
        ("GET", ["profiles", _, "export"]) => (false, Long),
        ("GET" | "POST", ["profiles", _, "backups"]) => (false, Long),
        ("DELETE", ["profiles", _, "backups", _]) => (false, Short),
        ("POST", ["profiles", _, "backups", _, "restore"]) => (false, Long),
        ("GET", ["recordings"]) => (false, Short),
        ("POST", ["recordings", "vacuum" | "merge"]) => (false, Long),
        ("GET" | "DELETE", ["recordings", _]) => (false, Short),
        ("GET", ["recordings", _, "frames" | "audio" | "export.webm"]) => (false, Long),
        ("GET", ["sessions", _, "downloads"]) => (false, Short),
        ("GET", ["sessions", _, "downloads", _]) => (false, Long),
        ("DELETE", ["sessions", _, "downloads", _]) => (false, Short),
        ("GET", ["recordings", _, "favicon"]) => (false, Short),
        ("POST", ["recordings", _, "keep"]) => (false, Short),
        ("GET" | "POST", ["shares"]) => (false, Short),
        ("GET", ["notes"]) => (false, Short),
        ("GET" | "POST", ["notes", _, _]) => (false, Short),
        ("PUT" | "DELETE", ["notes", _, _, _]) => (false, Short),
        ("DELETE", ["shares", _]) => (false, Short),
        ("PUT", ["json", "new"]) => (true, Short),
        ("PUT", ["json", "close" | "activate", _]) => (true, Short),
        _ => return None,
    };
    Some(PeerTarget {
        segments: segments.into_iter().map(str::to_string).collect(),
        devtools,
        wait,
    })
}

/// A request body's chunks, failing once more than `limit` bytes have come.
pub(crate) fn capped_chunks(
    body: axum::body::Body,
    limit: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    use futures_util::StreamExt;
    body.into_data_stream().scan(0usize, move |seen, chunk| {
        let item = match chunk {
            Ok(bytes) => {
                *seen = seen.saturating_add(bytes.len());
                if *seen > limit {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "The body is too large",
                    ))
                } else {
                    Ok(bytes)
                }
            }
            Err(e) => Err(std::io::Error::other(e)),
        };
        std::future::ready(Some(item))
    })
}

/// Forward a request to the peer it names, streaming the body both ways: an
/// import or an export never sits in memory, and the wait is as long as the
/// kind of request needs, not the overview's few seconds.
pub(crate) async fn api_peer_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: axum::http::Method,
    AxumPath((peer, rest)): AxumPath<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Body,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(target) = peer_target(&method, &rest) else {
        return api_error(StatusCode::NOT_FOUND, "Unknown route for a peer").into_response();
    };
    let Some(peer_url) = get_peer_url(&state, &peer).await else {
        return api_error(
            StatusCode::NOT_FOUND,
            format!("Peer '{peer}' not found in mesh"),
        )
        .into_response();
    };
    let Ok(mut url) = url::Url::parse(&peer_url) else {
        return api_error(StatusCode::BAD_GATEWAY, "The peer's URL is invalid").into_response();
    };
    {
        let Ok(mut path) = url.path_segments_mut() else {
            return api_error(StatusCode::BAD_GATEWAY, "The peer's URL is invalid").into_response();
        };
        path.pop_if_empty();
        if !target.devtools {
            path.push("api");
        }
        path.extend(&target.segments);
    }
    url.set_query(query.as_deref().filter(|q| !q.is_empty()));

    let upstream_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = state
        .peer_request(state.peer_client.request(upstream_method, url))
        .timeout(target.wait.timeout());
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range.clone());
    }
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if length.map_or(headers.contains_key(header::TRANSFER_ENCODING), |n| n > 0) {
        if let Some(ct) = headers.get(header::CONTENT_TYPE) {
            req = req.header(header::CONTENT_TYPE, ct.clone());
        }
        if let Some(n) = length {
            req = req.header(header::CONTENT_LENGTH, n);
        }
        req = req.body(reqwest::Body::wrap_stream(capped_chunks(
            body,
            MAX_UPLOAD_BYTES,
        )));
    }

    match req.send().await {
        Ok(res) => {
            let status =
                StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_DISPOSITION,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
                header::CACHE_CONTROL,
                header::ETAG,
                header::LAST_MODIFIED,
                axum::http::HeaderName::from_static("x-page-id"),
            ] {
                if let Some(v) = res.headers().get(name.as_str()) {
                    if let Ok(v) = HeaderValue::from_bytes(v.as_bytes()) {
                        builder = builder.header(name, v);
                    }
                }
            }
            builder
                .body(axum::body::Body::from_stream(res.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => api_error(
            StatusCode::BAD_GATEWAY,
            format!("Peer communication failed: {e}"),
        )
        .into_response(),
    }
}

pub(crate) async fn api_list_peers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let peers_guard = state.peers.read().await;
    let peer_list: Vec<serde_json::Value> = peers_guard
        .iter()
        .map(|(k, v)| {
            serde_json::json!({
                "name": k,
                "url": v,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "local": {
            "name": state.node_name,
            "url": state.public_url,
            "version": env!("CARGO_PKG_VERSION"),
            "revision": REVISION,
            "uptime_seconds": state.start_time.elapsed().as_secs(),
        },
        "peers": peer_list,
    })))
}

pub(crate) async fn api_add_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AddPeerRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let trimmed = req.url.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Peer URL cannot be empty" })),
        )
            .into_response();
    }

    if trimmed == state.public_url.trim_end_matches('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Cannot peer with self" })),
        )
            .into_response();
    }
    // Every request to a peer carries the cluster secret.
    if !relay::peer_url_ok(&trimmed) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Peer URL must be http(s) and not a loopback address" })),
        )
            .into_response();
    }

    let ping_url = format!("{}/api/mesh/node", trimmed);
    let detected_name = match state
        .peer_request(state.http_client.get(&ping_url))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res
            .json::<NodeInfo>()
            .await
            .map(|info| info.name)
            .unwrap_or_else(|_| {
                infer_peer_name(&trimmed, &state.node_name)
                    .unwrap_or_else(|| req.name.clone().unwrap_or_else(|| "peer".to_string()))
            }),
        _ => infer_peer_name(&trimmed, &state.node_name)
            .unwrap_or_else(|| req.name.unwrap_or_else(|| "peer".to_string())),
    };

    if detected_name == state.node_name {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Cannot peer with self" })),
        )
            .into_response();
    }

    {
        let mut guard = state.peers.write().await;
        relay::deny_url(&trimmed);
        guard.insert(detected_name.clone(), trimmed.clone());
    }

    let my_url = state.public_url.clone();
    let my_name = state.node_name.clone();
    let peer_announce_url = format!("{}/api/peers", trimmed);
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _ = state_clone
            .peer_request(state_clone.http_client.post(&peer_announce_url))
            .json(&serde_json::json!({ "url": my_url, "name": my_name }))
            .send()
            .await;
    });

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "status": "ok", "name": detected_name, "url": trimmed })),
    )
        .into_response()
}

pub(crate) async fn api_remove_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let mut guard = state.peers.write().await;
    let removed = guard.remove(&peer).is_some() || {
        let key_to_remove = guard.iter().find_map(|(k, v)| {
            if v == &peer || k.eq_ignore_ascii_case(&peer) {
                Some(k.clone())
            } else {
                None
            }
        });
        if let Some(k) = key_to_remove {
            guard.remove(&k).is_some()
        } else {
            false
        }
    };

    if removed {
        Json(serde_json::json!({ "status": "removed" })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Peer not found" })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_peer_known_by_its_url_takes_the_name_it_reports() {
        let url = "http://127.0.0.1:3952".to_string();
        let mut peers = HashMap::new();
        peers.insert(url.clone(), url.clone());
        assert_eq!(
            super::rekey_peer(&peers, &url, &url, "bravo").as_deref(),
            Some("bravo")
        );
        // A name read from the URL stays; a reported name already taken is not reused.
        peers.insert("bravo".to_string(), "http://elsewhere".to_string());
        assert_eq!(super::rekey_peer(&peers, &url, &url, "bravo"), None);
        // A guessed name gives way to the reported one; a matching one stays.
        let mut named = HashMap::new();
        named.insert("127".to_string(), url.clone());
        assert_eq!(
            super::rekey_peer(&named, "127", &url, "carol").as_deref(),
            Some("carol")
        );
        assert_eq!(super::rekey_peer(&named, "127", &url, "127"), None);
        assert_eq!(
            super::infer_peer_name("http://127.0.0.1:3952", "alpha"),
            None
        );
        assert_eq!(super::infer_peer_name("http://[::1]:3952", "alpha"), None);
        assert_eq!(
            super::infer_peer_name("https://browser.rofl-14.nb.example", "fnuc").as_deref(),
            Some("rofl-14")
        );
    }

    use super::*;

    #[test]
    fn peers_are_matched_exactly() {
        let peers: HashMap<String, String> = [
            ("rofl-13", "https://browser.rofl-13.example"),
            ("rofl-14", "https://browser.rofl-14.example/"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let url = |p: &str| find_peer(&peers, p).map(String::as_str);
        assert_eq!(url("rofl-13"), Some("https://browser.rofl-13.example"));
        assert_eq!(url("ROFL-14"), Some("https://browser.rofl-14.example/"));
        assert_eq!(
            url("https://browser.rofl-14.example"),
            Some("https://browser.rofl-14.example/")
        );
        // Not a part of a name or an address, which used to pick one at random.
        assert_eq!(url("rofl-1"), None);
        assert_eq!(url("rofl"), None);
        assert_eq!(url("browser.rofl-13"), None);
        assert_eq!(url(""), None);
    }

    #[test]
    fn only_known_routes_reach_a_peer() {
        use axum::http::Method;
        let target = |m: Method, p: &str| peer_target(&m, p);
        let t = target(Method::GET, "profiles/Work").unwrap();
        assert_eq!(t.segments, ["profiles", "Work"]);
        assert!(!t.devtools);
        assert_eq!(t.wait, PeerWait::Short);
        assert_eq!(
            target(Method::POST, "profiles/Work/import").unwrap().wait,
            PeerWait::Long
        );
        assert_eq!(
            target(Method::GET, "recordings/abc/export.webm")
                .unwrap()
                .wait,
            PeerWait::Long
        );
        assert_eq!(
            target(Method::GET, "sessions/abc/downloads/g1")
                .unwrap()
                .wait,
            PeerWait::Long
        );
        assert!(target(Method::GET, "sessions/abc/downloads").is_some());
        assert!(target(Method::DELETE, "sessions/abc/downloads/g1").is_some());
        assert!(target(Method::GET, "extension/nodes").is_some());
        assert!(target(Method::POST, "extension/nodes/abc/command").is_some());
        assert!(target(Method::DELETE, "extension/nodes/abc").is_some());
        // A viewer token must never be promoted to a pairing credential by
        // the forwarding node's cluster-secret header.
        assert!(target(Method::POST, "extension/pair").is_none());
        assert!(target(Method::GET, "extension/connect").is_none());
        assert!(target(Method::PUT, "json/new").unwrap().devtools);
        assert!(target(Method::PUT, "json/close/ABC").unwrap().devtools);
        // The method matters.
        assert!(target(Method::GET, "json/new").is_none());
        assert!(target(Method::DELETE, "settings").is_none());
        // Nothing else of the peer is reachable, nor a way out of the route.
        assert!(target(Method::POST, "peers/rofl-14/sessions").is_none());
        assert!(target(Method::GET, "mesh/overview").is_none());
        assert!(target(Method::GET, "profiles/../settings").is_none());
        assert!(target(Method::GET, "profiles/./x").is_none());
        assert!(target(Method::GET, "profiles//x").is_none());
        assert!(target(Method::GET, "profiles/").is_none());
        assert!(target(Method::GET, "profiles/a\\b").is_none());
        assert!(target(Method::POST, "sessions/abc/passkey").is_none());
    }

    #[tokio::test]
    async fn forwarded_bodies_are_capped() {
        use futures_util::StreamExt;
        let body = |chunks: Vec<&'static [u8]>| {
            axum::body::Body::from_stream(futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|c| Ok::<_, std::io::Error>(Bytes::from_static(c))),
            ))
        };
        let ok: Vec<_> = capped_chunks(body(vec![b"abc", b"de"]), 5).collect().await;
        assert!(ok.iter().all(Result::is_ok));
        let over: Vec<_> = capped_chunks(body(vec![b"abc", b"def"]), 5).collect().await;
        assert!(over[0].is_ok());
        assert!(over[1].is_err());
    }
}
