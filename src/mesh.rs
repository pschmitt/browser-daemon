//! The mesh view: what this node and its peers report.

use super::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeInfo {
    pub name: String,
    pub url: String,
    pub version: String,
    /// Git commit it was built from (absent in builds without one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub uptime_seconds: u64,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub profiles_count: usize,
    pub recordings_count: usize,
    pub storage_used_bytes: u64,
    pub max_storage_bytes: u64,
    #[serde(default)]
    pub supported_engines: Vec<BrowserEngine>,
    #[serde(default)]
    pub proxies: Vec<proxies::ProxyInfo>,
    #[serde(default)]
    pub settings: settings::NodeSettings,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshSession {
    #[serde(flatten)]
    pub session: SessionSummary,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshProfile {
    #[serde(flatten)]
    pub profile: ProfileInfo,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshTarget {
    #[serde(flatten)]
    pub target: DevToolsTarget,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshRecording {
    #[serde(flatten)]
    pub recording: RecordingMeta,
    pub node_name: String,
    pub node_url: String,
    pub is_local: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MeshOverview {
    pub local_node: NodeInfo,
    pub peers: Vec<PeerState>,
    pub sessions: Vec<MeshSession>,
    pub profiles: Vec<MeshProfile>,
    pub targets: Vec<MeshTarget>,
    pub recordings: Vec<MeshRecording>,
    pub total_sessions: usize,
    pub total_capacity: usize,
    pub total_profiles: usize,
    pub total_recordings: usize,
    pub total_nodes: usize,
    pub online_nodes: usize,
    pub total_storage_bytes: u64,
    pub max_storage_bytes: u64,
}

pub(crate) async fn api_mesh_node(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let sessions_count = state.sessions.read().await.len();
    let profiles_count = match fs::read_dir(&state.config.profiles_dir).await {
        Ok(mut dir) => {
            let mut c = 0;
            while let Ok(Some(e)) = dir.next_entry().await {
                if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    c += 1;
                }
            }
            c
        }
        Err(_) => 0,
    };
    let local_recordings = recorder::list_recordings(&state.recorder_config).await;
    let recordings_count = local_recordings.len();
    let storage_used_bytes: u64 = local_recordings.iter().map(|r| r.size_bytes).sum();
    let max_storage_bytes = state.recorder_config.max_storage_bytes;
    let mut supported_engines = vec![BrowserEngine::Chromium];
    if state.config.firefox_bin.is_some() {
        supported_engines.push(BrowserEngine::Firefox);
    }

    Ok(Json(NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        revision: REVISION.map(str::to_string),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: sessions_count,
        max_sessions: state.max_sessions,
        profiles_count,
        recordings_count,
        storage_used_bytes,
        max_storage_bytes,
        supported_engines,
        proxies: state.proxy_infos(),
        settings: state.settings.get(),
    }))
}

/// How long a peer may take to answer one of the overview's requests.
pub(crate) const PEER_POLL_TIMEOUT: Duration = Duration::from_millis(1800);

/// What one peer reported.
pub(crate) struct PeerReport {
    pub(crate) state: PeerState,
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) profiles: Vec<ProfileInfo>,
    pub(crate) targets: Vec<DevToolsTarget>,
    pub(crate) recordings: Vec<RecordingMeta>,
    pub(crate) max_storage_bytes: u64,
}

/// A GET of the overview's from a peer; empty when it cannot be had.
pub(crate) async fn peer_poll<T: serde::de::DeserializeOwned + Default>(
    state: &AppState,
    url: String,
) -> T {
    match state
        .peer_request(state.http_client.get(url))
        .timeout(PEER_POLL_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => T::default(),
    }
}

pub(crate) async fn fetch_peer(state: &AppState, name: String, url: String) -> PeerReport {
    let unreachable = |latency_ms: Option<u64>, error: String| PeerReport {
        state: PeerState {
            name: name.clone(),
            url: url.clone(),
            status: "unreachable".to_string(),
            version: None,
            revision: None,
            latency_ms,
            active_sessions: 0,
            max_sessions: 8,
            profiles_count: 0,
            recordings_count: 0,
            storage_used_bytes: 0,
            uptime_seconds: 0,
            supported_engines: vec![BrowserEngine::Chromium],
            proxies: Vec::new(),
            settings: None,
            error: Some(error),
        },
        name: name.clone(),
        url: url.clone(),
        sessions: Vec::new(),
        profiles: Vec::new(),
        targets: Vec::new(),
        recordings: Vec::new(),
        max_storage_bytes: 0,
    };

    let t0 = Instant::now();
    let node = state
        .peer_request(state.http_client.get(format!("{url}/api/mesh/node")))
        .timeout(PEER_POLL_TIMEOUT)
        .send()
        .await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    let res = match node {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => return unreachable(Some(latency_ms), format!("HTTP {}", res.status())),
        Err(err) => return unreachable(None, err.to_string()),
    };
    let info: Option<NodeInfo> = res.json().await.ok();
    if let Some(ref info) = info {
        learn_peer_name(state, &name, &url, &info.name).await;
    }

    // The rest at once: a slow peer costs one wait, not four.
    let (sessions, profiles, targets, recordings) = tokio::join!(
        peer_poll::<Vec<SessionSummary>>(state, format!("{url}/api/sessions")),
        peer_poll::<Vec<ProfileInfo>>(state, format!("{url}/api/profiles")),
        peer_poll::<Vec<DevToolsTarget>>(state, format!("{url}/json/list")),
        peer_poll::<Vec<RecordingMeta>>(state, format!("{url}/api/recordings")),
    );

    let (peer, name, max_storage_bytes) = match info {
        Some(info) => (
            PeerState {
                name: info.name.clone(),
                url: url.clone(),
                status: "online".to_string(),
                version: Some(info.version),
                revision: info.revision,
                latency_ms: Some(latency_ms),
                active_sessions: info.active_sessions,
                max_sessions: info.max_sessions,
                profiles_count: info.profiles_count,
                recordings_count: info.recordings_count,
                storage_used_bytes: info.storage_used_bytes,
                uptime_seconds: info.uptime_seconds,
                supported_engines: info.supported_engines,
                proxies: info.proxies,
                settings: Some(info.settings),
                error: None,
            },
            info.name,
            info.max_storage_bytes,
        ),
        // Answered, but not as a node: counted online with what is known.
        None => (
            PeerState {
                name: name.clone(),
                url: url.clone(),
                status: "online".to_string(),
                version: None,
                revision: None,
                latency_ms: Some(latency_ms),
                active_sessions: 0,
                max_sessions: 8,
                profiles_count: 0,
                recordings_count: 0,
                storage_used_bytes: 0,
                uptime_seconds: 0,
                supported_engines: vec![BrowserEngine::Chromium],
                proxies: Vec::new(),
                settings: None,
                error: None,
            },
            name,
            0,
        ),
    };
    PeerReport {
        state: peer,
        name,
        url,
        sessions,
        profiles,
        targets,
        recordings,
        max_storage_bytes,
    }
}

pub async fn fetch_mesh_overview(state: &AppState) -> MeshOverview {
    let peers: Vec<(String, String)> = {
        let guard = state.peers.read().await;
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };

    // This node and the peers at the same time.
    let local = async {
        let (targets, profiles, recordings) = tokio::join!(
            get_local_targets(state),
            get_local_profiles(state),
            recorder::list_recordings(&state.recorder_config),
        );
        // The tab counts come from the target list: no second round of requests.
        let sessions = snapshot_sessions(state).await;
        let active = state.active_recordings.read().await.clone();
        let sessions: Vec<SessionSummary> = sessions
            .iter()
            .map(|s| {
                let tabs = targets
                    .iter()
                    .filter(|t| t.session_id == Some(s.id) && t.target_type == "page")
                    .count();
                s.summary(tabs, active.get(&s.id))
            })
            .collect();
        (sessions, profiles, targets, recordings)
    };
    let remote = futures_util::future::join_all(
        peers
            .into_iter()
            .map(|(name, url)| fetch_peer(state, name, url)),
    );
    let ((local_sessions, local_profiles, local_targets, local_recordings), reports) =
        tokio::join!(local, remote);

    let local_storage_used_bytes: u64 = local_recordings.iter().map(|r| r.size_bytes).sum();
    let mut local_supported_engines = vec![BrowserEngine::Chromium];
    if state.config.firefox_bin.is_some() {
        local_supported_engines.push(BrowserEngine::Firefox);
    }
    let local_node = NodeInfo {
        name: state.node_name.clone(),
        url: state.public_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        revision: REVISION.map(str::to_string),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        active_sessions: local_sessions.len(),
        max_sessions: state.max_sessions,
        profiles_count: local_profiles.len(),
        recordings_count: local_recordings.len(),
        storage_used_bytes: local_storage_used_bytes,
        max_storage_bytes: state.recorder_config.max_storage_bytes,
        supported_engines: local_supported_engines,
        proxies: state.proxy_infos(),
        settings: state.settings.get(),
    };

    let mut peer_states = Vec::new();
    let mut all_sessions = Vec::new();
    let mut all_profiles = Vec::new();
    let mut all_targets = Vec::new();
    let mut all_recordings = Vec::new();

    let (node_name, node_url) = (local_node.name.clone(), local_node.url.clone());
    all_sessions.extend(local_sessions.into_iter().map(|session| MeshSession {
        session,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_profiles.extend(local_profiles.into_iter().map(|profile| MeshProfile {
        profile,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_targets.extend(local_targets.into_iter().map(|target| MeshTarget {
        target,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));
    all_recordings.extend(local_recordings.into_iter().map(|recording| MeshRecording {
        recording,
        node_name: node_name.clone(),
        node_url: node_url.clone(),
        is_local: true,
    }));

    let mut total_capacity = local_node.max_sessions;
    let mut online_nodes = 1;
    let mut total_storage_bytes = local_node.storage_used_bytes;
    let mut max_storage_bytes = local_node.max_storage_bytes;

    for report in reports {
        if report.state.status == "online" {
            online_nodes += 1;
            total_capacity += report.state.max_sessions;
            total_storage_bytes += report.state.storage_used_bytes;
            max_storage_bytes += report.max_storage_bytes;
        }
        peer_states.push(report.state);
        let (node_name, node_url) = (report.name, report.url);
        all_sessions.extend(report.sessions.into_iter().map(|session| MeshSession {
            session,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_profiles.extend(report.profiles.into_iter().map(|profile| MeshProfile {
            profile,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_targets.extend(report.targets.into_iter().map(|target| MeshTarget {
            target,
            node_name: node_name.clone(),
            node_url: node_url.clone(),
            is_local: false,
        }));
        all_recordings.extend(
            report
                .recordings
                .into_iter()
                .map(|recording| MeshRecording {
                    recording,
                    node_name: node_name.clone(),
                    node_url: node_url.clone(),
                    is_local: false,
                }),
        );
    }

    all_recordings.sort_by_key(|r| std::cmp::Reverse(r.recording.started_at_secs));

    let total_nodes = 1 + peer_states.len();
    let total_sessions = all_sessions.len();
    let total_profiles = all_profiles.len();
    let total_recordings = all_recordings.len();

    MeshOverview {
        local_node,
        peers: peer_states,
        sessions: all_sessions,
        profiles: all_profiles,
        targets: all_targets,
        recordings: all_recordings,
        total_sessions,
        total_capacity,
        total_profiles,
        total_recordings,
        total_nodes,
        online_nodes,
        total_storage_bytes,
        max_storage_bytes,
    }
}

pub(crate) async fn api_mesh_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    let snapshot = state.overview_snapshot(Duration::from_millis(750)).await;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        snapshot.json.clone(),
    ))
}
