//! Node settings and proxy listing handlers.

use super::*;

#[derive(Deserialize)]
pub(crate) struct UpdateSettingsRequest {
    /// Proxy id; "none" or "" for direct. Absent: unchanged.
    #[serde(default)]
    pub(crate) default_proxy: Option<String>,
    /// Runtime node groups, replaced as a whole; those from the startup flags
    /// are immutable and left out. Absent: unchanged.
    #[serde(default)]
    pub(crate) groups: Option<Vec<String>>,
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

const MAX_GROUPS: usize = 16;
const MAX_GROUP_LEN: usize = 64;

/// Trimmed, unique, without the startup-flag groups; names are plain labels.
fn normalize_groups(groups: &[String], fixed: &[String]) -> Result<Vec<String>, ApiError> {
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        let g = g.trim();
        if g.is_empty() || fixed.iter().any(|f| f == g) || out.iter().any(|o| o == g) {
            continue;
        }
        if g.len() > MAX_GROUP_LEN
            || !g
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("Invalid group name {g:?}: use letters, digits, - _ . : (up to {MAX_GROUP_LEN})"),
            ));
        }
        out.push(g.to_string());
    }
    if out.len() > MAX_GROUPS {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("At most {MAX_GROUPS} runtime groups"),
        ));
    }
    Ok(out)
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
            if let Some(ref groups) = req.groups {
                settings.groups = normalize_groups(groups, &state.node_groups)?;
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

#[cfg(test)]
mod tests {
    use super::normalize_groups;

    #[test]
    fn runtime_groups_skip_fixed_and_duplicates() {
        let fixed = vec!["cloud".to_string()];
        let got = normalize_groups(
            &[" gpu ".into(), "cloud".into(), "gpu".into(), "".into()],
            &fixed,
        )
        .unwrap();
        assert_eq!(got, vec!["gpu"]);
        assert!(normalize_groups(&["a b".into()], &fixed).is_err());
    }
}
