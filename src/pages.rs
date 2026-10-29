//! The static pages, icons and manifests, and the login redirect.

use super::*;

pub(crate) static HANDOFF_HTML: &str = include_str!("../handoff.html");

pub(crate) static DASHBOARD_HTML: &str = include_str!("../dashboard.html");

pub(crate) static FAVICON_ICO: &[u8] = include_bytes!("../assets/icons/favicon.ico");

pub(crate) static FAVICON_SVG: &str = include_str!("../assets/favicon.svg");

pub(crate) static HANDOFF_ICO: &[u8] = include_bytes!("../assets/icons/handoff.ico");

pub(crate) static HANDOFF_SVG: &str = include_str!("../assets/handoff.svg");

pub(crate) static MANIFEST_JSON: &str = include_str!("../manifest.webmanifest");

pub(crate) static SW_JS: &str = include_str!("../sw.js");

pub(crate) static ICON_192: &[u8] = include_bytes!("../assets/icons/icon-192.png");

pub(crate) static ICON_512: &[u8] = include_bytes!("../assets/icons/icon-512.png");
pub(crate) static ICON_MASKABLE_512: &[u8] =
    include_bytes!("../assets/icons/icon-maskable-512.png");

pub(crate) static DOWNLOAD_WORKER_JS: &str = include_str!("../download-worker.js");

/// What the dashboard and the handoff view share (helpers, colours).
pub(crate) static COMMON_JS: &str = include_str!("../assets/common.js");

pub(crate) static COMMON_CSS: &str = include_str!("../assets/common.css");

pub(crate) async fn favicon_ico() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/x-icon"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        FAVICON_ICO,
    )
}

pub(crate) async fn favicon_svg() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        FAVICON_SVG,
    )
}

pub(crate) async fn handoff_favicon_ico() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/x-icon"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        HANDOFF_ICO,
    )
}

pub(crate) async fn handoff_favicon_svg() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        HANDOFF_SVG,
    )
}

pub(crate) async fn manifest_webmanifest() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/manifest+json"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        MANIFEST_JSON,
    )
}

pub(crate) async fn service_worker_js() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/javascript"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        SW_JS,
    )
}

pub(crate) async fn icon_192_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        ICON_192,
    )
}

pub(crate) async fn icon_512_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        ICON_512,
    )
}

/// Full-bleed for launchers that cut icons to their own shape.
pub(crate) async fn icon_maskable_512_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        ICON_MASKABLE_512,
    )
}

pub(crate) async fn download_worker_js() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/javascript"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        DOWNLOAD_WORKER_JS,
    )
}

// Revalidated on every load, so a page and its shared code never come from two versions.
pub(crate) async fn common_js() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/javascript"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        COMMON_JS,
    )
}

pub(crate) async fn common_css() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        COMMON_CSS,
    )
}

/// A page that logs the browser in: the cookie comes with it, never the secret.
pub(crate) fn login_page(state: &AppState, headers: &HeaderMap, html: &'static str) -> Response {
    let mut res = (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Html(html),
    )
        .into_response();
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .next()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
        });
    if let Some(cookie) = state
        .auth
        .cookie(secure, auth::now())
        .and_then(|c| HeaderValue::from_str(&c).ok())
    {
        res.headers_mut().append(header::SET_COOKIE, cookie);
    }
    res
}

pub(crate) async fn dashboard_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    login_page(&state, &headers, DASHBOARD_HTML)
}

pub(crate) async fn handoff_page(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let html = if query.contains_key("sessions") {
        DASHBOARD_HTML
    } else {
        HANDOFF_HTML
    };
    login_page(&state, &headers, html)
}

/// A token for tools that cannot send the cookie or a header (`?auth=` on a
/// CDP endpoint). `null` when the daemon has no cluster secret.
pub(crate) async fn api_auth_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    state.check_auth(&headers)?;
    let issued = state.auth.issue(auth::now());
    Ok::<_, ApiError>((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(serde_json::json!({
            "token": issued.as_ref().map(|(t, _)| t),
            "expires_at": issued.as_ref().map(|(_, e)| e),
        })),
    ))
}
