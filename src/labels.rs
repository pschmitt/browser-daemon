//! Labels: short tags on nodes, profiles, sessions and recordings, and the label
//! registry (an optional icon per label) that the nodes of a mesh keep in step.
//!
//! - Where an item keeps its labels: a node in its settings (plus the immutable ones
//!   from `--node-group`), a profile in its metadata, a recording in its directory
//!   (`bd-labels.json`), a session in `<state dir>/labels/<id>.json` (gone with it).
//! - A label exists as soon as an item carries it; the registry only holds what is
//!   *about* a label: its icon, and a tombstone once it was deleted or renamed.
//! - Every node holds a copy of the registry. A change is pushed to the peers that
//!   are up, and nodes compare registries whenever the mesh overview is fetched, so
//!   a node that was away catches up. The newest change of a label wins; a
//!   tombstone beats a definition that is older than it and makes the node take the
//!   label off its own items (or put its replacement there, after a rename).

use std::collections::BTreeMap;

use super::*;

/// The labels file in a recording's directory.
pub const FILE: &str = "bd-labels.json";
const REGISTRY_FILE: &str = "labels.json";
/// The largest custom picture of a label.
pub const MAX_IMAGE_BYTES: usize = 128 * 1024;
/// How long a tombstone is kept for nodes that were away.
const TOMBSTONE_KEEP_MS: u64 = 30 * 24 * 3600 * 1000;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Node,
    Session,
    Profile,
    Recording,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "node" | "nodes" => Some(Self::Node),
            "session" | "sessions" => Some(Self::Session),
            "profile" | "profiles" => Some(Self::Profile),
            "recording" | "recordings" => Some(Self::Recording),
            _ => None,
        }
    }
}

/// A label's picture: a Material Design Icons name (`home-assistant`), or an image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LabelIcon {
    Mdi { name: String },
    Image { mime: String, data: String },
}

impl LabelIcon {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Mdi { name } => {
                let ok = !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                ok.then_some(())
                    .ok_or_else(|| "An mdi icon name is lowercase letters, digits and -".into())
            }
            Self::Image { mime, data } => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| "The picture is not valid base64".to_string())?;
                if !profiles::ICON_TYPES.contains(&mime.as_str())
                    || bytes.is_empty()
                    || bytes.len() > MAX_IMAGE_BYTES
                {
                    return Err(format!(
                        "The picture must be a png, svg, webp, jpeg or gif of at most {} KB",
                        MAX_IMAGE_BYTES / 1024
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LabelDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<LabelIcon>,
    /// Milliseconds; the newest definition of a label wins.
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Tombstone {
    pub at: u64,
    /// The label that replaced it, when it was renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Registry {
    #[serde(default)]
    pub defs: BTreeMap<String, LabelDef>,
    #[serde(default)]
    pub deleted: BTreeMap<String, Tombstone>,
}

/// A label's picture without the image itself (`GET /api/labels/<name>/icon`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IconDigest {
    Mdi { name: String },
    Image { mime: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DefDigest {
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconDigest>,
}

/// The registry as the overview and the peers' comparison carry it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Digest {
    #[serde(default)]
    pub defs: BTreeMap<String, DefDigest>,
    #[serde(default)]
    pub deleted: BTreeMap<String, Tombstone>,
}

/// What a node tells a peer: its items' labels (sessions and recordings) and its registry.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LabelSync {
    #[serde(default)]
    pub items: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub registry: Digest,
}

/// The result of merging a peer's registry in.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    pub changed: bool,
    /// Labels whose tombstone is new here: take them off the items, or replace them.
    pub cascades: Vec<(String, Option<String>)>,
}

impl Registry {
    pub fn digest(&self) -> Digest {
        Digest {
            defs: self
                .defs
                .iter()
                .map(|(name, d)| {
                    let icon = d.icon.as_ref().map(|i| match i {
                        LabelIcon::Mdi { name } => IconDigest::Mdi { name: name.clone() },
                        LabelIcon::Image { mime, .. } => IconDigest::Image { mime: mime.clone() },
                    });
                    (
                        name.clone(),
                        DefDigest {
                            updated_at: d.updated_at,
                            icon,
                        },
                    )
                })
                .collect(),
            deleted: self.deleted.clone(),
        }
    }

    /// Take over what `other` knows that is newer than what this has.
    pub fn merge(&mut self, other: &Registry) -> Merged {
        let mut out = Merged::default();
        for (name, t) in &other.deleted {
            let def_at = self.defs.get(name).map(|d| d.updated_at);
            let tomb_at = self.deleted.get(name).map(|t| t.at);
            if def_at.is_none_or(|d| t.at > d) && tomb_at.is_none_or(|l| t.at > l) {
                self.defs.remove(name);
                self.deleted.insert(name.clone(), t.clone());
                out.cascades.push((name.clone(), t.renamed_to.clone()));
                out.changed = true;
            }
        }
        for (name, d) in &other.defs {
            if self.deleted.get(name).is_some_and(|t| t.at >= d.updated_at) {
                continue;
            }
            if self
                .defs
                .get(name)
                .is_none_or(|l| d.updated_at > l.updated_at)
            {
                self.defs.insert(name.clone(), d.clone());
                self.deleted.remove(name);
                out.changed = true;
            }
        }
        let cutoff = now_ms().saturating_sub(TOMBSTONE_KEEP_MS);
        let before = self.deleted.len();
        self.deleted.retain(|_, t| t.at >= cutoff);
        out.changed |= self.deleted.len() != before;
        out
    }

    /// What this knows that a peer, which reported `digest`, does not.
    pub fn newer_than(&self, digest: &Digest) -> Registry {
        Registry {
            defs: self
                .defs
                .iter()
                .filter(|(n, d)| {
                    digest
                        .defs
                        .get(*n)
                        .is_none_or(|p| d.updated_at > p.updated_at)
                        && digest.deleted.get(*n).is_none_or(|t| t.at < d.updated_at)
                })
                .map(|(n, d)| (n.clone(), d.clone()))
                .collect(),
            deleted: self
                .deleted
                .iter()
                .filter(|(n, t)| {
                    digest.deleted.get(*n).is_none_or(|p| t.at > p.at)
                        && digest.defs.get(*n).is_none_or(|d| t.at > d.updated_at)
                })
                .map(|(n, t)| (n.clone(), t.clone()))
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.defs.is_empty() && self.deleted.is_empty()
    }
}

/// `labels` with `old` taken off, or replaced by `new`; false when it was not there.
pub fn rewrite(labels: &mut Vec<String>, old: &str, new: Option<&str>) -> bool {
    let Some(pos) = labels.iter().position(|l| l == old) else {
        return false;
    };
    labels.remove(pos);
    if let Some(new) = new {
        if !labels.iter().any(|l| l == new) {
            labels.push(new.to_string());
            labels.sort();
        }
    }
    true
}

// ------------------------------------------------------------------ storage

/// One change at a time: reads, changes and writes are a unit.
static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

fn io_error(e: std::io::Error) -> ApiError {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Could not save the labels: {e}"),
    )
}

fn registry_path(state: &AppState) -> PathBuf {
    state.config.state_dir().join(REGISTRY_FILE)
}

fn sessions_dir(state: &AppState) -> PathBuf {
    state.config.state_dir().join("labels")
}

async fn load(state: &AppState) -> Registry {
    match fs::read(registry_path(state)).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(error = %e, "Ignoring the unreadable label registry");
            Registry::default()
        }),
        Err(_) => Registry::default(),
    }
}

async fn save(state: &AppState, registry: &Registry) -> std::io::Result<()> {
    persist::private_dir(state.config.state_dir()).await?;
    persist::write_atomic(&registry_path(state), &serde_json::to_vec_pretty(registry)?).await
}

async fn read_list(path: &Path) -> Vec<String> {
    match fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn write_list(path: &Path, labels: &[String]) -> std::io::Result<()> {
    if labels.is_empty() {
        return match fs::remove_file(path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        persist::private_dir(dir).await.ok();
    }
    persist::write_atomic(path, &serde_json::to_vec_pretty(labels)?).await
}

/// A session ended: its labels go with it.
pub async fn forget_session(state: &AppState, id: Uuid) {
    let _ = fs::remove_file(sessions_dir(state).join(format!("{id}.json"))).await;
}

/// Where an item's labels are kept.
enum Place {
    File(PathBuf),
    Profile(Box<Profile>),
    Node,
}

async fn place_of(state: &AppState, kind: Kind, key: &str) -> Result<Place, ApiError> {
    let gone = |what: &str| api_error(StatusCode::NOT_FOUND, format!("{what} not found"));
    match kind {
        Kind::Node if key == state.node_name => Ok(Place::Node),
        Kind::Node => Err(gone("Node")),
        Kind::Session => {
            let id: Uuid = key.parse().map_err(|_| gone("Session"))?;
            if !state.sessions.read().await.contains_key(&id) {
                return Err(gone("Session"));
            }
            Ok(Place::File(sessions_dir(state).join(format!("{id}.json"))))
        }
        Kind::Profile => Ok(Place::Profile(Box::new(lookup_profile(state, key).await?))),
        Kind::Recording => {
            let id: Uuid = key.parse().map_err(|_| gone("Recording"))?;
            let dir = state.recorder_config.recordings_dir.join(id.to_string());
            if !fs::try_exists(dir.join("meta.json")).await.unwrap_or(false) {
                return Err(gone("Recording"));
            }
            Ok(Place::File(dir.join(FILE)))
        }
    }
}

async fn current(state: &AppState, place: &Place) -> Vec<String> {
    match place {
        Place::File(path) => read_list(path).await,
        Place::Profile(p) => p.meta.labels.clone(),
        Place::Node => state.effective_groups(),
    }
}

async fn store(state: &AppState, place: Place, labels: Vec<String>) -> Result<(), ApiError> {
    match place {
        Place::File(path) => write_list(&path, &labels).await.map_err(io_error),
        Place::Profile(mut p) => {
            p.meta.labels = labels;
            profiles::write_meta(&p.dir, &p.meta)
                .await
                .map_err(io_error)
        }
        Place::Node => {
            let fixed = &state.node_groups;
            let runtime: Vec<String> = labels.into_iter().filter(|l| !fixed.contains(l)).collect();
            state
                .settings
                .update(|s| {
                    s.labels = runtime;
                    Ok::<(), ApiError>(())
                })
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    settings::UpdateError::Rejected(e) => e,
                    settings::UpdateError::Io(e) => io_error(e),
                })
        }
    }
}

/// A change to an item's labels: replace them, then add, then remove.
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct LabelChange {
    #[serde(default, deserialize_with = "profiles_api::tabs_field")]
    pub labels: Option<Vec<String>>,
    #[serde(default, deserialize_with = "profiles_api::tabs_field")]
    pub add: Option<Vec<String>>,
    #[serde(default, deserialize_with = "profiles_api::tabs_field")]
    pub remove: Option<Vec<String>>,
}

impl LabelChange {
    pub fn apply(&self, current: &[String]) -> Result<Vec<String>, String> {
        let mut labels = match self.labels.as_deref() {
            Some(replacement) => profiles::normalize_labels(replacement)?,
            None => current.to_vec(),
        };
        labels.extend(profiles::normalize_labels(
            self.add.as_deref().unwrap_or_default(),
        )?);
        let removed = profiles::normalize_labels(self.remove.as_deref().unwrap_or_default())?;
        labels.retain(|l| !removed.contains(l));
        profiles::normalize_labels(&labels)
    }
}

/// A label that was deleted and is put to use again gets a fresh definition, which
/// beats the tombstone on every node. Returns what changed, to be pushed.
pub async fn note_assigned(state: &AppState, labels: &[String]) -> Option<Registry> {
    let _guard = LOCK.lock().await;
    let mut registry = load(state).await;
    let mut delta = Registry::default();
    for label in labels {
        if registry.deleted.remove(label).is_some() {
            let def = LabelDef {
                icon: None,
                updated_at: now_ms(),
            };
            registry.defs.insert(label.clone(), def.clone());
            delta.defs.insert(label.clone(), def);
        }
    }
    if delta.is_empty() || save(state, &registry).await.is_err() {
        return None;
    }
    Some(delta)
}

/// Apply a change to one item of this node; returns its labels afterwards.
pub async fn change_item(
    state: &AppState,
    kind: Kind,
    key: &str,
    change: &LabelChange,
) -> Result<Vec<String>, ApiError> {
    let place = place_of(state, kind, key).await?;
    let before = current(state, &place).await;
    let after = change
        .apply(&before)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    if after != before {
        store(state, place, after.clone()).await?;
        let added: Vec<String> = after
            .iter()
            .filter(|l| !before.contains(l))
            .cloned()
            .collect();
        if let Some(delta) = note_assigned(state, &added).await {
            push(state, &delta).await;
        }
        info!(kind = ?kind, item = key, labels = ?after, "Labels changed");
    }
    Ok(after)
}

/// The labels of this node's sessions and recordings, `"<kind>/<id>"` -> labels.
pub async fn items(state: &AppState) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let sessions: Vec<Uuid> = state.sessions.read().await.keys().copied().collect();
    for id in sessions {
        let labels = read_list(&sessions_dir(state).join(format!("{id}.json"))).await;
        if !labels.is_empty() {
            out.insert(format!("session/{id}"), labels);
        }
    }
    if let Ok(mut dir) = fs::read_dir(&state.recorder_config.recordings_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let labels = read_list(&entry.path().join(FILE)).await;
            if !labels.is_empty() {
                out.insert(
                    format!("recording/{}", entry.file_name().to_string_lossy()),
                    labels,
                );
            }
        }
    }
    out
}

/// Take `old` off every item of this node (or put `new` there instead).
pub async fn cascade(state: &AppState, old: &str, new: Option<&str>) {
    // Sessions and recordings.
    if let Ok(mut dir) = fs::read_dir(sessions_dir(state)).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            rewrite_file(&entry.path(), old, new).await;
        }
    }
    if let Ok(mut dir) = fs::read_dir(&state.recorder_config.recordings_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            rewrite_file(&entry.path().join(FILE), old, new).await;
        }
    }
    // Profiles.
    let _guard = state.profiles_lock.lock().await;
    for mut p in profiles::list(&state.config.profiles_dir).await {
        if rewrite(&mut p.meta.labels, old, new) {
            if let Err(e) = profiles::write_meta(&p.dir, &p.meta).await {
                warn!(profile = %p.meta.name, error = %e, "Could not update a profile's labels");
            }
        }
    }
    // This node. (Labels from the startup flags stay: change them in its configuration.)
    let _ = state
        .settings
        .update(|s| {
            rewrite(&mut s.labels, old, new);
            Ok::<(), ()>(())
        })
        .await;
}

async fn rewrite_file(path: &Path, old: &str, new: Option<&str>) {
    if !fs::try_exists(path).await.unwrap_or(false) {
        return;
    }
    let mut labels = read_list(path).await;
    if rewrite(&mut labels, old, new) {
        let _ = write_list(path, &labels).await;
    }
}

// --------------------------------------------------------------------- sync

/// Send `delta` to every peer (those that are down catch up later).
pub async fn push(state: &AppState, delta: &Registry) {
    if delta.is_empty() {
        return;
    }
    let urls: Vec<String> = state.peers.read().await.values().cloned().collect();
    futures_util::future::join_all(urls.into_iter().map(|url| async move {
        let _ = state
            .peer_request(
                state
                    .http_client
                    .post(format!("{url}/api/label-sync/merge"))
                    .json(delta),
            )
            .timeout(Duration::from_secs(4))
            .send()
            .await;
    }))
    .await;
}

/// Merge what a peer (or a push) brings, and apply what is new to this node's items.
pub async fn merge_in(state: &AppState, other: &Registry) -> std::io::Result<Merged> {
    let merged = {
        let _guard = LOCK.lock().await;
        let mut registry = load(state).await;
        let merged = registry.merge(other);
        if merged.changed {
            save(state, &registry).await?;
        }
        merged
    };
    for (old, new) in &merged.cascades {
        cascade(state, old, new.as_deref()).await;
    }
    Ok(merged)
}

/// Compare registries with a peer that reported `theirs`: take what is newer there
/// (fetching the images), and send it what is newer here.
pub async fn reconcile(state: &AppState, peer_url: &str, theirs: &Digest) {
    let ours = load(state).await.digest();
    if &ours == theirs {
        return;
    }
    let mut incoming = Registry {
        deleted: theirs.deleted.clone(),
        defs: BTreeMap::new(),
    };
    for (name, d) in &theirs.defs {
        if ours
            .defs
            .get(name)
            .is_some_and(|o| o.updated_at >= d.updated_at)
        {
            continue;
        }
        match &d.icon {
            None => {
                incoming.defs.insert(
                    name.clone(),
                    LabelDef {
                        icon: None,
                        updated_at: d.updated_at,
                    },
                );
            }
            Some(IconDigest::Mdi { name: icon }) => {
                incoming.defs.insert(
                    name.clone(),
                    LabelDef {
                        icon: Some(LabelIcon::Mdi { name: icon.clone() }),
                        updated_at: d.updated_at,
                    },
                );
            }
            Some(IconDigest::Image { .. }) => {
                let url = format!("{peer_url}/api/label-sync/{}", urlencode(name));
                if let Ok(res) = state
                    .peer_request(state.http_client.get(url))
                    .timeout(Duration::from_secs(4))
                    .send()
                    .await
                {
                    if let Ok(def) = res.json::<LabelDef>().await {
                        incoming.defs.insert(name.clone(), def);
                    }
                }
            }
        }
    }
    if let Err(e) = merge_in(state, &incoming).await {
        warn!(error = %e, "Could not merge a peer's label registry");
    }
    let delta = load(state).await.newer_than(theirs);
    if !delta.is_empty() {
        let _ = state
            .peer_request(
                state
                    .http_client
                    .post(format!("{peer_url}/api/label-sync/merge"))
                    .json(&delta),
            )
            .timeout(Duration::from_secs(4))
            .send()
            .await;
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// This node's side of the comparison.
pub async fn sync_state(state: &AppState) -> LabelSync {
    LabelSync {
        items: items(state).await,
        registry: load(state).await.digest(),
    }
}

/// The registry as the overview carries it.
pub async fn digest(state: &AppState) -> Digest {
    load(state).await.digest()
}

// ---------------------------------------------------------------------- API

/// `GET /api/label-sync`: this node's item labels and registry (for its peers).
pub(crate) async fn api_sync(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(sync_state(&state).await))
}

/// `GET /api/label-sync/<name>`: one definition, with its image.
pub(crate) async fn api_sync_def(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    load(&state)
        .await
        .defs
        .remove(&name)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Label not defined"))
}

/// `POST /api/label-sync/merge`: take over what is newer in the registry sent.
pub(crate) async fn api_sync_merge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(other): Json<Registry>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    merge_in(&state, &other).await.map_err(io_error)?;
    Ok(Json(digest(&state).await))
}

/// `GET /api/labels`: the registry (icons, tombstones).
pub(crate) async fn api_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(digest(&state).await))
}

/// `GET /api/labels/<name>/icon`: a label's own image.
pub(crate) async fn api_icon(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    use base64::Engine as _;
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Some(LabelIcon::Image { mime, data }) =
        load(&state).await.defs.remove(&name).and_then(|d| d.icon)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Deserialize, Debug, Default)]
pub(crate) struct PutLabel {
    /// Absent: unchanged; null: no icon.
    #[serde(default, deserialize_with = "profiles_api::nullable")]
    pub(crate) icon: Option<Option<LabelIcon>>,
    /// Rename the label (on every node's items); an existing label of that name
    /// takes the items over.
    #[serde(default)]
    pub(crate) rename_to: Option<String>,
}

/// `PUT /api/labels/<name>`: set a label's icon or rename it, on the whole mesh.
pub(crate) async fn api_put(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PutLabel>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let bad = |e: String| api_error(StatusCode::BAD_REQUEST, e);
    let name = profiles::normalize_label(&name).map_err(bad)?;
    if let Some(Some(ref icon)) = req.icon {
        icon.check().map_err(bad)?;
    }
    let target = req
        .rename_to
        .as_deref()
        .map(profiles::normalize_label)
        .transpose()
        .map_err(bad)?
        .filter(|t| *t != name);
    let mut delta = Registry::default();
    let mut cascades = Vec::new();
    {
        let _guard = LOCK.lock().await;
        let mut registry = load(&state).await;
        let now = now_ms();
        let mut def = registry.defs.get(&name).cloned().unwrap_or_default();
        if let Some(icon) = req.icon {
            def.icon = icon;
        }
        def.updated_at = now;
        match target {
            None => {
                registry.deleted.remove(&name);
                registry.defs.insert(name.clone(), def.clone());
                delta.defs.insert(name.clone(), def);
            }
            Some(ref to) => {
                // The new name keeps the old one's icon unless it has one already.
                let mut new_def = registry.defs.get(to).cloned().unwrap_or_default();
                if new_def.icon.is_none() {
                    new_def.icon = def.icon.clone();
                }
                new_def.updated_at = now;
                registry.deleted.remove(to);
                registry.defs.insert(to.clone(), new_def.clone());
                delta.defs.insert(to.clone(), new_def);
                registry.defs.remove(&name);
                let tomb = Tombstone {
                    at: now,
                    renamed_to: Some(to.clone()),
                };
                registry.deleted.insert(name.clone(), tomb.clone());
                delta.deleted.insert(name.clone(), tomb);
                cascades.push((name.clone(), Some(to.clone())));
            }
        }
        save(&state, &registry).await.map_err(io_error)?;
    }
    for (old, new) in &cascades {
        cascade(&state, old, new.as_deref()).await;
    }
    push(&state, &delta).await;
    Ok(Json(digest(&state).await))
}

/// `DELETE /api/labels/<name>`: remove the label from every node, profile, session
/// and recording of the mesh, and forget its icon.
pub(crate) async fn api_delete(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name =
        profiles::normalize_label(&name).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    let mut delta = Registry::default();
    {
        let _guard = LOCK.lock().await;
        let mut registry = load(&state).await;
        let tomb = Tombstone {
            at: now_ms(),
            renamed_to: None,
        };
        registry.defs.remove(&name);
        registry.deleted.insert(name.clone(), tomb.clone());
        delta.deleted.insert(name.clone(), tomb);
        save(&state, &registry).await.map_err(io_error)?;
    }
    cascade(&state, &name, None).await;
    push(&state, &delta).await;
    info!(label = %name, "Label deleted");
    Ok(Json(serde_json::json!({
        "status": "deleted",
        // Labels of the startup flags stay on their node: change its configuration.
        "kept_by_flags": state.node_groups.contains(&name),
    })))
}

fn kind_of(s: &str) -> Result<Kind, ApiError> {
    Kind::parse(s).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "Labels are for nodes, profiles, sessions and recordings",
        )
    })
}

/// `GET /api/item-labels/<kind>/<id>`: an item's labels.
pub(crate) async fn api_item_get(
    AxumPath((kind, key)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let place = place_of(&state, kind_of(&kind)?, &key).await?;
    Ok(Json(current(&state, &place).await))
}

/// `PUT /api/item-labels/<kind>/<id>`: change an item's labels (`labels`, `add`, `remove`).
pub(crate) async fn api_item_put(
    AxumPath((kind, key)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(change): Json<LabelChange>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let labels = change_item(&state, kind_of(&kind)?, &key, &change).await?;
    Ok(Json(labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Times are offsets from now: tombstones older than the retention are pruned.
    fn at(n: u64) -> u64 {
        static BASE: std::sync::LazyLock<u64> = std::sync::LazyLock::new(now_ms);
        *BASE + n * 1000
    }
    fn def(n: u64) -> LabelDef {
        LabelDef {
            icon: None,
            updated_at: at(n),
        }
    }
    fn tomb(n: u64, to: Option<&str>) -> Tombstone {
        Tombstone {
            at: at(n),
            renamed_to: to.map(str::to_string),
        }
    }
    fn reg(defs: &[(&str, u64)], dead: &[(&str, u64)]) -> Registry {
        Registry {
            defs: defs.iter().map(|(n, a)| (n.to_string(), def(*a))).collect(),
            deleted: dead
                .iter()
                .map(|(n, a)| (n.to_string(), tomb(*a, None)))
                .collect(),
        }
    }

    #[test]
    fn rewrite_removes_or_replaces() {
        let mut l = vec!["a".to_string(), "b".to_string()];
        assert!(rewrite(&mut l, "a", None));
        assert_eq!(l, ["b"]);
        assert!(rewrite(&mut l, "b", Some("c")));
        assert_eq!(l, ["c"]);
        assert!(!rewrite(&mut l, "zzz", Some("d")));
        let mut l = vec!["a".to_string(), "b".to_string()];
        assert!(rewrite(&mut l, "a", Some("b")));
        assert_eq!(l, ["b"]);
    }

    #[test]
    fn the_newer_definition_wins() {
        let mut ours = reg(&[("ha", 10)], &[]);
        let m = ours.merge(&reg(&[("ha", 20), ("work", 5)], &[]));
        assert!(m.changed && m.cascades.is_empty());
        assert_eq!(ours.defs["ha"].updated_at, at(20));
        assert!(ours.defs.contains_key("work"));
        assert!(!ours.merge(&reg(&[("ha", 20)], &[])).changed);
    }

    #[test]
    fn a_tombstone_beats_an_older_definition_and_cascades_once() {
        let mut ours = reg(&[("ha", 10)], &[]);
        let theirs = reg(&[], &[("ha", 20)]);
        let m = ours.merge(&theirs);
        assert_eq!(m.cascades, [("ha".to_string(), None)]);
        assert!(ours.defs.is_empty());
        // Merging it again changes nothing.
        assert_eq!(ours.merge(&theirs), Merged::default());
    }

    #[test]
    fn a_label_defined_after_its_deletion_comes_back_everywhere() {
        let mut stale = reg(&[], &[("ha", 20)]);
        let m = stale.merge(&reg(&[("ha", 30)], &[]));
        assert!(m.changed && m.cascades.is_empty());
        assert!(stale.deleted.is_empty() && stale.defs.contains_key("ha"));
        // ... while a definition older than the tombstone does not.
        let mut stale = reg(&[], &[("ha", 20)]);
        assert!(!stale.merge(&reg(&[("ha", 10)], &[])).changed);
    }

    #[test]
    fn a_rename_cascades_to_the_new_name() {
        let mut ours = reg(&[("old", 1)], &[]);
        let mut theirs = reg(&[("new", 9)], &[]);
        theirs.deleted.insert("old".into(), tomb(9, Some("new")));
        let m = ours.merge(&theirs);
        assert_eq!(m.cascades, [("old".to_string(), Some("new".to_string()))]);
        assert!(ours.defs.contains_key("new") && !ours.defs.contains_key("old"));
    }

    #[test]
    fn newer_than_is_what_the_peer_lacks() {
        let ours = reg(&[("a", 10), ("b", 10)], &[("c", 10)]);
        let theirs = reg(&[("a", 10), ("b", 5), ("c", 4)], &[]).digest();
        let delta = ours.newer_than(&theirs);
        assert_eq!(delta.defs.keys().collect::<Vec<_>>(), ["b"]);
        assert_eq!(delta.deleted.keys().collect::<Vec<_>>(), ["c"]);
        assert!(ours.newer_than(&ours.digest()).is_empty());
    }

    #[test]
    fn icons_are_checked() {
        assert!(LabelIcon::Mdi {
            name: "home-assistant".into()
        }
        .check()
        .is_ok());
        assert!(LabelIcon::Mdi {
            name: "Home Assistant".into()
        }
        .check()
        .is_err());
        assert!(LabelIcon::Image {
            mime: "image/png".into(),
            data: "AAAA".into()
        }
        .check()
        .is_ok());
        assert!(LabelIcon::Image {
            mime: "text/html".into(),
            data: "AAAA".into()
        }
        .check()
        .is_err());
        assert!(LabelIcon::Image {
            mime: "image/png".into(),
            data: "!!".into()
        }
        .check()
        .is_err());
    }

    #[test]
    fn a_change_replaces_then_adds_then_removes() {
        let c = LabelChange {
            labels: Some(vec!["b".into(), "a".into()]),
            add: Some(vec!["C".into()]),
            remove: Some(vec!["a".into()]),
        };
        assert_eq!(c.apply(&["zzz".into()]).unwrap(), ["b", "c"]);
        let only_add = LabelChange {
            add: Some(vec!["x".into()]),
            ..Default::default()
        };
        assert_eq!(only_add.apply(&["a".into()]).unwrap(), ["a", "x"]);
    }
}
