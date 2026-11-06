//! Profile export, import and transfer between nodes.

use super::*;

#[path = "profile_archive.rs"]
mod archive;

#[derive(Deserialize, Debug, Default)]
pub struct ImportProfileParams {
    pub overwrite: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TransferProfileRequest {
    pub target_node: String,
    #[serde(default)]
    pub target_name: Option<String>,
    #[serde(default)]
    pub delete_source: bool,
    #[serde(default)]
    pub overwrite: bool,
}

/// What a child process wrote to stderr (a little of it), collected while it runs: a
/// full pipe would stop the child.
pub(crate) fn drain_stderr(
    stderr: Option<tokio::process::ChildStderr>,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        const KEEP: usize = 8192;
        let Some(mut stderr) = stderr else {
            return String::new();
        };
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = KEEP.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        String::from_utf8_lossy(&kept).trim().to_string()
    })
}

/// The profile directory as a gzipped tar, produced while it is read: the archive
/// never sits in memory. A failing tar ends the stream with an error, so the
/// receiver sees a broken archive rather than a short, valid one.
pub(crate) fn tar_export(
    dir: &Path,
) -> std::io::Result<impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
    let mut child = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .arg(".")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("tar has no output"))?;
    let errors = drain_stderr(child.stderr.take());
    Ok(futures_util::stream::unfold(
        Some((child, stdout, errors)),
        |state| async move {
            use tokio::io::AsyncReadExt;
            let (mut child, mut stdout, errors) = state?;
            let mut buf = vec![0u8; 64 * 1024];
            match stdout.read(&mut buf).await {
                Ok(0) => match child.wait().await {
                    Ok(status) if status.success() => None,
                    other => {
                        let why = errors.await.unwrap_or_default();
                        error!(error = %why, status = ?other, "tar export failed");
                        Some((
                            Err(std::io::Error::other(format!("tar failed: {why}"))),
                            None,
                        ))
                    }
                },
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), Some((child, stdout, errors))))
                }
                Err(e) => Some((Err(e), None)),
            }
        },
    ))
}

pub(crate) async fn api_export_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let archive = {
        // Checked and started under the profile's lock: no session starts in between.
        let _lock = state.profile_locks.lock(profile.meta.id).await;
        ensure_profile_idle(&state, &profile, "export").await?;
        tar_export(&profile.dir).map_err(|e| {
            error!(profile = %profile.meta.name, error = %e, "Failed to run tar command for profile export");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to archive profile: {e}"),
            )
        })?
    };
    // Header values are ASCII: keep a readable file name, fall back to the id.
    let file_stem: String = profile
        .meta
        .name
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' || *c == ' ')
        .collect();
    let file_stem = match file_stem.trim() {
        "" => profile.meta.id.to_string(),
        s => s.to_string(),
    };
    let cd_value = format!("attachment; filename=\"{}.tar.gz\"", file_stem);
    let cd_header =
        HeaderValue::from_str(&cd_value).unwrap_or_else(|_| HeaderValue::from_static("attachment"));

    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/gzip"),
            ),
            (header::CONTENT_DISPOSITION, cd_header),
        ],
        axum::body::Body::from_stream(archive),
    ))
}

/// Archives being extracted wait in a hidden directory (`.import-<id>`, which no
/// profile listing shows) until they are complete.
pub(crate) const IMPORT_PREFIX: &str = ".import-";

/// A directory removed when this goes, also when the request is dropped halfway
/// (the client left).
pub(crate) struct StagingDir(Option<PathBuf>);

impl StagingDir {
    pub(crate) fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    /// It was moved into place: nothing to clean up.
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let Some(dir) = self.0.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    let _ = fs::remove_dir_all(dir).await;
                });
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// Validate and unpack a bounded gzipped tar upload into `dir`. Returns the
/// compressed bytes received.
pub(crate) async fn extract_archive(body: axum::body::Body, dir: &Path) -> Result<u64, ApiError> {
    archive::extract(body, dir)
        .await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, format!("tar import failed: {e}")))
}

/// Import an archive as the profile named in the path. The archive's own
/// `profile.json` supplies the engine; archives from the old name-based
/// layout fall back to the `firefox_` naming convention. The archive is
/// unpacked as it arrives, into a hidden directory that becomes the profile once
/// it is complete.
pub(crate) async fn api_import_profile(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ImportProfileParams>,
    body: axum::body::Body,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&name)?;
    let overwrite = params.overwrite.unwrap_or(false);
    let exists = |name: &str| {
        let dir = state.config.profiles_dir.clone();
        let name = name.to_string();
        async move {
            profiles::find(&dir, &name)
                .await
                .filter(|p| p.meta.name == name)
        }
    };
    // Before the upload is read: a profile that is in the way is no reason to wait for it.
    if !overwrite && exists(&name).await.is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile '{name}' already exists on target"),
        ));
    }

    let mut staging = StagingDir(Some(
        state
            .config
            .profiles_dir
            .join(format!("{IMPORT_PREFIX}{}", Uuid::new_v4())),
    ));
    if let Err(e) = fs::create_dir_all(staging.path()).await {
        error!(profile = %name, error = %e, "Failed to create profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create profile directory",
        ));
    }
    extract_archive(body, staging.path()).await?;

    let (name, engine) = match profiles::read_meta(staging.path()).await {
        Some(meta) => (name, meta.engine),
        None => profiles::legacy_identity(&name),
    };
    let _guard = state.profiles_lock.lock().await;
    let existing = exists(&name).await;
    let _existing_lock = match existing {
        Some(ref e) => Some(state.profile_locks.lock(e.meta.id).await),
        None => None,
    };
    if let Some(ref existing) = existing {
        if !overwrite {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("Profile '{name}' already exists on target"),
            ));
        }
        if profile_use(&state, existing.meta.id).await.is_some() {
            return Err(api_error(
                StatusCode::CONFLICT,
                "Cannot overwrite active profile (in use by a session)",
            ));
        }
    }

    let target_dir = state.config.profiles_dir.join(Uuid::new_v4().to_string());
    fs::rename(staging.path(), &target_dir).await.map_err(|e| {
        error!(profile = %name, error = %e, "Failed to move the imported profile into place");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create profile directory",
        )
    })?;
    staging.disarm();
    let profile = match profiles::adopt(target_dir.clone(), &name, engine).await {
        Ok(p) => p,
        Err(e) => {
            error!(profile = %name, error = %e, "Failed to write profile metadata");
            let _ = fs::remove_dir_all(&target_dir).await;
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write profile metadata",
            ));
        }
    };
    // Replace the old copy only once the new one is complete.
    if let Some(existing) = existing {
        state.dir_sizes.forget(&existing.dir);
        if let Err(e) = fs::remove_dir_all(&existing.dir).await {
            warn!(profile = %name, error = %e, "Imported profile, but failed to remove the overwritten copy");
        }
    }

    let info = profile_info(&state, &profile).await;
    info!(profile = %name, id = %profile.meta.id, size_bytes = %info.size_bytes, "Profile imported successfully");
    Ok((StatusCode::CREATED, Json(info)))
}

pub(crate) async fn api_transfer_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TransferProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let source = lookup_profile(&state, &key).await?;

    let target_node = req.target_node.trim();
    if target_node.is_empty() || target_node == state.node_name {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Target node must be a different node in the cluster",
        ));
    }

    let target_name = req
        .target_name
        .as_deref()
        .and_then(profiles::clean_name)
        .unwrap_or_else(|| source.meta.name.clone());

    let target_peer_url = get_peer_url(&state, target_node).await.ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Target node '{}' not found in mesh", target_node),
        )
    })?;

    // No session starts on the profile while it is copied (and perhaps removed).
    let _lock = state.profile_locks.lock(source.meta.id).await;
    ensure_profile_idle(&state, &source, "export").await?;

    // 1. Archive the local profile as a stream (profile.json carries the engine)
    let archive = tar_export(&source.dir).map_err(|e| {
        error!(profile = %source.meta.name, error = %e, "Failed to run tar command for profile export");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to archive profile: {e}"),
        )
    })?;

    // 2. Pass it on to the target node's /api/profiles/{target_name}/import as it is produced
    let mut import_url = peer_profile_url(&target_peer_url, &target_name, Some("import"));
    if req.overwrite {
        import_url.push_str("?overwrite=true");
    }

    let upload_res = state
        .peer_request(state.peer_client.post(&import_url))
        .timeout(PeerWait::Long.timeout())
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(reqwest::Body::wrap_stream(archive))
        .send()
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to upload profile archive to target node");
            api_error(
                StatusCode::BAD_GATEWAY,
                format!("Failed to send profile to target node: {}", e),
            )
        })?;

    if !upload_res.status().is_success() {
        let status =
            StatusCode::from_u16(upload_res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let error_json = upload_res.json::<serde_json::Value>().await.unwrap_or_else(
            |_| serde_json::json!({ "error": "Target node rejected profile import" }),
        );
        return Err((status, Json(error_json)));
    }

    // 3. If delete_source was requested, remove the local directory (unless a session
    //    got hold of the profile meanwhile: it has newer data than what was sent)
    let mut deleted_source = false;
    if req.delete_source {
        if profile_use(&state, source.meta.id).await.is_some() {
            warn!(profile = %source.meta.name, "Profile transferred, but a session uses it now: source kept");
        } else {
            state.dir_sizes.forget(&source.dir);
            if let Err(e) = fs::remove_dir_all(&source.dir).await {
                warn!(profile = %source.meta.name, error = %e, "Profile transferred, but failed to delete source directory");
            } else {
                info!(profile = %source.meta.name, "Source profile deleted after transfer");
                deleted_source = true;
            }
        }
    }

    info!(
        source = %source.meta.name,
        target_node = %target_node,
        target_name = %target_name,
        "Profile transferred successfully"
    );

    Ok(Json(serde_json::json!({
        "status": "transferred",
        "source_node": state.node_name,
        "target_node": target_node,
        "source_name": source.meta.name,
        "source_id": source.meta.id,
        "target_name": target_name,
        "deleted_source": deleted_source,
    })))
}

#[derive(Deserialize)]
pub(crate) struct PlacementRequest {
    pub profile: Option<String>,
    pub node: Option<String>,
    pub group: Option<String>,
    pub engine: Option<BrowserEngine>,
    #[serde(default)]
    pub create: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Placement {
    pub node: String,
    pub node_url: String,
    pub profile: Option<String>,
}

fn select_placement(ov: &MeshOverview, req: &PlacementRequest) -> Result<Placement, ApiError> {
    let profile = if let Some(name) = req.profile.as_deref() {
        // A node that answers with a partial list cannot establish which copy is authoritative.
        if ov.peers.iter().any(|p| {
            p.status == "online"
                && ov.profiles.iter().filter(|m| m.node_name == p.name).count() < p.profiles_count
        }) {
            return Err(api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Profile inventory is incomplete; retry when all nodes are reachable",
            ));
        }
        let hits: Vec<_> = ov
            .profiles
            .iter()
            .filter(|p| p.profile.name == name)
            .collect();
        let unreachable: Vec<&str> = ov
            .peers
            .iter()
            .filter(|p| p.status != "online")
            .map(|p| p.name.as_str())
            .collect();
        match hits.as_slice() {
            [profile] => Some(*profile),
            // The profile may be on a node that is down, for example restarting. Creating it
            // now would lose its logins to a new, empty copy.
            [] if !unreachable.is_empty() => {
                return Err(api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "Profile not found on the reachable nodes; retry when these are \
                         reachable: {}",
                        unreachable.join(", ")
                    ),
                ))
            }
            [] if req.create => None,
            [] => {
                return Err(api_error(
                    StatusCode::NOT_FOUND,
                    "Profile not found in mesh",
                ))
            }
            _ => {
                return Err(api_error(
                    StatusCode::CONFLICT,
                    "Profile exists on several nodes; resolve duplicate copies before placement",
                ))
            }
        }
    } else {
        None
    };
    let engine = profile
        .map(|p| p.profile.engine)
        .or(req.engine)
        .unwrap_or_default();
    if req.engine.is_some_and(|e| e != engine) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Requested engine differs from the profile's engine",
        ));
    }
    let candidates = std::iter::once((
        &ov.local_node.name,
        &ov.local_node.url,
        ov.local_node.active_sessions,
        ov.local_node.max_sessions,
        &ov.local_node.groups,
        &ov.local_node.supported_engines,
    ))
    .chain(ov.peers.iter().filter(|p| p.status == "online").map(|p| {
        (
            &p.name,
            &p.url,
            p.active_sessions,
            p.max_sessions,
            &p.groups,
            &p.supported_engines,
        )
    }));
    let target = candidates
        .filter(|(name, _, _, _, groups, engines)| {
            req.node.as_ref().is_none_or(|n| n == *name)
                && req.group.as_ref().is_none_or(|g| groups.contains(g))
                && engines.contains(&engine)
        })
        .filter(|(name, _, active, max, _, _)| {
            active < max
                || profile.is_some_and(|p| &p.node_name == *name && p.profile.session_id.is_some())
        })
        .min_by(|a, b| {
            let resident = |name: &str| profile.is_some_and(|p| p.node_name == name);
            resident(b.0)
                .cmp(&resident(a.0))
                .then_with(|| {
                    (a.2 as f64 / a.3.max(1) as f64).total_cmp(&(b.2 as f64 / b.3.max(1) as f64))
                })
                .then_with(|| a.0.cmp(b.0))
        })
        .ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "No online node with capacity and the requested engine matches the node/group",
            )
        })?;
    Ok(Placement {
        node: target.0.clone(),
        node_url: target.1.clone(),
        profile: req.profile.clone(),
    })
}

pub(crate) async fn api_place_browser(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<PlacementRequest>,
) -> Result<Json<Placement>, ApiError> {
    state.check_auth(&headers)?;
    for text in [&mut req.profile, &mut req.node, &mut req.group]
        .into_iter()
        .flatten()
    {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "Placement selectors must not be empty",
            ));
        }
        *text = trimmed.to_string();
    }
    let ov = fetch_mesh_overview(&state).await;
    let placement = select_placement(&ov, &req)?;
    if let Some(name) = req.profile.as_deref() {
        let Some(source) = ov.profiles.iter().find(|p| p.profile.name == name) else {
            return Ok(Json(placement));
        };
        if source.node_name != placement.node {
            let transfer = TransferProfileRequest {
                target_node: placement.node.clone(),
                target_name: None,
                delete_source: true,
                overwrite: false,
            };
            let result = if source.is_local {
                let response = api_transfer_profile(
                    AxumPath(
                        source
                            .profile
                            .id
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| source.profile.name.clone()),
                    ),
                    State(state.clone()),
                    headers,
                    Json(transfer),
                )
                .await?
                .into_response();
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Invalid transfer response"))?;
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Invalid transfer response"))?
            } else {
                let url = peer_profile_url(
                    &source.node_url,
                    &source
                        .profile
                        .id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| source.profile.name.clone()),
                    Some("transfer"),
                );
                let response = state.peer_request(state.peer_client.post(url))
                    .timeout(PeerWait::Long.timeout()).json(&transfer).send().await
                    .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Profile transfer request failed; inspect source and destination before retrying"))?;
                let status = response.status();
                if !status.is_success() {
                    return Err(api_error(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), "Profile transfer refused; the source may be active or the destination may already exist"));
                }
                response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Invalid transfer response"))?
            };
            if result["deleted_source"] != true {
                return Err(api_error(StatusCode::CONFLICT, "Transfer did not remove the source; resolve duplicate copies before opening the destination"));
            }
        }
    }
    Ok(Json(placement))
}

// ---------------------------------------------------------------------------
// Peer-to-Peer Mesh Handlers & Proxy APIs
// ---------------------------------------------------------------------------

pub(crate) async fn copy_dir_all(src: PathBuf, dst: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        fn copy_all(src: &Path, dst: &Path) -> std::io::Result<()> {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                let dest_path = dst.join(entry.file_name());
                if file_type.is_dir() {
                    copy_all(&entry.path(), &dest_path)?;
                } else if file_type.is_file() {
                    std::fs::copy(entry.path(), dest_path)?;
                }
            }
            Ok(())
        }
        copy_all(&src, &dst)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// `{peer}/api/profiles/{name}[/{action}]`, the name encoded as one path segment.
pub(crate) fn peer_profile_url(peer_url: &str, name: &str, action: Option<&str>) -> String {
    let mut url = match url::Url::parse(peer_url) {
        Ok(url) => url,
        Err(_) => {
            let action = action.map(|a| format!("/{a}")).unwrap_or_default();
            return format!("{}/api/profiles/{}{}", peer_url, name, action);
        }
    };
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.pop_if_empty().extend(["api", "profiles", name]);
        if let Some(action) = action {
            segments.push(action);
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement_overview() -> MeshOverview {
        let node = |name: &str, groups: &[&str]| {
            serde_json::json!({
                "name": name, "url": format!("https://{name}.example.com"), "groups": groups,
                "version": "0", "uptime_seconds": 0, "active_sessions": 0, "max_sessions": 4,
                "profiles_count": 0, "recordings_count": 0, "storage_used_bytes": 0,
                "max_storage_bytes": 0, "status": "online", "latency_ms": null,
                "supported_engines": ["chromium"]
            })
        };
        serde_json::from_value(serde_json::json!({
            "local_node": node("home", &["home"]), "peers": [node("worker", &["cloud"])],
            "sessions": [], "profiles": [], "targets": [], "recordings": [],
            "total_sessions": 0, "total_capacity": 8, "total_profiles": 0,
            "total_recordings": 0, "total_nodes": 2, "online_nodes": 2,
            "total_storage_bytes": 0, "max_storage_bytes": 0
        }))
        .unwrap()
    }

    fn placement_request(
        profile: Option<&str>,
        node: Option<&str>,
        group: Option<&str>,
    ) -> PlacementRequest {
        PlacementRequest {
            profile: profile.map(str::to_string),
            node: node.map(str::to_string),
            group: group.map(str::to_string),
            engine: None,
            create: false,
        }
    }

    fn add_profile(ov: &mut MeshOverview, node: &str, active: bool) {
        ov.profiles.push(
            serde_json::from_value(serde_json::json!({
                "name": "demo", "engine": "chromium", "size_bytes": 0, "is_active": active,
                "session_id": active.then_some("00000000-0000-0000-0000-000000000001"),
                "created_at_secs": null, "modified_at_secs": null,
                "node_name": node, "node_url": format!("https://{node}.example.com"),
                "is_local": node == "home"
            }))
            .unwrap(),
        );
    }

    #[test]
    fn groups_never_fall_back_to_an_unmatched_node() {
        let mut ov = placement_overview();
        let req = placement_request(None, None, Some("cloud"));
        assert_eq!(select_placement(&ov, &req).unwrap().node, "worker");
        ov.peers[0].active_sessions = 4;
        assert!(select_placement(&ov, &req).is_err());
        ov.peers[0].active_sessions = 0;
        ov.peers[0].status = "unreachable".into();
        assert!(select_placement(&ov, &req).is_err());
        assert!(select_placement(
            &placement_overview(),
            &placement_request(None, Some("home"), Some("cloud"))
        )
        .is_err());
    }

    #[test]
    fn resident_profiles_are_preferred_and_can_reuse_full_nodes() {
        let mut ov = placement_overview();
        add_profile(&mut ov, "worker", true);
        ov.peers[0].active_sessions = 4;
        assert_eq!(
            select_placement(&ov, &placement_request(Some("demo"), None, None))
                .unwrap()
                .node,
            "worker"
        );
        assert_eq!(
            select_placement(&ov, &placement_request(Some("demo"), Some("home"), None))
                .unwrap()
                .node,
            "home"
        );
    }

    #[test]
    fn ambiguous_missing_and_incomplete_profiles_are_refused() {
        let mut ov = placement_overview();
        let req = placement_request(Some("demo"), Some("worker"), None);
        assert!(select_placement(&ov, &req).is_err());
        add_profile(&mut ov, "home", false);
        assert_eq!(select_placement(&ov, &req).unwrap().node, "worker");
        ov.peers[0].profiles_count = 1;
        assert!(select_placement(&ov, &req).is_err());
        ov.peers[0].profiles_count = 0;
        add_profile(&mut ov, "worker", false);
        assert!(select_placement(&ov, &req).is_err());
    }

    #[test]
    fn new_profiles_require_explicit_creation_and_complete_inventory() {
        let mut ov = placement_overview();
        let mut req = placement_request(Some("new-profile"), None, Some("cloud"));
        assert!(select_placement(&ov, &req).is_err());
        req.create = true;
        let placed = select_placement(&ov, &req).unwrap();
        assert_eq!(placed.node, "worker");
        assert_eq!(placed.profile.as_deref(), Some("new-profile"));
        ov.peers[0].status = "unreachable".into();
        assert!(select_placement(&ov, &req).is_err());
    }

    #[test]
    fn an_unreachable_node_does_not_block_a_profile_found_elsewhere() {
        let mut ov = placement_overview();
        let mut down = ov.peers[0].clone();
        down.name = "down".into();
        down.groups = vec!["cloud".into()];
        down.status = "unreachable".into();
        ov.peers.push(down);
        ov.peers[0].profiles_count = 1;
        add_profile(&mut ov, "worker", false);
        let req = placement_request(Some("demo"), None, Some("cloud"));
        assert_eq!(select_placement(&ov, &req).unwrap().node, "worker");
    }

    #[test]
    fn an_unreachable_node_may_hold_a_profile_not_found_elsewhere() {
        let mut ov = placement_overview();
        ov.peers[0].status = "unreachable".into();
        let mut req = placement_request(Some("demo"), None, None);
        req.create = true;
        let err = select_placement(&ov, &req).err().unwrap();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        // Not a 404 either: it may exist there.
        req.create = false;
        let err = select_placement(&ov, &req).err().unwrap();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_node_answering_with_a_partial_list_still_blocks() {
        let mut ov = placement_overview();
        ov.peers[0].profiles_count = 1;
        let mut req = placement_request(Some("demo"), None, None);
        req.create = true;
        assert!(select_placement(&ov, &req).is_err());
    }

    #[test]
    fn placement_checks_engine_support() {
        let ov = placement_overview();
        let mut req = placement_request(None, None, Some("cloud"));
        req.engine = Some(BrowserEngine::Firefox);
        assert!(select_placement(&ov, &req).is_err());
    }

    #[test]
    fn peer_profile_urls_encode_the_name() {
        assert_eq!(
            peer_profile_url(
                "https://browser.h.ts.example/",
                "Google (p@x.dev)",
                Some("import")
            ),
            "https://browser.h.ts.example/api/profiles/Google%20(p@x.dev)/import"
        );
        assert_eq!(
            peer_profile_url("https://browser.h.ts.example", "a b", None),
            "https://browser.h.ts.example/api/profiles/a%20b"
        );
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bd-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_profile_survives_the_trip_through_export_and_import() {
        use futures_util::StreamExt;
        let (src, dst) = (scratch_dir("src"), scratch_dir("dst"));
        std::fs::create_dir_all(src.join("Default")).unwrap();
        std::fs::write(src.join("profile.json"), b"{}").unwrap();
        // Bigger than the pipes between us and tar, and not compressible.
        let mut big = Vec::new();
        let mut x = 0x2545_f491_u32;
        for _ in 0..(6 * 1024 * 1024) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            big.push(x as u8);
        }
        std::fs::write(src.join("Default/Cookies"), &big).unwrap();

        let archive = tar_export(&src).unwrap();
        let received = extract_archive(axum::body::Body::from_stream(archive), &dst)
            .await
            .unwrap();
        assert!(received > 6 * 1024 * 1024);
        assert_eq!(std::fs::read(dst.join("Default/Cookies")).unwrap(), big);
        assert_eq!(std::fs::read(dst.join("profile.json")).unwrap(), b"{}");

        // An export of something that is not there fails at the end of the stream.
        let missing = tar_export(&src.join("nope")).unwrap();
        let chunks: Vec<_> = missing.collect().await;
        assert!(chunks.last().unwrap().is_err());
        std::fs::remove_dir_all(src).unwrap();
        std::fs::remove_dir_all(dst).unwrap();
    }

    #[tokio::test]
    async fn a_broken_or_empty_archive_is_refused_without_stalling() {
        let dst = scratch_dir("bad");
        let body = |bytes: Vec<u8>| {
            axum::body::Body::from_stream(futures_util::stream::iter(
                bytes
                    .chunks(64 * 1024)
                    .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
                    .collect::<Vec<_>>(),
            ))
        };
        // Much more than a pipe holds: tar quits at the first bytes.
        let err = extract_archive(body(vec![b'x'; 4 * 1024 * 1024]), &dst)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1 .0["error"].as_str().unwrap().contains("tar"));
        let err = extract_archive(body(Vec::new()), &dst).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(dst).unwrap();
    }

    #[tokio::test]
    async fn a_staging_directory_goes_when_the_request_does() {
        let dir = scratch_dir("stage");
        let staging = StagingDir(Some(dir.clone()));
        drop(staging);
        for _ in 0..50 {
            if !dir.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the staging directory is still there");
    }
}
