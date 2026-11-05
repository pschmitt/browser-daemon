//! Generic login automation for an existing Chromium session tab.

use super::*;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 90;

/// Credential-bearing input intentionally has no `Debug` implementation.
#[derive(Deserialize)]
pub(crate) struct LoginRequest {
    pub(crate) tab: String,
    pub(crate) username: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) totp_secret: Option<String>,
    #[serde(default)]
    pub(crate) totp_code: Option<String>,
    /// Exact HTTP(S) origins that may receive credentials. Defaults to the tab's
    /// origin when this call starts.
    #[serde(default)]
    pub(crate) allowed_origins: Vec<String>,
    /// A visible element that is explicit evidence of successful login.
    #[serde(default)]
    pub(crate) success_selector: Option<String>,
    #[serde(default)]
    pub(crate) timeout_seconds: Option<u64>,
}

impl LoginRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if self.tab.trim().is_empty() || self.tab.len() > 256 {
            return Err("tab is required and must be at most 256 bytes");
        }
        if self.username.is_empty()
            || self.username.len() > 4096
            || self.password.is_empty()
            || self.password.len() > 4096
        {
            return Err("username and password are required and must be at most 4096 bytes");
        }
        if self.totp_secret.as_ref().is_some_and(|s| s.len() > 256) {
            return Err("totp_secret must be at most 256 bytes");
        }
        if self.totp_secret.is_some() && self.totp_code.is_some() {
            return Err("give totp_secret or totp_code, not both");
        }
        if self
            .totp_code
            .as_ref()
            .is_some_and(|c| !c.chars().all(|c| c.is_ascii_digit()) || !(6..=8).contains(&c.len()))
        {
            return Err("totp_code must contain 6 to 8 digits");
        }
        if self
            .success_selector
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 512)
        {
            return Err("success_selector must be a non-empty selector of at most 512 bytes");
        }
        if self.allowed_origins.len() > 10
            || self
                .allowed_origins
                .iter()
                .any(|o| parse_origin(o).is_none())
        {
            return Err("allowed_origins must contain at most 10 HTTP(S) origins");
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        )
    }
}

fn parse_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn origin_allowed(value: &str, allowed: &[String]) -> bool {
    parse_origin(value).is_some_and(|origin| allowed.iter().any(|allowed| allowed == &origin))
}

fn totp(secret: &str, unix_secs: u64) -> Result<String, &'static str> {
    let normalized: String = secret
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-' && *c != '=')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let key = data_encoding::BASE32_NOPAD
        .decode(normalized.as_bytes())
        .map_err(|_| "invalid TOTP seed")?;
    if key.is_empty() {
        return Err("invalid TOTP seed");
    }
    let counter = (unix_secs / 30).to_be_bytes();
    let mut mac = HmacSha1::new_from_slice(&key).map_err(|_| "invalid TOTP seed")?;
    mac.update(&counter);
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let binary = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    Ok(format!("{:06}", binary % 1_000_000))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn login_url_allowed(url: &str, allowed: &[String]) -> bool {
    origin_allowed(url, allowed)
}

fn status_outcome(state: &serde_json::Value, submitted: bool) -> Option<&'static str> {
    if state["success"].as_bool() == Some(true) {
        Some("authenticated")
    } else if state["totp"].as_bool() == Some(true) {
        Some("needs_totp")
    } else if state["interaction"].as_bool() == Some(true) {
        Some("needs_interaction")
    } else if submitted && state["error"].as_bool() == Some(true) {
        Some("failed")
    } else {
        None
    }
}

async fn within_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl Future<Output = Result<T, String>>,
) -> Option<Result<T, String>> {
    tokio::time::timeout(
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        work,
    )
    .await
    .ok()
}

fn answer(outcome: &'static str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "outcome": outcome }))
}

pub(crate) async fn api_session_login(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if state.cluster_secret.is_none() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "Login needs a cluster secret on the node",
        ));
    }
    if state.auth.credential(&headers, None, auth::now()) != Some(auth::Credential::Secret) {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "This operation needs the cluster secret",
        ));
    }
    req.validate()
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;

    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    if session.bidi.is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "Login automation currently requires a Chromium session",
        ));
    }
    let targets = get_local_targets(&state).await;
    let page = targets
        .iter()
        .find(|t| t.id == req.tab && t.session_id == Some(id) && t.target_type == "page")
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Tab not found in this session"))?;
    let initial_origin = parse_origin(&page.url)
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "Tab is not on an HTTP(S) page"))?;
    let allowed = if req.allowed_origins.is_empty() {
        vec![initial_origin.clone()]
    } else {
        req.allowed_origins
            .iter()
            .filter_map(|o| parse_origin(o))
            .collect()
    };
    if !allowed.iter().any(|o| o == &initial_origin) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "The tab origin is not in allowed_origins",
        ));
    }

    session.activity.touch();
    let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, req.tab);
    let deadline = tokio::time::Instant::now() + req.timeout();
    let mut username_submitted = false;
    let mut password_submitted = false;
    let mut totp_submitted = false;
    let mut submitted = false;
    loop {
        if tokio::time::Instant::now() >= deadline {
            let outcome = "unknown";
            info!(session_id = %id, outcome, "Login attempt ended");
            return Ok(answer(outcome));
        }
        let targets = get_local_targets(&state).await;
        let Some(page) = targets
            .iter()
            .find(|t| t.id == req.tab && t.session_id == Some(id) && t.target_type == "page")
        else {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                "Tab not found in this session",
            ));
        };
        if !login_url_allowed(&page.url, &allowed) {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                "Tab redirected outside allowed_origins",
            ));
        }
        let Some(page_state) = within_deadline(
            deadline,
            autofill::login_state_chromium(&ws_url, req.success_selector.as_deref(), |url| {
                login_url_allowed(url, &allowed)
            }),
        )
        .await
        else {
            return Ok(answer("unknown"));
        };
        let page_state = page_state
            .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not inspect the login page"))?;
        if let Some(outcome) = status_outcome(&page_state, submitted) {
            if outcome == "needs_totp" && !totp_submitted {
                let code = if let Some(code) = req.totp_code.as_deref() {
                    Some(code.to_string())
                } else if let Some(seed) = req.totp_secret.as_deref() {
                    Some(
                        totp(seed, now_secs())
                            .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?,
                    )
                } else {
                    None
                };
                let Some(code) = code else {
                    info!(session_id = %id, outcome, "Login attempt ended");
                    return Ok(answer(outcome));
                };
                let creds = autofill::Creds {
                    totp: Some(code),
                    mask_totp: true,
                    ..Default::default()
                };
                let Some(filled) = within_deadline(
                    deadline,
                    autofill::fill_chromium(
                        &ws_url,
                        |url| login_url_allowed(url, &allowed),
                        &creds,
                    ),
                )
                .await
                else {
                    return Ok(answer("unknown"));
                };
                let filled = filled.map_err(|_| {
                    api_error(StatusCode::BAD_GATEWAY, "Could not fill the login page")
                })?;
                if filled.contains(&"totp".to_string()) {
                    let Some(submit) =
                        within_deadline(deadline, autofill::login_submit_chromium(&ws_url)).await
                    else {
                        return Ok(answer("unknown"));
                    };
                    submit.map_err(|_| {
                        api_error(StatusCode::BAD_GATEWAY, "Could not submit the login page")
                    })?;
                    totp_submitted = true;
                    submitted = true;
                } else {
                    info!(session_id = %id, outcome = "needs_totp", "Login attempt ended");
                    return Ok(answer("needs_totp"));
                }
            } else {
                info!(session_id = %id, outcome, "Login attempt ended");
                return Ok(answer(outcome));
            }
        } else if page_state["password"].as_bool() == Some(true) && !password_submitted {
            let creds = autofill::Creds {
                username: Some(req.username.clone()),
                password: Some(req.password.clone()),
                ..Default::default()
            };
            let Some(filled) = within_deadline(
                deadline,
                autofill::fill_chromium(&ws_url, |url| login_url_allowed(url, &allowed), &creds),
            )
            .await
            else {
                return Ok(answer("unknown"));
            };
            let filled = filled
                .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not fill the login page"))?;
            if filled.contains(&"password".to_string()) {
                let Some(submit) =
                    within_deadline(deadline, autofill::login_submit_chromium(&ws_url)).await
                else {
                    return Ok(answer("unknown"));
                };
                submit.map_err(|_| {
                    api_error(StatusCode::BAD_GATEWAY, "Could not submit the login page")
                })?;
                password_submitted = true;
                submitted = true;
            }
        } else if page_state["username"].as_bool() == Some(true)
            && !username_submitted
            && !password_submitted
        {
            let creds = autofill::Creds {
                username: Some(req.username.clone()),
                ..Default::default()
            };
            let Some(filled) = within_deadline(
                deadline,
                autofill::fill_chromium(&ws_url, |url| login_url_allowed(url, &allowed), &creds),
            )
            .await
            else {
                return Ok(answer("unknown"));
            };
            let filled = filled
                .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not fill the login page"))?;
            if filled.contains(&"username".to_string()) {
                let Some(submit) =
                    within_deadline(deadline, autofill::login_submit_chromium(&ws_url)).await
                else {
                    return Ok(answer("unknown"));
                };
                submit.map_err(|_| {
                    api_error(StatusCode::BAD_GATEWAY, "Could not submit the login page")
                })?;
                username_submitted = true;
                submitted = true;
            }
        } else if page_state["interaction"].as_bool() == Some(true) {
            info!(session_id = %id, outcome = "needs_interaction", "Login attempt ended");
            return Ok(answer("needs_interaction"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp_matches_rfc_6238_sha1_six_digit_vector() {
        assert_eq!(
            totp("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59).unwrap(),
            "287082"
        );
        assert_eq!(
            totp("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ====", 59).unwrap(),
            "287082"
        );
    }

    #[test]
    fn origins_are_exact_and_reject_non_web_urls() {
        let allowed = vec!["https://login.example.test".to_string()];
        assert!(origin_allowed("https://login.example.test/path", &allowed));
        assert!(!origin_allowed("https://evil.example.test/", &allowed));
        assert!(parse_origin("javascript:alert(1)").is_none());
    }

    #[test]
    fn only_explicit_success_evidence_claims_authentication() {
        assert_eq!(
            status_outcome(&serde_json::json!({"success":true}), true),
            Some("authenticated")
        );
        assert_eq!(
            status_outcome(&serde_json::json!({"password":false}), true),
            None
        );
        assert_eq!(
            status_outcome(&serde_json::json!({"totp":true}), true),
            Some("needs_totp")
        );
    }
}
