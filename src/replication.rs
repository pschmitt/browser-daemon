//! Profile replication within a pool.
//!
//! A profile whose `pool` names a node label is copied to every other node that
//! carries that label, whenever it is free (no session): after a session of it
//! ends, and every few minutes to catch up nodes that were away. Only the copy in
//! the profiles directory (the owner's) is used by sessions; the others wait in
//! `<profiles>/.replicas/<id>/`, out of every listing. Opening the profile on a
//! node that holds a current replica is a handoff (a last, small sync, then the
//! owner's copy becomes a replica and the target's replica becomes the profile)
//! instead of copying the whole profile. When the owner is unreachable, the
//! newest replica is promoted instead ("available"): what a session on the lost
//! owner wrote after the last sync is lost, and when that node comes back, the
//! older copy steps down (after a backup).
//!
//! A sync sends only the files that changed (size or modification time) and
//! leaves out what a browser rebuilds by itself (caches, lock files).

use super::*;

/// Where a node keeps the replicas it holds for other nodes (hidden: no listing shows it).
pub const REPLICAS_DIR: &str = ".replicas";

/// Every catch-up round: profiles that changed without a session ending (a rename,
/// notes), and nodes that were away when a session ended.
pub const RECONCILE_EVERY: Duration = Duration::from_secs(300);

/// A plan waits this long for its files.
const PLAN_TTL: Duration = Duration::from_secs(3600);

pub fn replicas_root(profiles_dir: &Path) -> PathBuf {
    profiles_dir.join(REPLICAS_DIR)
}

pub fn replica_dir(profiles_dir: &Path, id: Uuid) -> PathBuf {
    replicas_root(profiles_dir).join(id.to_string())
}

/// Directories a browser rebuilds by itself, and files that only mean something to
/// the browser process that wrote them.
const SKIPPED_DIRS: &[&str] = &[
    // Chromium
    "Cache",
    "Code Cache",
    "GPUCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "ShaderCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "CacheStorage",
    "ScriptCache",
    "component_crx_cache",
    "Crashpad",
    // Firefox
    "cache2",
    "startupCache",
    "thumbnails",
    "crashes",
    "minidumps",
];
const SKIPPED_FILES: &[&str] = &[
    "SingletonLock",
    "SingletonCookie",
    "SingletonSocket",
    "RunningChromeVersion",
    "lock",
    ".parentlock",
    "parent.lock",
];

/// Whether a path inside a profile (relative, `/`-separated) stays out of replicas.
pub fn skipped(rel: &str) -> bool {
    let mut parts = rel.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return SKIPPED_FILES.contains(&part) || SKIPPED_DIRS.contains(&part);
        }
        if SKIPPED_DIRS.contains(&part) {
            return true;
        }
    }
    false
}

/// One file of a profile, as a sync compares it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    /// Seconds: what a tar archive keeps.
    pub mtime: i64,
}

/// The files of a profile directory that replicas keep, sorted by path.
pub fn manifest(dir: &Path) -> std::io::Result<Vec<Entry>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let Some(rel) = rel.to_str().map(str::to_owned) else {
                continue;
            };
            if skipped(&rel) {
                continue;
            }
            // Symlinks are left alone: a browser's are locks.
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                let meta = entry.metadata()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                out.push(Entry {
                    path: rel,
                    size: meta.len(),
                    mtime,
                });
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// What the owner's copy has that the replica lacks or has in another version.
pub fn needed(owner: &[Entry], replica: &[Entry]) -> Vec<String> {
    let have: HashMap<&str, &Entry> = replica.iter().map(|e| (e.path.as_str(), e)).collect();
    owner
        .iter()
        .filter(|e| have.get(e.path.as_str()) != Some(e))
        .map(|e| e.path.clone())
        .collect()
}

/// Files of the replica the owner's copy no longer has.
pub fn obsolete(owner: &[Entry], replica: &[Entry]) -> Vec<String> {
    let keep: std::collections::HashSet<&str> = owner.iter().map(|e| e.path.as_str()).collect();
    replica
        .iter()
        .filter(|e| !keep.contains(e.path.as_str()))
        .map(|e| e.path.clone())
        .collect()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CopyInfo {
    pub id: Uuid,
    pub name: String,
    pub engine: BrowserEngine,
    pub pool: Option<String>,
    pub generation: u64,
    pub owner_since_secs: Option<u64>,
}

impl CopyInfo {
    fn of(meta: &profiles::ProfileMeta) -> Self {
        Self {
            id: meta.id,
            name: meta.name.clone(),
            engine: meta.engine,
            pool: meta.pool.clone(),
            generation: meta.generation,
            owner_since_secs: meta.owner_since_secs,
        }
    }

    /// Which of two copies of one profile is newer: generation, then the later
    /// promotion, then the node name (so every node picks the same one).
    fn rank<'a>(&self, node: &'a str) -> (u64, u64, &'a str) {
        (self.generation, self.owner_since_secs.unwrap_or(0), node)
    }
}

/// What a node holds of pooled profiles: its labels, the profiles it owns, its replicas.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeCopies {
    pub node: String,
    pub labels: Vec<String>,
    pub ephemeral: bool,
    pub primaries: Vec<CopyInfo>,
    pub replicas: Vec<CopyInfo>,
}

pub async fn list_replicas(profiles_dir: &Path) -> Vec<CopyInfo> {
    let root = replicas_root(profiles_dir);
    let mut out = Vec::new();
    let Ok(mut entries) = fs::read_dir(&root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if let Some(meta) = profiles::read_meta(&entry.path()).await {
            out.push(CopyInfo::of(&meta));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

async fn local_copies(state: &AppState) -> NodeCopies {
    let primaries = profiles::list(&state.config.profiles_dir)
        .await
        .iter()
        .filter(|p| p.meta.pool.is_some())
        .map(|p| CopyInfo::of(&p.meta))
        .collect();
    NodeCopies {
        node: state.node_name.clone(),
        labels: state.effective_groups(),
        ephemeral: state.ephemeral_node,
        primaries,
        replicas: list_replicas(&state.config.profiles_dir).await,
    }
}

// ---------------------------------------------------------------------------
// Replica side
// ---------------------------------------------------------------------------

struct Pending {
    files: Vec<Entry>,
    at: Instant,
}

/// Plans waiting for their files, by profile id: the owner sends the whole list
/// once, then only the files it lacks.
static PENDING: std::sync::LazyLock<std::sync::Mutex<HashMap<Uuid, Pending>>> =
    std::sync::LazyLock::new(Default::default);

#[derive(Serialize, Deserialize, Debug)]
pub struct Plan {
    pub meta: profiles::ProfileMeta,
    pub files: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct PlanAnswer {
    /// Files to send.
    pub need: Vec<String>,
    /// Nothing to send and nothing to remove: the replica is current.
    pub current: bool,
}

/// This node owns the profile itself: two owners are a conflict the reconcile resolves.
async fn refuse_if_owner(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    if let Some(own) = profiles::find(&state.config.profiles_dir, &id.to_string()).await {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "This node owns the profile",
                "copy": CopyInfo::of(&own.meta),
            })),
        ));
    }
    Ok(())
}

pub(crate) async fn api_list_copies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<NodeCopies>, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(local_copies(&state).await))
}

pub(crate) async fn api_replica_plan(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(plan): Json<Plan>,
) -> Result<Json<PlanAnswer>, ApiError> {
    state.check_auth(&headers)?;
    if plan.meta.id != id {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Plan is for another profile",
        ));
    }
    if plan.files.iter().any(|e| {
        e.path.is_empty()
            || e.path.starts_with('/')
            || e.path
                .split('/')
                .any(|p| p == ".." || p == "." || p.is_empty())
    }) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Invalid path in plan"));
    }
    let _lock = state.profile_locks.lock(id).await;
    refuse_if_owner(&state, id).await?;
    let dir = replica_dir(&state.config.profiles_dir, id);
    let have = tokio::task::spawn_blocking(move || manifest(&dir))
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let need = needed(&plan.files, &have);
    let current = need.is_empty() && obsolete(&plan.files, &have).is_empty();
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, p| p.at.elapsed() < PLAN_TTL);
    if current {
        pending.remove(&id);
    } else {
        pending.insert(
            id,
            Pending {
                files: plan.files,
                at: Instant::now(),
            },
        );
    }
    Ok(Json(PlanAnswer { need, current }))
}

/// A copy of `src` whose files are hard links: replacing a file in it (rename)
/// leaves `src` as it was.
fn link_clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if kind.is_dir() {
            link_clone(&entry.path(), &to)?;
        } else if kind.is_file() {
            std::fs::hard_link(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Put the received files into `staging`, replacing (not rewriting) what is there,
/// and take out what the owner no longer has.
fn merge_delta(delta: &Path, staging: &Path, files: &[Entry]) -> std::io::Result<()> {
    let mut received = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &entry.path(), out)?;
            } else if kind.is_file() {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.push(rel.to_owned());
                }
            }
        }
        Ok(())
    }
    walk(delta, delta, &mut received)?;
    let wanted: std::collections::HashSet<&str> = files.iter().map(|e| e.path.as_str()).collect();
    for rel in received {
        // Only what the plan named: an archive cannot slip in a lock file or a cache.
        if !rel.to_str().is_some_and(|r| wanted.contains(r)) {
            continue;
        }
        let to = staging.join(&rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(delta.join(&rel), &to)?;
    }
    for rel in obsolete(files, &manifest(staging)?) {
        let _ = std::fs::remove_file(staging.join(rel));
    }
    Ok(())
}

pub(crate) async fn api_replica_apply(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<CopyInfo>, ApiError> {
    state.check_auth(&headers)?;
    let root = replicas_root(&state.config.profiles_dir);
    persist::private_dir(&state.config.profiles_dir)
        .await
        .and(persist::private_dir(&root).await)
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Unpacked before the lock: a slow upload holds nothing up.
    let delta = StagingDir::new(root.join(format!(".delta-{}", Uuid::new_v4())));
    fs::create_dir_all(delta.path())
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    extract_archive(body, delta.path()).await?;

    let _lock = state.profile_locks.lock(id).await;
    refuse_if_owner(&state, id).await?;
    let Some(plan) = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "No plan for this replica; send one first",
        ));
    };
    let current = replica_dir(&state.config.profiles_dir, id);
    let mut staging = StagingDir::new(root.join(format!(".staging-{}", Uuid::new_v4())));
    let (from, to, into) = (
        current.clone(),
        staging.path().to_owned(),
        delta.path().to_owned(),
    );
    tokio::task::spawn_blocking(move || {
        if from.is_dir() {
            link_clone(&from, &to)?;
        } else {
            std::fs::create_dir_all(&to)?;
        }
        merge_delta(&into, &to, &plan.files)
    })
    .await
    .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not update the replica: {e}"),
        )
    })?;
    let Some(meta) = profiles::read_meta(staging.path()).await else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Replica has no profile.json",
        ));
    };
    if meta.id != id {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Replica is of another profile",
        ));
    }
    // Swap in the new copy; the old one goes only once the new one is in place.
    let old = root.join(format!(".old-{}", Uuid::new_v4()));
    let had = fs::metadata(&current).await.is_ok();
    if had {
        fs::rename(&current, &old)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if let Err(e) = fs::rename(staging.path(), &current).await {
        if had {
            let _ = fs::rename(&old, &current).await;
        }
        return Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    staging.disarm();
    if had {
        let _ = fs::remove_dir_all(&old).await;
    }
    state.dir_sizes.forget(&current);
    Ok(Json(CopyInfo::of(&meta)))
}

pub(crate) async fn api_delete_replica(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    state.check_auth(&headers)?;
    let _lock = state.profile_locks.lock(id).await;
    let dir = replica_dir(&state.config.profiles_dir, id);
    match fs::remove_dir_all(&dir).await {
        Ok(()) => {
            info!(profile_id = %id, "Replica removed");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Make the replica the profile sessions of this node use.
pub(crate) async fn promote(state: &AppState, id: Uuid) -> Result<Profile, ApiError> {
    let _guard = state.profiles_lock.lock().await;
    let _lock = state.profile_locks.lock(id).await;
    refuse_if_owner(state, id).await?;
    let from = replica_dir(&state.config.profiles_dir, id);
    let Some(mut meta) = profiles::read_meta(&from).await else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "No replica of this profile here",
        ));
    };
    ensure_name_free(state, &meta.name, Some(id)).await?;
    let to = state.config.profiles_dir.join(id.to_string());
    fs::rename(&from, &to)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    meta.owner_since_secs = Some(profiles::now_secs());
    profiles::write_meta(&to, &meta)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(profile = %meta.name, generation = meta.generation, "Replica promoted: this node owns the profile now");
    Ok(Profile { meta, dir: to })
}

pub(crate) async fn api_promote_replica(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ProfileInfo>, ApiError> {
    state.check_auth(&headers)?;
    let profile = promote(&state, id).await?;
    Ok(Json(profile_info(&state, &profile).await))
}

// ---------------------------------------------------------------------------
// Owner side
// ---------------------------------------------------------------------------

/// The listed files of `dir` as a gzipped tar, produced while it is read.
fn tar_files(
    dir: &Path,
    paths: Vec<String>,
) -> std::io::Result<impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
    let mut child = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .arg("--null")
        .arg("--no-recursion")
        .arg("-T")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("tar has no input"))?;
    tokio::spawn(async move {
        let mut list = Vec::new();
        for p in paths {
            list.extend_from_slice(p.as_bytes());
            list.push(0);
        }
        let _ = stdin.write_all(&list).await;
    });
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
                        Some((
                            Err(std::io::Error::other(format!(
                                "tar failed ({other:?}): {why}"
                            ))),
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

async fn peer_copies(state: &AppState, url: &str) -> Option<NodeCopies> {
    state
        .peer_request(state.http_client.get(format!("{url}/api/replicas")))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

/// The reachable peers carrying `pool`, with what they hold.
pub async fn pool_peers(state: &AppState, pool: &str) -> Vec<(String, NodeCopies)> {
    let peers: Vec<(String, String)> = state
        .peers
        .read()
        .await
        .iter()
        .map(|(n, u)| (n.clone(), u.clone()))
        .collect();
    let found = futures_util::future::join_all(peers.into_iter().map(|(name, url)| async move {
        let copies = peer_copies(state, &url).await?;
        (copies.node == name && !copies.ephemeral && copies.labels.iter().any(|l| l == pool))
            .then_some((url, copies))
    }))
    .await;
    found.into_iter().flatten().collect()
}

/// Bring the replica on `url` up to date. The caller holds the profile's lock and
/// made sure no session uses it. Returns whether anything was sent.
async fn sync_to(state: &AppState, profile: &Profile, url: &str) -> Result<bool, String> {
    let dir = profile.dir.clone();
    let files = tokio::task::spawn_blocking(move || manifest(&dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let id = profile.meta.id;
    let answer: PlanAnswer = state
        .peer_request(
            state
                .peer_client
                .post(format!("{url}/api/replicas/{id}/plan"))
                .json(&Plan {
                    meta: profile.meta.clone(),
                    files,
                }),
        )
        .timeout(PeerWait::Short.timeout())
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if answer.current {
        return Ok(false);
    }
    let archive = tar_files(&profile.dir, answer.need).map_err(|e| e.to_string())?;
    state
        .peer_request(
            state
                .peer_client
                .put(format!("{url}/api/replicas/{id}"))
                .header(header::CONTENT_TYPE, "application/gzip")
                .body(reqwest::Body::wrap_stream(archive)),
        )
        .timeout(PeerWait::Long.timeout())
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Sync a free pooled profile to the other nodes of its pool. Returns false when
/// the profile is in use (it syncs after its session ends).
pub async fn sync_profile(state: &AppState, id: Uuid) -> bool {
    let _lock = state.profile_locks.lock(id).await;
    let Some(profile) = profiles::find(&state.config.profiles_dir, &id.to_string()).await else {
        return false;
    };
    let Some(pool) = profile.meta.pool.clone() else {
        return false;
    };
    if profile_use(state, id).await.is_some() {
        return false;
    }
    for (url, copies) in pool_peers(state, &pool).await {
        // The other node owns it too: the reconcile settles that, not a sync.
        if copies.primaries.iter().any(|c| c.id == id) {
            continue;
        }
        match sync_to(state, &profile, &url).await {
            Ok(true) => {
                info!(profile = %profile.meta.name, node = %copies.node, generation = profile.meta.generation, "Replica updated")
            }
            Ok(false) => {}
            Err(e) => {
                warn!(profile = %profile.meta.name, node = %copies.node, error = %e, "Could not update replica")
            }
        }
    }
    true
}

/// A session of the profile ended: count it and pass the result on.
pub async fn after_session(state: AppState, id: Uuid) {
    {
        let _lock = state.profile_locks.lock(id).await;
        let Some(mut profile) = profiles::find(&state.config.profiles_dir, &id.to_string()).await
        else {
            return;
        };
        if profile.meta.pool.is_none() {
            return;
        }
        profile.meta.generation += 1;
        if let Err(e) = profiles::write_meta(&profile.dir, &profile.meta).await {
            warn!(profile = %profile.meta.name, error = %e, "Could not count the session");
            return;
        }
    }
    sync_profile(&state, id).await;
}

/// Turn this node's copy into a replica (another node owns the profile now).
async fn demote(state: &AppState, profile: &Profile) -> std::io::Result<()> {
    let root = replicas_root(&state.config.profiles_dir);
    persist::private_dir(&root).await?;
    let to = replica_dir(&state.config.profiles_dir, profile.meta.id);
    if fs::metadata(&to).await.is_ok() {
        fs::remove_dir_all(&to).await?;
    }
    state.dir_sizes.forget(&profile.dir);
    fs::rename(&profile.dir, &to).await
}

#[derive(Deserialize)]
pub(crate) struct HandoffRequest {
    pub target_node: String,
}

/// Hand a free pooled profile to another node of its pool: a last sync, then this
/// copy becomes a replica. The caller promotes the target's replica next.
pub(crate) async fn api_handoff_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<HandoffRequest>,
) -> Result<Json<CopyInfo>, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(handoff(&state, &key, req.target_node.trim()).await?))
}

pub(crate) async fn handoff(
    state: &AppState,
    key: &str,
    target_node: &str,
) -> Result<CopyInfo, ApiError> {
    let profile = lookup_profile(state, key).await?;
    if profile.meta.pool.is_none() {
        return Err(api_error(StatusCode::BAD_REQUEST, "Profile has no pool"));
    }
    let url = get_peer_url(state, target_node)
        .await
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Target node not found in mesh"))?;
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    ensure_profile_idle(state, &profile, "hand off").await?;
    sync_to(state, &profile, &url).await.map_err(|e| {
        api_error(
            StatusCode::BAD_GATEWAY,
            format!("Could not sync to the target: {e}"),
        )
    })?;
    demote(state, &profile)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(profile = %profile.meta.name, target = %target_node, "Profile handed off");
    Ok(CopyInfo::of(&profile.meta))
}

/// Promote the replica on a node (this one when `url` is empty).
pub(crate) async fn promote_on(state: &AppState, url: &str, id: Uuid) -> Result<(), ApiError> {
    if url.is_empty() {
        return promote(state, id).await.map(|_| ());
    }
    let res = state
        .peer_request(
            state
                .peer_client
                .post(format!("{url}/api/replicas/{id}/promote")),
        )
        .timeout(PeerWait::Short.timeout())
        .send()
        .await
        .map_err(|e| {
            api_error(
                StatusCode::BAD_GATEWAY,
                format!("Could not promote the replica: {e}"),
            )
        })?;
    if !res.status().is_success() {
        let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return Err(api_error(status, "The node refused to promote its replica"));
    }
    Ok(())
}

/// Ask the owner (this node when `url` is empty) to hand the profile to `target_node`.
pub(crate) async fn handoff_on(
    state: &AppState,
    url: &str,
    key: &str,
    target_node: &str,
) -> Result<(), ApiError> {
    if url.is_empty() {
        return handoff(state, key, target_node).await.map(|_| ());
    }
    let res = state
        .peer_request(
            state
                .peer_client
                .post(profile_transfer::peer_profile_url(
                    url,
                    key,
                    Some("handoff"),
                ))
                .json(&serde_json::json!({ "target_node": target_node })),
        )
        .timeout(PeerWait::Long.timeout())
        .send()
        .await
        .map_err(|e| {
            api_error(
                StatusCode::BAD_GATEWAY,
                format!("Handoff request failed: {e}"),
            )
        })?;
    if !res.status().is_success() {
        let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = res.json::<serde_json::Value>().await.unwrap_or_default();
        return Err((status, Json(body)));
    }
    Ok(())
}

/// Remove the profile's replicas from its pool (it was deleted, or left the pool).
pub async fn drop_replicas(state: &AppState, id: Uuid, pool: &str) {
    for (url, copies) in pool_peers(state, pool).await {
        if !copies.replicas.iter().any(|c| c.id == id) {
            continue;
        }
        let _ = state
            .peer_request(state.peer_client.delete(format!("{url}/api/replicas/{id}")))
            .timeout(PeerWait::Short.timeout())
            .send()
            .await;
    }
}

/// Two nodes own one profile (a replica was promoted while the owner was away):
/// the older copy steps down, after a backup of what it had.
async fn settle_conflicts(
    state: &AppState,
    profile: &Profile,
    peers: &[(String, NodeCopies)],
) -> bool {
    let mine = CopyInfo::of(&profile.meta);
    let Some(winner) = peers
        .iter()
        .flat_map(|(_, c)| {
            c.primaries
                .iter()
                .filter(|p| p.id == mine.id)
                .map(move |p| (p, &c.node))
        })
        .filter(|(p, node)| p.rank(node) > mine.rank(&state.node_name))
        .max_by(|a, b| a.0.rank(a.1).cmp(&b.0.rank(b.1)))
    else {
        return false;
    };
    let _lock = state.profile_locks.lock(mine.id).await;
    if profile_use(state, mine.id).await.is_some() {
        warn!(profile = %mine.name, other = %winner.1, "Two nodes own this profile; it is in use here, so it stays until its session ends");
        return true;
    }
    match backups::create(
        &state.config.profiles_dir,
        profile,
        Some("replication conflict"),
    )
    .await
    {
        Ok(_) => {}
        Err(e) => {
            warn!(profile = %mine.name, error = %e, "Two nodes own this profile, but the backup before stepping down failed: kept");
            return true;
        }
    }
    match demote(state, profile).await {
        Ok(()) => {
            warn!(profile = %mine.name, generation = mine.generation, owner = %winner.1, owner_generation = winner.0.generation, "Another node owns a newer copy of this profile: this one is a replica now (backup kept)")
        }
        Err(e) => warn!(profile = %mine.name, error = %e, "Could not step down"),
    }
    true
}

/// One catch-up round over this node's pooled profiles.
pub async fn reconcile(state: &AppState) {
    let mut by_pool: HashMap<String, Vec<Profile>> = HashMap::new();
    for p in profiles::list(&state.config.profiles_dir).await {
        if let Some(pool) = p.meta.pool.clone() {
            by_pool.entry(pool).or_default().push(p);
        }
    }
    for (pool, owned) in by_pool {
        let peers = pool_peers(state, &pool).await;
        for profile in owned {
            if settle_conflicts(state, &profile, &peers).await {
                continue;
            }
            sync_profile(state, profile.meta.id).await;
        }
    }
}

/// Where the newest replica of a profile is, among the reachable nodes (this one included).
pub async fn newest_replica(state: &AppState, name: &str) -> Option<(String, String, CopyInfo)> {
    let mut found: Vec<(String, String, CopyInfo)> = list_replicas(&state.config.profiles_dir)
        .await
        .into_iter()
        .filter(|c| c.name == name)
        .map(|c| (state.node_name.clone(), String::new(), c))
        .collect();
    let peers: Vec<String> = state.peers.read().await.values().cloned().collect();
    let theirs =
        futures_util::future::join_all(peers.iter().map(|url| peer_copies(state, url))).await;
    for (url, copies) in peers.iter().zip(theirs) {
        let Some(copies) = copies else { continue };
        for c in copies.replicas.into_iter().filter(|c| c.name == name) {
            found.push((copies.node.clone(), url.clone(), c));
        }
    }
    found
        .into_iter()
        .max_by(|a, b| a.2.rank(&a.0).cmp(&b.2.rank(&b.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            path: path.into(),
            size,
            mtime,
        }
    }

    #[test]
    fn caches_and_locks_stay_out_of_replicas() {
        for rel in [
            "Default/Cache/Cache_Data/index",
            "Default/Code Cache/js/abc",
            "GPUCache/data_0",
            "Default/Service Worker/CacheStorage/x",
            "SingletonLock",
            "cache2/entries/ABC",
            ".parentlock",
        ] {
            assert!(skipped(rel), "{rel}");
        }
        for rel in [
            "Default/Cookies",
            "Default/Local Storage/leveldb/000003.log",
            "Default/IndexedDB/https_x_0.indexeddb.leveldb/CURRENT",
            "profile.json",
            "cookies.sqlite",
            "Default/Cache.txt",
        ] {
            assert!(!skipped(rel), "{rel}");
        }
    }

    #[test]
    fn only_changed_files_are_sent_and_gone_ones_removed() {
        let owner = vec![e("a", 1, 10), e("b", 2, 20), e("c", 3, 30)];
        let replica = vec![e("a", 1, 10), e("b", 2, 19), e("d", 4, 40)];
        assert_eq!(needed(&owner, &replica), vec!["b", "c"]);
        assert_eq!(obsolete(&owner, &replica), vec!["d"]);
        assert!(needed(&owner, &owner).is_empty() && obsolete(&owner, &owner).is_empty());
    }

    #[test]
    fn the_newer_copy_wins_and_every_node_agrees() {
        let copy = |generation, since| CopyInfo {
            id: Uuid::nil(),
            name: "p".into(),
            engine: BrowserEngine::Chromium,
            pool: Some("cloud".into()),
            generation,
            owner_since_secs: Some(since),
        };
        assert!(copy(5, 1).rank("a") > copy(4, 9).rank("z"));
        assert!(copy(5, 9).rank("a") > copy(5, 1).rank("z"));
        assert!(copy(5, 1).rank("b") > copy(5, 1).rank("a"));
    }

    #[test]
    fn manifests_list_kept_files_and_merges_replace_without_touching_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = tmp.path().join("owner");
        std::fs::create_dir_all(owner.join("Default/Cache")).unwrap();
        std::fs::write(owner.join("Default/Cookies"), b"new").unwrap();
        std::fs::write(owner.join("Default/Cache/x"), b"cache").unwrap();
        std::fs::write(owner.join("profile.json"), b"{}").unwrap();
        let files = manifest(&owner).unwrap();
        let paths: Vec<_> = files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["Default/Cookies", "profile.json"]);

        // The old replica: an outdated cookie file and a file the owner dropped.
        let old = tmp.path().join("old");
        std::fs::create_dir_all(old.join("Default")).unwrap();
        std::fs::write(old.join("Default/Cookies"), b"old").unwrap();
        std::fs::write(old.join("gone"), b"x").unwrap();
        let staging = tmp.path().join("staging");
        link_clone(&old, &staging).unwrap();
        let delta = tmp.path().join("delta");
        std::fs::create_dir_all(delta.join("Default")).unwrap();
        std::fs::write(delta.join("Default/Cookies"), b"new").unwrap();
        std::fs::write(delta.join("profile.json"), b"{}").unwrap();
        std::fs::write(delta.join("SingletonLock"), b"no").unwrap();
        merge_delta(&delta, &staging, &files).unwrap();

        assert_eq!(
            std::fs::read(staging.join("Default/Cookies")).unwrap(),
            b"new"
        );
        assert!(!staging.join("gone").exists());
        assert!(!staging.join("SingletonLock").exists());
        // The previous replica is unchanged until it is swapped out.
        assert_eq!(std::fs::read(old.join("Default/Cookies")).unwrap(), b"old");
        assert!(old.join("gone").exists());
    }
}
