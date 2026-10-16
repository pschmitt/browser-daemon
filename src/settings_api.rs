//! Node settings and proxy listing handlers.

use super::*;

#[derive(Deserialize)]
pub(crate) struct UpdateSettingsRequest {
    /// Proxy id; "none" or "" for direct. Absent: unchanged.
    #[serde(default)]
    pub(crate) default_proxy: Option<String>,
    /// Engine defaults, each engine replaced as a whole. Absent: unchanged.
    #[serde(default)]
    pub(crate) engines: Option<EnginesPatch>,
}

/// The engines of an `UpdateSettingsRequest` that change; one left out stays.
#[derive(Deserialize)]
pub(crate) struct EnginesPatch {
    #[serde(default)]
    pub(crate) chromium: Option<settings::EngineDefaults>,
    #[serde(default)]
    pub(crate) firefox: Option<settings::EngineDefaults>,
}

pub(crate) async fn api_get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(state.settings.get()))
}

pub(crate) async fn api_update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let settings = state
        .settings
        .update(|settings| {
            if let Some(ref key) = req.default_proxy {
                settings.default_proxy = if key.trim().is_empty() {
                    None
                } else {
                    state.check_proxy(key)?
                };
            }
            if let Some(engines) = req.engines {
                if let Some(chromium) = engines.chromium {
                    settings.engines.chromium = chromium;
                }
                if let Some(firefox) = engines.firefox {
                    settings.engines.firefox = firefox;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            settings::UpdateError::Rejected(e) => e,
            settings::UpdateError::Io(e) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not save the settings: {e}"),
            ),
        })?;
    info!(settings = ?settings, "Node settings changed");
    Ok(Json(settings))
}

// ---------------------------------------------------------------------------
// Recording & Auto-Vacuum APIs
// ---------------------------------------------------------------------------

pub(crate) async fn api_list_proxies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(state.proxy_infos()))
}
