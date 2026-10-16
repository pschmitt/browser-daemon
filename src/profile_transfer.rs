//! Profile export, import and transfer between nodes.

use super::*;

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

/// Unpack a gzipped tar from a request body into `dir`, as it arrives. Returns the
/// bytes received.
pub(crate) async fn extract_archive(body: axum::body::Body, dir: &Path) -> Result<u64, ApiError> {
    use futures_util::StreamExt;
    let mut child = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg("-")
        .arg("--no-same-owner")
        .arg("-C")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            error!(error = %e, "Failed to spawn tar process for import");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn tar: {e}"),
            )
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| api_error(StatusCode::INTERNAL_SERVER_ERROR, "tar has no input"))?;
    // Read while tar runs: it blocks on a full stderr pipe, and we on its stdin.
    let errors = drain_stderr(child.stderr.take());

    let mut chunks = Box::pin(capped_chunks(body, MAX_UPLOAD_BYTES));
    let mut received = 0u64;
    let mut write_failed = false;
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(stdin);
                let _ = child.kill().await;
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    format!("Upload failed: {e}"),
                ));
            }
        };
        received += chunk.len() as u64;
        // A failing write means tar gave up (a broken archive): its message says why.
        if stdin.write_all(&chunk).await.is_err() {
            write_failed = true;
            break;
        }
    }
    drop(stdin);
    if received == 0 {
        let _ = child.kill().await;
        return Err(api_error(StatusCode::BAD_REQUEST, "Empty archive body"));
    }
    let status = child.wait().await.map_err(|e| {
        error!(error = %e, "Failed to wait on tar process");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tar extraction error: {e}"),
        )
    })?;
    if write_failed || !status.success() {
        let why = errors.await.unwrap_or_default();
        error!(error = %why, "tar extraction failed");
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("tar extraction failed: {why}"),
        ));
    }
    Ok(received)
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
