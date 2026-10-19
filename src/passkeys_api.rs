//! Passkey, autofill and passkey-helper handlers.

use super::*;

#[derive(Deserialize)]
pub(crate) struct PasskeyRequest {
    pub(crate) page_id: String,
    pub(crate) rp_id: String,
    /// true: only look up which vault entry would be used (or that it's locked).
    #[serde(default)]
    pub(crate) find: bool,
    /// Vault entry id, when several entries hold a passkey for the site.
    #[serde(default)]
    pub(crate) entry: Option<String>,
    /// Sign the request's challenge (base64url) instead of injecting the passkey
    /// (Firefox: the page answers navigator.credentials.get() with the result).
    #[serde(default)]
    pub(crate) challenge: Option<String>,
    #[serde(default)]
    pub(crate) allow: Vec<String>,
}

/// A passkey approved in the live view: the helper puts it into the page.
/// Answers right away with the request's id; its progress is at /api/passkey/{id}.
pub(crate) async fn api_session_passkey(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PasskeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let session = state
        .sessions
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    let kind = match (req.find, &req.challenge) {
        (true, _) => "find",
        (false, Some(_)) => "assert",
        (false, None) => "inject",
    };
    // The page comes from the browser's own page list, never from what the caller
    // says, and must be one of this session's. The origin to sign for and the rp_id
    // follow from it: a page must not obtain an assertion for another site.
    let targets = get_local_targets(&state).await;
    let page = targets
        .iter()
        .find(|t| t.id == req.page_id && t.session_id == Some(id))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found in this session"))?;
    let page_url = url::Url::parse(&page.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page has no web origin"))?;
    let rp_id = req.rp_id.trim().trim_end_matches('.').to_ascii_lowercase();
    if !secrets::rp_id_valid_for(page_url.host_str().unwrap_or_default(), &rp_id) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The page is not on {rp_id} or a subdomain of it"),
        ));
    }
    let origin = (kind == "assert").then(|| page_url.origin().ascii_serialization());
    if !req.find {
        info!(session_id = %id, rp_id = %rp_id, kind, "Passkey approved in the live view");
    }
    let signing = passkeys::Signing {
        origin,
        challenge: req.challenge,
        allow: req.allow,
    };
    let request = state
        .passkeys
        .start(req.page_id, rp_id, kind, req.entry, signing)
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(serde_json::json!({ "request": request })))
}

/// A passkey approved in the live view of a shared browser's tab (with control):
/// the helper signs the page's challenge and the page's hook answers with it (the
/// browser's own authenticator is never touched). The origin and site come from the
/// tab's address as the browser reports it.
pub(crate) async fn api_extension_passkey(
    AxumPath((id, tab)): AxumPath<(Uuid, u32)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PasskeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let shared = crate::extension::controlled(&state, id, tab)?;
    let kind = match (req.find, &req.challenge) {
        (true, _) => "find",
        (false, Some(_)) => "assert",
        (false, None) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "A shared browser's passkey needs the page's challenge",
            ))
        }
    };
    let page_url = url::Url::parse(&shared.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page has no web origin"))?;
    let rp_id = req.rp_id.trim().trim_end_matches('.').to_ascii_lowercase();
    if !secrets::rp_id_valid_for(page_url.host_str().unwrap_or_default(), &rp_id) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The page is not on {rp_id} or a subdomain of it"),
        ));
    }
    if !req.find {
        info!(browser = %id, rp_id = %rp_id, "Passkey approved in a shared browser's live view");
    }
    let signing = passkeys::Signing {
        origin: (kind == "assert").then(|| page_url.origin().ascii_serialization()),
        challenge: req.challenge,
        allow: req.allow,
    };
    let request = state
        .passkeys
        .start(
            crate::extension::page_id(id, tab),
            rp_id,
            kind,
            req.entry,
            signing,
        )
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(serde_json::json!({ "request": request })))
}

#[derive(Deserialize)]
pub(crate) struct AutofillRequest {
    pub(crate) page_id: String,
    /// Vault entry id to fill; without one, the entries for the page's site are listed.
    #[serde(default)]
    pub(crate) entry: Option<String>,
    /// Fields of the entry to fill (default: username and password).
    #[serde(default)]
    pub(crate) fields: Vec<String>,
    /// Listing: unlock the vault (from the phone) if it's locked.
    #[serde(default)]
    pub(crate) unlock: bool,
    /// Listing: search the whole vault for this instead.
    #[serde(default)]
    pub(crate) query: Option<String>,
    /// Filling: the user confirmed an entry that isn't saved for the site.
    #[serde(default)]
    pub(crate) any_site: bool,
}

/// The vault button of the live view: list the logins for the page's site, or
/// fill one. The site is the page's host as the browser has it, never what the
/// view says. Answers with the request's id, polled at /api/passkey/{id}.
pub(crate) async fn api_session_autofill(
    AxumPath(id): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AutofillRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let targets = get_local_targets(&state).await;
    let page = targets
        .iter()
        .find(|t| t.id == req.page_id && t.session_id == Some(id))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found in this session"))?;
    let host = url::Url::parse(&page.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
    if let Some(session) = state.sessions.read().await.get(&id) {
        session.activity.touch();
    }
    if req.entry.is_some() {
        info!(
            session_id = %id,
            host = %host,
            any_site = req.any_site,
            "Login fill approved in the live view"
        );
    }
    let query = req
        .query
        .map(|q| q.trim().chars().take(100).collect::<String>())
        .filter(|q| !q.is_empty());
    let fields = if req.fields.is_empty() {
        vec!["username".into(), "password".into()]
    } else {
        req.fields
    };
    let request = state
        .passkeys
        .start_autofill(
            req.page_id,
            host.clone(),
            passkeys::Autofill {
                session: Some(id),
                entry: req.entry,
                fields,
                unlock: req.unlock,
                query,
                any_site: req.any_site,
            },
        )
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(
        serde_json::json!({ "request": request, "host": host }),
    ))
}

/// The vault button in the live view of a shared browser's tab (with control):
/// as `api_session_autofill`, for the site the browser reports for the tab.
pub(crate) async fn api_extension_autofill(
    AxumPath((id, tab)): AxumPath<(Uuid, u32)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AutofillRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let shared = crate::extension::controlled(&state, id, tab)?;
    let host = url::Url::parse(&shared.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
    if req.entry.is_some() {
        info!(browser = %id, host = %host, any_site = req.any_site, "Login fill approved in a shared browser's live view");
    }
    let query = req
        .query
        .map(|q| q.trim().chars().take(100).collect::<String>())
        .filter(|q| !q.is_empty());
    let fields = if req.fields.is_empty() {
        vec!["username".into(), "password".into()]
    } else {
        req.fields
    };
    let request = state
        .passkeys
        .start_autofill(
            crate::extension::page_id(id, tab),
            host.clone(),
            passkeys::Autofill {
                session: None,
                entry: req.entry,
                fields,
                unlock: req.unlock,
                query,
                any_site: req.any_site,
            },
        )
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, &e))?;
    Ok(Json(
        serde_json::json!({ "request": request, "host": host }),
    ))
}

/// The helper's fill for a shared browser's tab (see `api_page_autofill`).
async fn extension_page_autofill(
    state: &AppState,
    page_id: &str,
    (id, tab): (Uuid, u32),
    req: PageAutofill,
) -> Result<Vec<String>, ApiError> {
    let shared = crate::extension::controlled(state, id, tab)?;
    let page = url::Url::parse(&shared.url).ok();
    if page
        .as_ref()
        .and_then(|u| u.host_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        != Some(req.host.to_ascii_lowercase().as_str())
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("The page is not on {} anymore", req.host),
        ));
    }
    let uris = if req.any_site {
        let origin = page
            .map(|u| u.origin().ascii_serialization())
            .filter(|o| o.starts_with("http"))
            .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
        vec![secrets::Uri {
            uri: origin,
            match_type: Some(1),
        }]
    } else {
        req.uris
    };
    if !secrets::url_matches(&shared.url, &uris) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The entry is not for {}", req.host),
        ));
    }
    if !state
        .passkeys
        .authorize_fill(req.request, page_id, &req.host, None)
    {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "No approved fill request for this page",
        ));
    }
    shared
        .fill(move |u: &str| secrets::url_matches(u, &uris), &req.creds)
        .await
}

#[derive(Deserialize)]
pub(crate) struct PageAutofill {
    /// The "fill" request this answers: only one the helper took, for this page and
    /// host, and only once.
    pub(crate) request: Uuid,
    /// The page must still be on this host (the one the entry was checked for).
    pub(crate) host: String,
    /// The entry's URIs: only frames on these sites are filled.
    pub(crate) uris: Vec<secrets::Uri>,
    /// The entry is not saved for the site and was picked anyway (the helper
    /// told the user): only the page's own site is filled, then.
    #[serde(default)]
    pub(crate) any_site: bool,
    #[serde(flatten)]
    pub(crate) creds: autofill::Creds,
}

/// The passkey helper fills a vault entry into a page (see autofill.rs). The
/// values are never logged.
pub(crate) async fn api_page_autofill(
    AxumPath(page_id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PageAutofill>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    if let Some(shared) = crate::extension::parse_page_id(&page_id) {
        let filled = extension_page_autofill(&state, &page_id, shared, req).await?;
        if filled.is_empty() {
            return Err(api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "No login form found on this page",
            ));
        }
        return Ok(Json(serde_json::json!({ "filled": filled })));
    }
    let targets = get_local_targets(&state).await;
    let page =
        resolve_page(&targets, &page_id, None).map_err(|e| api_error(StatusCode::NOT_FOUND, e))?;
    let actual = url::Url::parse(&page.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    if actual.as_deref() != Some(req.host.to_ascii_lowercase().as_str()) {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!("The page is not on {} anymore", req.host),
        ));
    }
    let uris = if req.any_site {
        let origin = url::Url::parse(&page.url)
            .ok()
            .map(|u| u.origin().ascii_serialization())
            .filter(|o| o.starts_with("http"))
            .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "The page is not on a website"))?;
        vec![secrets::Uri {
            uri: origin,
            match_type: Some(1),
        }]
    } else {
        req.uris
    };
    if !secrets::url_matches(&page.url, &uris) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            format!("The entry is not for {}", req.host),
        ));
    }
    if !state
        .passkeys
        .authorize_fill(req.request, &page_id, &req.host, page.session_id)
    {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "No approved fill request for this page",
        ));
    }
    let sid = page
        .session_id
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Page not found"))?;
    let session = state
        .sessions
        .read()
        .await
        .get(&sid)
        .cloned()
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Session not found"))?;
    session.activity.touch();
    let frame_ok = move |u: &str| secrets::url_matches(u, &uris);
    let filled = match session.bidi {
        Some(ref hub) => autofill::fill_firefox(hub, &page_id, frame_ok, &req.creds).await,
        None => {
            let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", session.port, page_id);
            autofill::fill_chromium(&ws_url, frame_ok, &req.creds).await
        }
    }
    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
    if filled.is_empty() {
        return Err(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "No login form found on this page",
        ));
    }
    Ok(Json(serde_json::json!({ "filled": filled })))
}

pub(crate) async fn api_passkey_status(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    state
        .passkeys
        .status(req)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such passkey request"))
}

#[derive(Deserialize)]
pub(crate) struct PasskeyNote {
    pub(crate) note: String,
}

pub(crate) async fn api_passkey_helper_note(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(n): Json<PasskeyNote>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(if state.passkeys.note(req, n.note) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::GONE
    })
}

#[derive(Deserialize)]
pub(crate) struct HelperPoll {
    /// The helper handles "find" requests (newer helpers say so).
    #[serde(default)]
    pub(crate) finds: bool,
    /// The helper handles "logins" and "fill" requests.
    #[serde(default)]
    pub(crate) autofill: bool,
}

pub(crate) async fn api_passkey_helper_next(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(poll): Query<HelperPoll>,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    Ok(
        match state
            .passkeys
            .next(Duration::from_secs(25), poll.finds, poll.autofill)
            .await
        {
            Some(req) => Json(req).into_response(),
            None => StatusCode::NO_CONTENT.into_response(),
        },
    )
}

pub(crate) async fn api_passkey_helper_done(
    AxumPath(req): AxumPath<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(outcome): Json<passkeys::Outcome>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(if state.passkeys.complete(req, outcome) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::GONE
    })
}
