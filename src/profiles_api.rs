//! Profile and backup handlers.

use super::*;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProfileInfo {
    /// Record the profile's sessions; the node's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    /// Their recordings take the sound; the node's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_audio: Option<bool>,
    /// Stable id; also the name of the profile's directory.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Display name.
    pub name: String,
    #[serde(default)]
    pub engine: BrowserEngine,
    /// Where the browser keeps this profile's data on its node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_dir: Option<String>,
    pub size_bytes: u64,
    pub is_active: bool,
    pub session_id: Option<Uuid>,
    pub created_at_secs: Option<u64>,
    pub modified_at_secs: Option<u64>,
    /// Proxy id new sessions of the profile use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Appearance of the profile's sessions.
    #[serde(default)]
    pub color_scheme: ColorScheme,
    /// Pages its sessions open with.
    #[serde(default)]
    pub tabs: Vec<String>,
    /// Changes whenever the profile's picture does; absent without one (`/profiles/{name}/icon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_rev: Option<u64>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ProfileDetails {
    #[serde(flatten)]
    pub profile: ProfileInfo,
    pub session: Option<SessionSummary>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateProfileRequest {
    pub name: String,
    pub clone_from: Option<String>,
    /// Engine of a new empty profile (a clone keeps its source's).
    #[serde(default)]
    pub engine: BrowserEngine,
    /// Proxy id new sessions of the profile use.
    pub proxy: Option<String>,
    /// Appearance of the profile's sessions (dark when unset).
    pub color_scheme: Option<ColorScheme>,
    /// Pages its sessions open with: a list, or addresses separated by spaces.
    #[serde(default, deserialize_with = "tabs_field")]
    pub tabs: Option<Vec<String>>,
    /// Record its sessions (the node's default when absent or null).
    #[serde(default)]
    pub record: Option<bool>,
    /// Its recordings take the sound (the node's default when absent or null).
    #[serde(default)]
    pub record_audio: Option<bool>,
}

/// Default tabs as a JSON list, or as one string of addresses separated by
/// whitespace or commas (what a prompt or `curl` gives).
pub(crate) fn tabs_field<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Tabs {
        List(Vec<String>),
        Text(String),
    }
    Ok(Option::<Tabs>::deserialize(d)?.map(|t| match t {
        Tabs::List(list) => list,
        Tabs::Text(text) => text
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect(),
    }))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UpdateProfileRequest {
    pub name: Option<String>,
    /// Proxy id new sessions use; "none" (or "") clears it.
    pub proxy: Option<String>,
    /// Appearance of new sessions; a running one keeps its own until restarted.
    pub color_scheme: Option<ColorScheme>,
    /// Pages new sessions open with; empty clears them.
    #[serde(default, deserialize_with = "tabs_field")]
    pub tabs: Option<Vec<String>>,
    /// Record new sessions (true/false), or null for the node's default.
    #[serde(default, deserialize_with = "nullable")]
    pub record: Option<Option<bool>>,
    /// Recordings take the sound (true/false), or null for the node's default.
    #[serde(default, deserialize_with = "nullable")]
    pub record_audio: Option<Option<bool>>,
}

/// A field that may be set to null (Some(None)) as well as left out (None).
pub(crate) fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CloneProfileRequest {
    pub target_name: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct ClearProfileParams {
    pub force: Option<bool>,
    /// Take a safety backup first (the default).
    pub backup: Option<bool>,
}

#[derive(Deserialize, Debug, Default)]
pub struct CreateBackupRequest {
    pub label: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct DeleteProfileParams {
    pub force: Option<bool>,
}

/// Keep what a throwaway session has gathered (logins, cookies, storage) as a new
/// profile. The browser is quit properly so its data is on disk, the data copied,
/// and a session of the new profile opens the same pages in its place.
pub(crate) async fn api_session_save_as_profile(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SaveProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.name)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    if let Some(ref p) = session.profile {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("The session already uses the profile '{p}'"),
        ));
    }
    ensure_name_free(&state, &name, None).await?;

    let urls: Vec<String> = session_targets(&state, &session)
        .await
        .into_iter()
        .filter(|t| {
            t.target_type == "page"
                && (t.url.starts_with("http://") || t.url.starts_with("https://"))
        })
        .map(|t| t.url)
        .collect();
    let recording = state.active_recordings.read().await.contains_key(&id);

    // The profile first: when it cannot be made, the browser is left alone.
    let profile = {
        let _guard = state.profiles_lock.lock().await;
        ensure_name_free(&state, &name, None).await?;
        profiles::create(&state.config.profiles_dir, &name, session.engine)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let meta = match keep_session_data(&session, &profile).await {
        Ok(meta) => meta,
        Err((err, quit)) => {
            let _ = fs::remove_dir_all(&profile.dir).await;
            // The browser has quit: the session is a shell without a browser.
            if quit {
                state.terminate_session(&id).await;
            }
            return Err(err);
        }
    };

    // The throwaway ends (its directory goes with it); the profile takes over.
    let (width, height) = session.window_size;
    let adblock = session.adblock;
    let color_scheme = session.color_scheme;
    state.terminate_session(&id).await;
    let new = state
        .spawn_session(
            session.engine,
            Some(&name),
            true,
            Some(width),
            Some(height),
            Some(recording),
            None,
            None,
            Some(adblock),
            None,
            Some(color_scheme),
            true,
            false,
            urls,
        )
        .await
        .map_err(|(status, msg)| api_error(status, msg))?;
    info!(profile = %name, from = %id, session_id = %new.id, "Session saved as a profile");
    let rec = state.active_recordings.read().await.get(&new.id).cloned();
    let summary = new.to_summary(&state.http_client, rec.as_ref()).await;
    Ok(Json(serde_json::json!({
        "profile": profile_info(&state, &meta_profile(&profile, meta)).await,
        "session": summary,
    })))
}

#[derive(Deserialize)]
pub(crate) struct SaveProfileRequest {
    pub(crate) name: String,
}

/// Quit a throwaway session's browser and copy its data into the profile. On
/// failure: the error, and whether the browser has quit by then.
pub(crate) async fn keep_session_data(
    session: &ManagedSession,
    profile: &Profile,
) -> Result<profiles::ProfileMeta, (ApiError, bool)> {
    // Quit the browser (data to disk), leaving its directory in place.
    let src = {
        let mut guard = session.browser.lock().await;
        let browser = guard.as_mut().ok_or_else(|| {
            (
                api_error(StatusCode::CONFLICT, "The session is not running"),
                false,
            )
        })?;
        let src = browser.temp_dir_path().cloned().ok_or_else(|| {
            (
                api_error(StatusCode::CONFLICT, "The session has no data to keep"),
                false,
            )
        })?;
        browser.quiesce().await;
        src
    };
    let failed = |msg: String| (api_error(StatusCode::INTERNAL_SERVER_ERROR, msg), true);

    let copied = tokio::process::Command::new("cp")
        .arg("-a")
        .arg("--")
        .arg(src.join("."))
        .arg(&profile.dir)
        .status()
        .await;
    if !matches!(copied, Ok(s) if s.success()) {
        return Err(failed("Could not copy the session's data".into()));
    }
    // What belongs to the run, not to the data.
    for leftover in [
        "SingletonLock",
        "SingletonSocket",
        "SingletonCookie",
        "DevToolsActivePort",
        "lock",
        ".parentlock",
        "parent.lock",
    ] {
        let _ = fs::remove_file(profile.dir.join(leftover)).await;
    }
    for cache in [
        "Default/Cache",
        "Default/Code Cache",
        "Default/GPUCache",
        "GrShaderCache",
        "ShaderCache",
        "component_crx_cache",
        "extensions_crx_cache",
        "cache2",
    ] {
        let _ = fs::remove_dir_all(profile.dir.join(cache)).await;
    }
    // The profile remembers the proxy and appearance the session had.
    let mut meta = profile.meta.clone();
    meta.proxy = session.relay.current();
    meta.color_scheme = Some(session.color_scheme);
    profiles::write_meta(&profile.dir, &meta)
        .await
        .map_err(|e| failed(e.to_string()))?;
    Ok(meta)
}

pub(crate) async fn profile_session(state: &AppState, id: Uuid) -> Option<ManagedSession> {
    let guard = state.sessions.read().await;
    guard.values().find(|s| s.profile_id == Some(id)).cloned()
}

pub(crate) async fn profile_info(state: &AppState, profile: &Profile) -> ProfileInfo {
    let session_id = profile_session(state, profile.meta.id).await.map(|s| s.id);
    let modified_at_secs = fs::metadata(&profile.dir)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    ProfileInfo {
        id: Some(profile.meta.id),
        name: profile.meta.name.clone(),
        engine: profile.meta.engine,
        storage_dir: Some(profile.dir.display().to_string()),
        size_bytes: state.dir_sizes.of(&profile.dir).await,
        is_active: session_id.is_some(),
        session_id,
        created_at_secs: Some(profile.meta.created_at_secs),
        modified_at_secs,
        proxy: profile.meta.proxy.clone(),
        color_scheme: profile.meta.color_scheme.unwrap_or_default(),
        tabs: profile.meta.tabs.clone(),
        record: profile.meta.record,
        record_audio: profile.meta.record_audio,
        icon_rev: profile.meta.icon.as_ref().map(|i| i.rev),
    }
}

pub(crate) async fn lookup_profile(state: &AppState, key: &str) -> Result<Profile, ApiError> {
    profiles::find(&state.config.profiles_dir, key)
        .await
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("Profile '{}' not found", key),
            )
        })
}

pub(crate) fn new_profile_name(name: &str) -> Result<String, ApiError> {
    profiles::clean_name(name)
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "Invalid profile name"))
}

pub(crate) async fn ensure_name_free(
    state: &AppState,
    name: &str,
    except: Option<Uuid>,
) -> Result<(), ApiError> {
    if profiles::name_taken(&state.config.profiles_dir, name, except).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile '{}' already exists", name),
        ));
    }
    Ok(())
}

/// Copy a profile's data into a new id directory under a new name.
pub(crate) async fn copy_profile(
    state: &AppState,
    source: &Profile,
    name: &str,
) -> Result<Profile, ApiError> {
    let dir = state.config.profiles_dir.join(Uuid::new_v4().to_string());
    if let Err(e) = copy_dir_all(source.dir.clone(), dir.clone()).await {
        error!(from = %source.meta.name, to = %name, error = %e, "Failed to copy profile directory");
        let _ = fs::remove_dir_all(&dir).await;
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to copy profile directory",
        ));
    }
    profiles::adopt(dir, name, source.meta.engine)
        .await
        .map_err(|e| {
            error!(profile = %name, error = %e, "Failed to write profile metadata");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write profile metadata",
            )
        })
}

pub(crate) async fn get_local_profiles(state: &AppState) -> Vec<ProfileInfo> {
    let profiles = profiles::list(&state.config.profiles_dir).await;
    futures_util::future::join_all(profiles.iter().map(|p| profile_info(state, p))).await
}

pub(crate) async fn api_list_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    state.check_auth(&headers)?;
    Ok(Json(get_local_profiles(&state).await))
}

pub(crate) async fn api_create_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.name)?;
    let proxy = req
        .proxy
        .as_deref()
        .map(|k| state.check_proxy(k))
        .transpose()?;
    let _guard = state.profiles_lock.lock().await;
    ensure_name_free(&state, &name, None).await?;

    let mut profile = match req.clone_from.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(source) => {
            let source = lookup_profile(&state, source).await?;
            copy_profile(&state, &source, &name).await?
        }
        None => profiles::create(&state.config.profiles_dir, &name, req.engine)
            .await
            .map_err(|e| {
                error!(profile = %name, error = %e, "Failed to create profile");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create profile",
                )
            })?,
    };

    let tabs = req.tabs.as_deref().map(tab_urls).transpose()?;
    if proxy.is_some()
        || req.color_scheme.is_some()
        || tabs.is_some()
        || req.record.is_some()
        || req.record_audio.is_some()
    {
        if let Some(proxy) = proxy {
            profile.meta.proxy = proxy;
        }
        if req.record.is_some() {
            profile.meta.record = req.record;
        }
        if req.record_audio.is_some() {
            profile.meta.record_audio = req.record_audio;
        }
        if req.color_scheme.is_some() {
            profile.meta.color_scheme = req.color_scheme;
        }
        if let Some(tabs) = tabs {
            profile.meta.tabs = tabs;
        }
        profiles::write_meta(&profile.dir, &profile.meta)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    info!(profile = %name, id = %profile.meta.id, "Profile created");
    Ok((
        StatusCode::CREATED,
        Json(profile_info(&state, &profile).await),
    ))
}

pub(crate) async fn api_get_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let session = match profile_session(&state, profile.meta.id).await {
        Some(s) => {
            let rec = state.active_recordings.read().await.get(&s.id).cloned();
            Some(s.to_summary(&state.http_client, rec.as_ref()).await)
        }
        None => None,
    };
    Ok(Json(ProfileDetails {
        profile: profile_info(&state, &profile).await,
        session,
    }))
}

/// Rename a profile. Its directory is named by id, so a running session keeps going.
/// Default tabs as given: each an http(s) address (a bare host gets https://);
/// empty entries are dropped, repeats too.
pub(crate) fn tab_urls(input: &[String]) -> Result<Vec<String>, ApiError> {
    let mut out: Vec<String> = Vec::new();
    for entry in input {
        if let Some(url) = tab_url(entry)? {
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    Ok(out)
}

/// One default tab as given: an http(s) address (a bare host gets https://);
/// empty is none.
pub(crate) fn tab_url(input: &str) -> Result<Option<String>, ApiError> {
    let v = input.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let full = if v.contains("://") {
        v.to_string()
    } else {
        format!("https://{v}")
    };
    match url::Url::parse(&full) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
            Ok(Some(u.to_string()))
        }
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "A default tab must be an http(s) address",
        )),
    }
}

pub(crate) async fn api_update_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let mut profile = lookup_profile(&state, &key).await?;
    if let Some(ref requested) = req.name {
        let name = new_profile_name(requested)?;
        if name != profile.meta.name {
            ensure_name_free(&state, &name, Some(profile.meta.id)).await?;
            let old = std::mem::replace(&mut profile.meta.name, name.clone());
            if let Err(e) = profiles::write_meta(&profile.dir, &profile.meta).await {
                error!(profile = %old, error = %e, "Failed to write profile metadata");
                return Err(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to rename profile",
                ));
            }
            for session in state.sessions.write().await.values_mut() {
                if session.profile_id == Some(profile.meta.id) {
                    session.profile = Some(name.clone());
                }
            }
            info!(from = %old, to = %name, id = %profile.meta.id, "Profile renamed");
        }
    }
    if let Some(ref requested) = req.proxy {
        let proxy = state.check_proxy(requested)?;
        if proxy != profile.meta.proxy {
            profile.meta.proxy = proxy;
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            // A running session keeps its proxy until it is restarted.
            info!(profile = %profile.meta.name, proxy = ?profile.meta.proxy, "Profile proxy changed");
        }
    }
    if let Some(scheme) = req.color_scheme {
        if Some(scheme) != profile.meta.color_scheme {
            profile.meta.color_scheme = Some(scheme);
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            info!(profile = %profile.meta.name, color_scheme = ?scheme, "Profile appearance changed");
        }
    }
    if let Some(ref requested) = req.tabs {
        let tabs = tab_urls(requested)?;
        if tabs != profile.meta.tabs {
            profile.meta.tabs = tabs;
            profiles::write_meta(&profile.dir, &profile.meta)
                .await
                .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            info!(profile = %profile.meta.name, tabs = ?profile.meta.tabs, "Profile default tabs changed");
        }
    }
    let (record, record_audio) = (
        req.record.unwrap_or(profile.meta.record),
        req.record_audio.unwrap_or(profile.meta.record_audio),
    );
    if (record, record_audio) != (profile.meta.record, profile.meta.record_audio) {
        profile.meta.record = record;
        profile.meta.record_audio = record_audio;
        profiles::write_meta(&profile.dir, &profile.meta)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // A running session keeps recording as it does until it is restarted.
        info!(profile = %profile.meta.name, record = ?record, record_audio = ?record_audio, "Profile recording settings changed");
    }
    Ok(Json(profile_info(&state, &profile).await))
}

/// The profile's picture.
pub(crate) async fn api_get_profile_icon(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = state.check_auth(&headers) {
        return e.into_response();
    }
    let Ok(profile) = lookup_profile(&state, &key).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((mime, bytes)) = profile
        .meta
        .icon
        .and_then(|i| i.bytes().map(|b| (i.mime, b)))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
            // An SVG here is only ever shown as an image, but nothing in it may run.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Set the profile's picture: the image as the request body, its type as Content-Type.
pub(crate) async fn api_set_profile_icon(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let mut profile = lookup_profile(&state, &key).await?;
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let icon = profiles::ProfileIcon::new(mime, &body).ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "The picture must be a png, svg, webp, jpeg or gif of at most {} KB",
                profiles::MAX_ICON_BYTES / 1024
            ),
        )
    })?;
    profile.meta.icon = Some(icon);
    profiles::write_meta(&profile.dir, &profile.meta)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(profile = %profile.meta.name, "Profile picture set");
    Ok(Json(profile_info(&state, &profile).await))
}

pub(crate) async fn api_delete_profile_icon(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let mut profile = lookup_profile(&state, &key).await?;
    if profile.meta.icon.take().is_some() {
        profiles::write_meta(&profile.dir, &profile.meta)
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        info!(profile = %profile.meta.name, "Profile picture removed");
    }
    Ok(Json(profile_info(&state, &profile).await))
}

pub(crate) async fn api_clone_profile(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CloneProfileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let name = new_profile_name(&req.target_name)?;
    let _guard = state.profiles_lock.lock().await;
    let source = lookup_profile(&state, &key).await?;
    ensure_name_free(&state, &name, None).await?;
    let profile = copy_profile(&state, &source, &name).await?;
    info!(from = %source.meta.name, to = %name, "Profile cloned");
    Ok((
        StatusCode::CREATED,
        Json(profile_info(&state, &profile).await),
    ))
}

pub(crate) async fn api_delete_profile(
    AxumPath(key): AxumPath<String>,
    Query(params): Query<DeleteProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    // Nothing starts a session of the profile between the check and the removal.
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    release_profile(&state, &profile, params.force.unwrap_or(false), "delete").await?;

    state.dir_sizes.forget(&profile.dir);
    if let Err(e) = fs::remove_dir_all(&profile.dir).await {
        error!(profile = %profile.meta.name, error = %e, "Failed to remove profile directory");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to remove profile directory",
        ));
    }

    backups::remove_all(&state.config.profiles_dir, profile.meta.id).await;
    state.history.delete(&profile.meta.id.to_string()).await;
    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile removed");
    Ok((
        StatusCode::OK,
        Json(
            serde_json::json!({ "status": "deleted", "profile": profile.meta.name, "id": profile.meta.id }),
        ),
    ))
}

/// What holds a profile.
pub(crate) enum ProfileUse {
    Session(Uuid),
    Starting,
}

pub(crate) async fn profile_use(state: &AppState, id: Uuid) -> Option<ProfileUse> {
    if let Some(session) = profile_session(state, id).await {
        return Some(ProfileUse::Session(session.id));
    }
    state
        .starting
        .has_profile(id)
        .then_some(ProfileUse::Starting)
}

/// Make sure no browser holds the profile: refuse while a session uses it,
/// unless `force` stops that session. The caller holds the profile's lock, so no
/// session is admitted meanwhile.
pub(crate) async fn release_profile(
    state: &AppState,
    profile: &Profile,
    force: bool,
    action: &str,
) -> Result<(), ApiError> {
    let session = match profile_use(state, profile.meta.id).await {
        None => return Ok(()),
        Some(ProfileUse::Starting) => {
            return Err(api_error(
                StatusCode::CONFLICT,
                format!("A session of the profile is starting. Try again to {action}."),
            ))
        }
        Some(ProfileUse::Session(id)) => id,
    };
    if !force {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("Profile is currently in use by active session {session}. Pass ?force=true to terminate session and {action}."),
        ));
    }
    info!(profile = %profile.meta.name, session_id = %session, "Force terminating session before {action}");
    state.terminate_session(&session).await;
    Ok(())
}

/// Back the profile up before a destructive action, keeping only the newest few.
pub(crate) async fn auto_backup(
    state: &AppState,
    profile: &Profile,
    what: &str,
) -> Result<(), ApiError> {
    let dir = &state.config.profiles_dir;
    backups::create(
        dir,
        profile,
        Some(&format!("{}{what}", backups::AUTO_PREFIX)),
    )
    .await
    .map_err(|e| {
        error!(profile = %profile.meta.name, error = %e, "Safety backup failed");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Safety backup failed, nothing was changed: {e}"),
        )
    })?;
    backups::prune_auto(dir, profile.meta.id).await;
    Ok(())
}

pub(crate) async fn api_list_backups(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    Ok(Json(
        backups::list(&state.config.profiles_dir, profile.meta.id).await,
    ))
}

pub(crate) async fn api_create_backup(
    AxumPath(key): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateBackupRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    // A session must not start on the profile while it is archived.
    let _guard = state.profiles_lock.lock().await;
    let profile = lookup_profile(&state, &key).await?;
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    if let Some(used) = profile_use(&state, profile.meta.id).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            match used {
                ProfileUse::Session(id) => {
                    format!("Cannot back up an active profile (in use by session {id})")
                }
                ProfileUse::Starting => "Cannot back up a profile that is starting".into(),
            },
        ));
    }
    let label = body.and_then(|Json(b)| b.label);
    let info = backups::create(&state.config.profiles_dir, &profile, label.as_deref())
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, error = %e, "Backup failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
    info!(profile = %profile.meta.name, backup = %info.id, size_bytes = info.size_bytes, "Profile backed up");
    Ok((StatusCode::CREATED, Json(info)))
}

pub(crate) async fn api_delete_backup(
    AxumPath((key, backup)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    if !backups::delete(&state.config.profiles_dir, profile.meta.id, &backup).await {
        return Err(api_error(StatusCode::NOT_FOUND, "Backup not found"));
    }
    Ok(Json(
        serde_json::json!({ "status": "deleted", "backup": backup }),
    ))
}

pub(crate) async fn api_restore_backup(
    AxumPath((key, backup)): AxumPath<(String, String)>,
    Query(params): Query<DeleteProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let _guard = state.profiles_lock.lock().await;
    let profile = lookup_profile(&state, &key).await?;
    let dir = &state.config.profiles_dir;
    if backups::get(dir, profile.meta.id, &backup).await.is_none() {
        return Err(api_error(StatusCode::NOT_FOUND, "Backup not found"));
    }
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    release_profile(&state, &profile, params.force.unwrap_or(false), "restore").await?;
    auto_backup(&state, &profile, "pre-restore").await?;
    backups::restore(dir, &profile, &backup)
        .await
        .map_err(|e| {
            error!(profile = %profile.meta.name, backup = %backup, error = %e, "Restore failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        })?;
    state.dir_sizes.forget(&profile.dir);
    info!(profile = %profile.meta.name, backup = %backup, "Profile restored from backup");
    Ok(Json(profile_info(&state, &profile).await))
}

/// Wipe a profile's cookies, logins and storage but keep the profile itself
/// (its name, engine, proxy, appearance and homepage).
pub(crate) async fn api_clear_profile(
    AxumPath(key): AxumPath<String>,
    Query(params): Query<ClearProfileParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let profile = lookup_profile(&state, &key).await?;
    let _lock = state.profile_locks.lock(profile.meta.id).await;
    release_profile(&state, &profile, params.force.unwrap_or(false), "clear").await?;
    // A safety net: clearing cannot be undone otherwise.
    if params.backup.unwrap_or(true) {
        auto_backup(&state, &profile, "pre-clear").await?;
    }

    let io_err = |e: std::io::Error| {
        error!(profile = %profile.meta.name, error = %e, "Failed to clear profile data");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to clear profile data: {e}"),
        )
    };
    let mut entries = fs::read_dir(&profile.dir).await.map_err(io_err)?;
    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        if entry.file_name() == profiles::META_FILE {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().await.map_err(io_err)?.is_dir();
        if is_dir {
            fs::remove_dir_all(&path).await.map_err(io_err)?;
        } else {
            fs::remove_file(&path).await.map_err(io_err)?;
        }
    }

    state.dir_sizes.forget(&profile.dir);
    info!(profile = %profile.meta.name, id = %profile.meta.id, "Profile data cleared");
    Ok(Json(profile_info(&state, &profile).await))
}

/// Why a profile cannot be exported or moved right now.
pub(crate) async fn ensure_profile_idle(
    state: &AppState,
    profile: &Profile,
    what: &str,
) -> Result<(), ApiError> {
    match profile_use(state, profile.meta.id).await {
        None => Ok(()),
        Some(ProfileUse::Session(id)) => Err(api_error(
            StatusCode::CONFLICT,
            format!("Cannot {what} active profile (in use by session {id})"),
        )),
        Some(ProfileUse::Starting) => Err(api_error(
            StatusCode::CONFLICT,
            format!("Cannot {what} a profile that is starting"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tabs_come_as_a_list_or_as_words() {
        let req = |json: &str| {
            serde_json::from_str::<UpdateProfileRequest>(json)
                .unwrap()
                .tabs
        };
        assert_eq!(
            req(r#"{"tabs":["a.test","b.test"]}"#),
            Some(vec!["a.test".into(), "b.test".into()])
        );
        assert_eq!(
            req(r#"{"tabs":"a.test  b.test,c.test"}"#).map(|t| t.len()),
            Some(3)
        );
        assert_eq!(req(r#"{"tabs":[]}"#), Some(vec![]));
        assert_eq!(req(r#"{}"#), None);
        let urls = tab_urls(&[
            "outlook.office.com".into(),
            "".into(),
            "https://outlook.office.com/".into(),
        ])
        .unwrap();
        assert_eq!(urls, vec!["https://outlook.office.com/".to_string()]);
        assert!(tab_urls(&["ftp://x.test/".into()]).is_err());
    }

    #[test]
    fn profile_recording_settings_can_be_left_set_or_cleared() {
        let req: UpdateProfileRequest = serde_json::from_str(r#"{"record_audio": false}"#).unwrap();
        assert_eq!((req.record, req.record_audio), (None, Some(Some(false))));
        let req: UpdateProfileRequest = serde_json::from_str(r#"{"record": null}"#).unwrap();
        assert_eq!(req.record, Some(None));
    }
}
