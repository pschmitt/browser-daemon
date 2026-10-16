//! Who may talk to the daemon, for every route at once (see `layer`).
//!
//! Three checks run before a handler does, WebSocket upgrades included:
//! the `Host` must be one this node answers to (DNS rebinding), an `Origin`
//! must be the node itself (cross-site WebSocket hijacking), and the request
//! must carry the cluster secret or a token derived from it. The routes that
//! hand out the cookie (`/`, `/handoff/`) and the static assets stay public.
//!
//! A token is `<expiry>.<HMAC-SHA256 of the expiry>`, keyed from the cluster
//! secret, so it survives restarts and is the same on every node of the mesh.
//! It travels as the `bd_auth` cookie (the dashboard), as `Bearer <token>`, or
//! as `?auth=<token>` (CDP endpoints given to tools that cannot set headers).
//! The secret itself still works as a Bearer or `X-Browser-Cluster-Secret`
//! (peers, the CLI, the passkey helper) and is never put in a URL or a page.
//! Without a cluster secret the daemon is open as before, but the Host and
//! Origin checks still apply.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

pub const COOKIE: &str = "bd_auth";
/// How long a token (and its cookie) stays valid.
pub const TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;

/// What a request proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// The daemon has no cluster secret.
    Open,
    /// A token (cookie, Bearer or `?auth=`): a viewer.
    Token,
    /// The cluster secret itself: a peer, the CLI or the passkey helper.
    Secret,
}

pub struct Auth {
    secret: Option<String>,
    /// HMAC key for tokens, derived from the secret.
    key: Option<Vec<u8>>,
    hosts: HostPolicy,
}

impl Auth {
    pub fn new(secret: Option<String>, hosts: HostPolicy) -> Self {
        let key = secret.as_deref().map(|s| {
            let mut mac = HmacSha256::new_from_slice(s.as_bytes()).expect("any key length");
            mac.update(b"browser-daemon auth token v1");
            mac.finalize().into_bytes().to_vec()
        });
        Self { secret, key, hosts }
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.secret.is_some()
    }

    #[allow(dead_code)]
    pub fn hosts(&self) -> &HostPolicy {
        &self.hosts
    }

    fn mac(&self, message: &str) -> Option<HmacSha256> {
        let mut mac = HmacSha256::new_from_slice(self.key.as_ref()?).ok()?;
        mac.update(message.as_bytes());
        Some(mac)
    }

    /// A token valid until `now + TOKEN_TTL_SECS`: (token, expiry).
    pub fn issue(&self, now: u64) -> Option<(String, u64)> {
        let expires = now + TOKEN_TTL_SECS;
        let tag = self.mac(&expires.to_string())?.finalize().into_bytes();
        Some((format!("{expires}.{}", hex(&tag)), expires))
    }

    pub fn verify_token(&self, token: &str, now: u64) -> bool {
        let Some((expires, tag)) = token.split_once('.') else {
            return false;
        };
        let (Ok(expires_at), Some(tag)) = (expires.parse::<u64>(), unhex(tag)) else {
            return false;
        };
        let Some(mac) = self.mac(expires) else {
            return false;
        };
        // verify_slice compares in constant time.
        mac.verify_slice(&tag).is_ok() && expires_at > now
    }

    /// Whether `candidate` is the cluster secret, in constant time.
    pub fn is_secret(&self, candidate: &str) -> bool {
        let (Some(secret), Some(key)) = (self.secret.as_deref(), self.key.as_deref()) else {
            return false;
        };
        let tag = |s: &str| {
            let mut m = HmacSha256::new_from_slice(key).expect("any key length");
            m.update(s.as_bytes());
            m
        };
        tag(candidate)
            .verify_slice(&tag(secret).finalize().into_bytes())
            .is_ok()
    }

    /// The strongest credential among the request's headers and `?auth=`.
    pub fn credential(
        &self,
        headers: &HeaderMap,
        query_auth: Option<&str>,
        now: u64,
    ) -> Option<Credential> {
        if self.secret.is_none() {
            return Some(Credential::Open);
        }
        let mut best = None;
        for candidate in candidates(headers, query_auth) {
            if self.is_secret(&candidate) {
                return Some(Credential::Secret);
            }
            if self.verify_token(&candidate, now) {
                best = Some(Credential::Token);
            }
        }
        best
    }

    /// The `Set-Cookie` value that logs a browser in.
    pub fn cookie(&self, secure: bool, now: u64) -> Option<String> {
        let (token, _) = self.issue(now)?;
        Some(format!(
            "{COOKIE}={token}; Max-Age={TOKEN_TTL_SECS}; Path=/; HttpOnly; SameSite=Strict{}",
            if secure { "; Secure" } else { "" }
        ))
    }

    /// Every check for one request: the credential it proved, or why it is refused.
    pub fn decide(&self, req: &RequestInfo<'_>, now: u64) -> Result<Credential, Refusal> {
        if req.path != "/health" && !self.hosts.allows(req.host) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "Host not allowed (see --allowed-host)",
            ));
        }
        if let Some(origin) = req.origin {
            if !origin_matches_host(origin, req.host) && !self.hosts.allows_origin(origin) {
                return Err(Refusal::new(StatusCode::FORBIDDEN, "Cross-origin request"));
            }
        }
        let public = is_public(req.method, req.path);
        // Browsers say what a request is: a page on another site that makes
        // the browser fetch or embed us is refused, only a click through to
        // the dashboard or handoff is fine.
        if matches!(req.sec_fetch_site, Some("cross-site" | "same-site"))
            && !(public && req.sec_fetch_mode == Some("navigate"))
        {
            return Err(Refusal::new(StatusCode::FORBIDDEN, "Cross-site request"));
        }
        if public {
            return Ok(self
                .credential(req.headers, req.query_auth, now)
                .unwrap_or(Credential::Open));
        }
        let Some(cred) = self.credential(req.headers, req.query_auth, now) else {
            return Err(Refusal::new(
                StatusCode::UNAUTHORIZED,
                "Unauthorized: invalid or missing credentials",
            ));
        };
        if cred == Credential::Token && secret_only(req.method, req.path) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "This needs the cluster secret, not a viewer token",
            ));
        }
        Ok(cred)
    }
}

pub struct Refusal {
    pub status: StatusCode,
    pub message: &'static str,
}

impl Refusal {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

/// The parts of a request the decision looks at.
pub struct RequestInfo<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub sec_fetch_site: Option<&'a str>,
    pub sec_fetch_mode: Option<&'a str>,
    pub query_auth: Option<&'a str>,
}

/// Routes anyone who can reach the node may use: static assets, the pages that
/// hand out the cookie, and what tools probe before they have one.
pub fn is_public(method: &Method, path: &str) -> bool {
    if method == Method::POST && path == "/api/agent-topic" {
        return true;
    }
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    matches!(
        path,
        "/" | "/dashboard"
            | "/handoff"
            | "/handoff/"
            | "/health"
            | "/json/version"
            | "/favicon.ico"
            | "/favicon.svg"
            | "/handoff/favicon.ico"
            | "/handoff/favicon.svg"
            | "/manifest.webmanifest"
            | "/manifest.json"
            | "/site.webmanifest"
            | "/sw.js"
            | "/icon-192.png"
            | "/icon-512.png"
            | "/download-worker.js"
    ) || path.starts_with("/assets/")
}

/// Routes that move the vault's secrets or hand the cluster secret to a third
/// party: a viewer's token (which a page in a session can obtain) is not enough.
pub fn secret_only(method: &Method, path: &str) -> bool {
    if path.starts_with("/api/passkey-helper/") {
        return true;
    }
    if method == Method::POST && path.starts_with("/api/pages/") && path.ends_with("/autofill") {
        return true;
    }
    // Every request to a peer carries the cluster secret.
    (path == "/api/peers" && method == Method::POST)
        || (method == Method::DELETE
            && path.starts_with("/api/peers/")
            && path.matches('/').count() == 3)
}

fn candidates(headers: &HeaderMap, query_auth: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(h) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
    {
        if let Some(t) = h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
        {
            out.push(t.trim().to_string());
        }
    }
    if let Some(h) = headers
        .get("x-browser-cluster-secret")
        .and_then(|h| h.to_str().ok())
    {
        out.push(h.trim().to_string());
    }
    if let Some(q) = query_auth {
        out.push(q.to_string());
    }
    for cookie in headers.get_all(header::COOKIE) {
        let Ok(cookie) = cookie.to_str() else {
            continue;
        };
        for pair in cookie.split(';') {
            if let Some(("bd_auth", v)) = pair.trim().split_once('=') {
                out.push(v.to_string());
            }
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------- hosts

/// The `Host` values this node answers to.
#[derive(Debug, Clone)]
pub struct HostPolicy {
    /// (host, port): a rule without a port takes any port.
    rules: Vec<(String, Option<u16>)>,
}

/// `host[:port]` of a `Host` header or `--allowed-host` value; IPv6 in brackets.
pub fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    let s = s.trim();
    if s.is_empty() || s.contains(['/', '@', ' ']) {
        return None;
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        (h, port)
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p.parse().ok()?)),
            Some(_) => return None,
            None => (s, None),
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some((host, port))
}

impl HostPolicy {
    /// The public URL's host, the loopback names on the bind port, and `extra`
    /// (`--allowed-host` values).
    pub fn new(public_url: &str, extra: &[String], bind_port: u16) -> Self {
        let mut rules = Vec::new();
        if let Some(host) = url::Url::parse(public_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string()))
        {
            rules.push((host.to_ascii_lowercase(), None));
        }
        for h in ["127.0.0.1", "localhost", "::1"] {
            rules.push((h.to_string(), Some(bind_port)));
        }
        rules.extend(extra.iter().filter_map(|e| split_host_port(e)));
        Self { rules }
    }

    pub fn allows(&self, host_header: Option<&str>) -> bool {
        let Some((host, port)) = host_header.and_then(split_host_port) else {
            return false;
        };
        self.rules
            .iter()
            .any(|(h, p)| *h == host && p.is_none_or(|p| Some(p) == port))
    }

    /// Whether an `Origin` is one of this node's own names. A proxy in front
    /// (the public vhost on another host) may rewrite `Host` to the node's
    /// name while the browser's `Origin` still names the public one.
    pub fn allows_origin(&self, origin: &str) -> bool {
        let Ok(url) = url::Url::parse(origin) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        self.allows(Some(&authority))
    }

    /// Names of the hosts the mesh reaches this node under, for the relay.
    pub fn names(&self) -> Vec<String> {
        self.rules.iter().map(|(h, _)| h.clone()).collect()
    }
}

/// Whether an `Origin` header names the node the request went to.
pub fn origin_matches_host(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let Ok(origin) = url::Url::parse(origin) else {
        return false; // "null" and garbage
    };
    let (Some(o_host), Some((h_host, h_port))) = (
        origin
            .host_str()
            .map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase()),
        split_host_port(host),
    ) else {
        return false;
    };
    // A Host header without a port means the scheme's default.
    let default = match origin.scheme() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    o_host == h_host && origin.port_or_known_default() == h_port.or(default)
}

// ----------------------------------------------------------- middleware

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `?auth=` value of a query string.
fn query_auth(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find_map(|(k, v)| (k == "auth").then(|| v.into_owned()))
}

/// Runs before every handler; see the module docs.
pub async fn layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let auth = state.auth.clone();
    let headers = req.headers();
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let host = header_str("host").or_else(|| req.uri().authority().map(|a| a.to_string()));
    let origin = header_str("origin");
    let site = header_str("sec-fetch-site");
    let mode = header_str("sec-fetch-mode");
    let q = query_auth(req.uri().query());
    let info = RequestInfo {
        method: req.method(),
        path: req.uri().path(),
        headers,
        host: host.as_deref(),
        origin: origin.as_deref(),
        sec_fetch_site: site.as_deref(),
        sec_fetch_mode: mode.as_deref(),
        query_auth: q.as_deref(),
    };
    match auth.decide(&info, now()) {
        Ok(cred) => {
            // Handlers still run `check_auth`: hand them the credential as a header.
            if let (Some(q), true) = (q.as_deref(), cred != Credential::Open) {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {q}")) {
                    req.headers_mut().insert(header::AUTHORIZATION, v);
                }
            }
            next.run(req).await
        }
        Err(refusal) => refusal.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(secret: Option<&str>) -> Auth {
        Auth::new(
            secret.map(str::to_string),
            HostPolicy::new(
                "https://browser.fnuc.test",
                &["extra.test:8443".to_string()],
                3001,
            ),
        )
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    struct Req<'a> {
        method: Method,
        path: &'a str,
        host: Option<&'a str>,
        origin: Option<&'a str>,
        site: Option<&'a str>,
        mode: Option<&'a str>,
        query: Option<&'a str>,
        headers: HeaderMap,
    }

    fn req<'a>(path: &'a str) -> Req<'a> {
        Req {
            method: Method::GET,
            path,
            host: Some("browser.fnuc.test"),
            origin: None,
            site: None,
            mode: None,
            query: None,
            headers: HeaderMap::new(),
        }
    }

    fn decide(a: &Auth, r: &Req) -> Result<Credential, StatusCode> {
        a.decide(
            &RequestInfo {
                method: &r.method,
                path: r.path,
                headers: &r.headers,
                host: r.host,
                origin: r.origin,
                sec_fetch_site: r.site,
                sec_fetch_mode: r.mode,
                query_auth: r.query,
            },
            1_000,
        )
        .map_err(|r| r.status)
    }

    #[test]
    fn tokens_verify_expire_and_reject_forgeries() {
        let a = auth(Some("s3cret"));
        let (t, exp) = a.issue(1_000).unwrap();
        assert_eq!(exp, 1_000 + TOKEN_TTL_SECS);
        assert!(a.verify_token(&t, 1_001));
        assert!(!a.verify_token(&t, exp));
        // Same secret, same token: nodes of a mesh and restarts agree.
        assert_eq!(auth(Some("s3cret")).issue(1_000).unwrap().0, t);
        assert!(!auth(Some("other")).verify_token(&t, 1_001));
        let (_, tag) = t.split_once('.').unwrap();
        assert!(!a.verify_token(&format!("{}.{tag}", exp + 1), 1_001));
        for bad in ["", ".", "x.y", "12.zz", "12.", "s3cret"] {
            assert!(!a.verify_token(bad, 1));
        }
        assert!(!auth(None).verify_token(&t, 1_001));
    }

    #[test]
    fn the_secret_compares_exactly() {
        let a = auth(Some("s3cret"));
        assert!(a.is_secret("s3cret"));
        assert!(!a.is_secret("s3cre"));
        assert!(!a.is_secret("s3cret "));
        assert!(!a.is_secret(""));
        assert!(!auth(None).is_secret(""));
    }

    #[test]
    fn credentials_from_cookie_bearer_header_and_query() {
        let a = auth(Some("s3cret"));
        let (t, _) = a.issue(1_000).unwrap();
        let cred = |h: &[(&str, &str)], q| a.credential(&headers(h), q, 1_001);
        assert_eq!(cred(&[], None), None);
        assert_eq!(
            cred(&[("authorization", "Bearer s3cret")], None),
            Some(Credential::Secret)
        );
        assert_eq!(
            cred(&[("x-browser-cluster-secret", "s3cret")], None),
            Some(Credential::Secret)
        );
        assert_eq!(
            cred(&[("authorization", &format!("Bearer {t}"))], None),
            Some(Credential::Token)
        );
        assert_eq!(
            cred(&[("cookie", &format!("a=b; bd_auth={t}; c=d"))], None),
            Some(Credential::Token)
        );
        assert_eq!(cred(&[], Some(&t)), Some(Credential::Token));
        assert_eq!(cred(&[("cookie", "bd_auth=garbage")], None), None);
        assert_eq!(cred(&[("authorization", "Basic s3cret")], None), None);
        // A wrong cookie does not hide a right header.
        assert_eq!(
            cred(
                &[
                    ("cookie", "bd_auth=garbage"),
                    ("authorization", "Bearer s3cret")
                ],
                None
            ),
            Some(Credential::Secret)
        );
        assert_eq!(
            auth(None).credential(&headers(&[]), None, 1),
            Some(Credential::Open)
        );
    }

    #[test]
    fn protected_routes_need_a_credential() {
        let a = auth(Some("s3cret"));
        let (t, _) = a.issue(1_000).unwrap();
        for path in [
            "/chromium",
            "/json/list",
            "/json/new",
            "/devtools/page/x",
            "/video",
            "/api/sessions",
            "/api/auth/token",
        ] {
            assert_eq!(
                decide(&a, &req(path)),
                Err(StatusCode::UNAUTHORIZED),
                "{path}"
            );
            let mut ok = req(path);
            ok.headers = headers(&[("cookie", &format!("bd_auth={t}"))]);
            assert_eq!(decide(&a, &ok), Ok(Credential::Token), "{path}");
        }
        // Open mode lets everything through.
        assert_eq!(decide(&auth(None), &req("/chromium")), Ok(Credential::Open));
    }

    #[test]
    fn public_routes_need_none() {
        let a = auth(Some("s3cret"));
        for path in [
            "/",
            "/handoff/",
            "/health",
            "/json/version",
            "/sw.js",
            "/favicon.svg",
            "/icon-192.png",
            "/assets/common.js",
        ] {
            assert!(decide(&a, &req(path)).is_ok(), "{path}");
        }
        let mut topic = req("/api/agent-topic");
        topic.method = Method::POST;
        assert!(decide(&a, &topic).is_ok());
        // Only what is listed, and only for reading.
        assert_eq!(
            decide(&a, &req("/api/agent-topic")),
            Err(StatusCode::UNAUTHORIZED)
        );
        let mut post = req("/");
        post.method = Method::POST;
        assert_eq!(decide(&a, &post), Err(StatusCode::UNAUTHORIZED));
        assert_eq!(decide(&a, &req("/json")), Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn viewers_cannot_reach_the_helper_or_peer_routes() {
        let a = auth(Some("s3cret"));
        let (t, _) = a.issue(1_000).unwrap();
        for (m, path) in [
            (Method::GET, "/api/passkey-helper/next"),
            (
                Method::POST,
                "/api/passkey-helper/00000000-0000-0000-0000-000000000000",
            ),
            (Method::POST, "/api/pages/abc/autofill"),
            (Method::POST, "/api/peers"),
            (Method::DELETE, "/api/peers/rofl-13"),
        ] {
            let mut viewer = req(path);
            viewer.method = m.clone();
            viewer.headers = headers(&[("authorization", &format!("Bearer {t}"))]);
            assert_eq!(decide(&a, &viewer), Err(StatusCode::FORBIDDEN), "{path}");
            let mut helper = req(path);
            helper.method = m;
            helper.headers = headers(&[("x-browser-cluster-secret", "s3cret")]);
            assert_eq!(decide(&a, &helper), Ok(Credential::Secret), "{path}");
        }
        // Reading the peer list and the peer proxy stay open to viewers.
        assert!(!secret_only(&Method::GET, "/api/peers"));
        assert!(!secret_only(&Method::DELETE, "/api/peers/rofl-13/sessions"));
        assert!(!secret_only(&Method::POST, "/api/peers/rofl-13/sessions"));
    }

    #[test]
    fn the_host_must_be_one_of_ours() {
        let a = auth(Some("s3cret"));
        let host = |h: Option<&str>| {
            let mut r = req("/");
            r.host = h;
            decide(&a, &r)
        };
        assert!(host(Some("browser.fnuc.test")).is_ok());
        assert!(host(Some("Browser.FNUC.test:443")).is_ok());
        assert!(host(Some("127.0.0.1:3001")).is_ok());
        assert!(host(Some("localhost:3001")).is_ok());
        assert!(host(Some("[::1]:3001")).is_ok());
        assert!(host(Some("extra.test:8443")).is_ok());
        assert_eq!(host(Some("evil.test")), Err(StatusCode::FORBIDDEN));
        assert_eq!(
            host(Some("rebind.evil.test:3001")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(host(Some("127.0.0.1")), Err(StatusCode::FORBIDDEN));
        assert_eq!(host(Some("127.0.0.1:8080")), Err(StatusCode::FORBIDDEN));
        assert_eq!(host(Some("extra.test")), Err(StatusCode::FORBIDDEN));
        assert_eq!(
            host(Some("browser.fnuc.test.evil.test")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(host(None), Err(StatusCode::FORBIDDEN));
        // Also without a cluster secret, and with one the wrong host loses.
        let mut open = req("/chromium");
        open.host = Some("evil.test");
        assert_eq!(decide(&auth(None), &open), Err(StatusCode::FORBIDDEN));
        let mut h = req("/health");
        h.host = Some("anything");
        assert!(decide(&a, &h).is_ok());
    }

    #[test]
    fn an_origin_must_be_the_node_itself() {
        let a = auth(None);
        let origin = |o: &'static str, host: &'static str| {
            let mut r = req("/chromium");
            r.host = Some(host);
            r.origin = Some(o);
            decide(&a, &r)
        };
        assert!(origin("https://browser.fnuc.test", "browser.fnuc.test").is_ok());
        // Behind a proxy that rewrites Host: the Origin is another allowed name.
        let hosts = HostPolicy::new("https://browser.fnuc.test", &["browser.test".into()], 3001);
        assert!(hosts.allows_origin("https://browser.test"));
        assert!(hosts.allows_origin("https://browser.fnuc.test:8443"));
        assert!(!hosts.allows_origin("https://evil.test"));
        assert!(!hosts.allows_origin("http://127.0.0.1"));
        assert!(!hosts.allows_origin("null"));
        assert!(origin("https://browser.fnuc.test:443", "browser.fnuc.test").is_ok());
        assert!(origin("http://127.0.0.1:3001", "127.0.0.1:3001").is_ok());
        assert_eq!(
            origin("https://evil.test", "browser.fnuc.test"),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            origin("null", "browser.fnuc.test"),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            origin("http://127.0.0.1:3002", "127.0.0.1:3001"),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            origin("https://browser.fnuc.test.evil.test", "browser.fnuc.test"),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            origin("https://sibling.fnuc.test", "browser.fnuc.test"),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn cross_site_fetches_are_refused_but_navigation_to_the_pages_is_not() {
        let a = auth(Some("s3cret"));
        let (t, _) = a.issue(1_000).unwrap();
        let mut img = req("/json/new");
        img.site = Some("cross-site");
        img.headers = headers(&[("cookie", &format!("bd_auth={t}"))]);
        assert_eq!(decide(&a, &img), Err(StatusCode::FORBIDDEN));
        img.site = Some("same-site");
        assert_eq!(decide(&a, &img), Err(StatusCode::FORBIDDEN));
        img.site = Some("same-origin");
        assert!(decide(&a, &img).is_ok());
        img.site = None; // not a browser
        assert!(decide(&a, &img).is_ok());
        let mut nav = req("/handoff/");
        nav.site = Some("cross-site");
        nav.mode = Some("navigate");
        assert!(decide(&a, &nav).is_ok());
        let mut sub = req("/handoff/");
        sub.site = Some("cross-site");
        sub.mode = Some("no-cors");
        assert_eq!(decide(&a, &sub), Err(StatusCode::FORBIDDEN));
        let mut nav_api = req("/api/profiles/x/export");
        nav_api.site = Some("cross-site");
        nav_api.mode = Some("navigate");
        nav_api.headers = img.headers.clone();
        assert_eq!(decide(&a, &nav_api), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn the_cookie_is_locked_down() {
        let a = auth(Some("s3cret"));
        let plain = a.cookie(false, 1_000).unwrap();
        assert!(plain.starts_with("bd_auth="));
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=2592000"] {
            assert!(plain.contains(attr), "{attr}");
        }
        assert!(!plain.contains("Secure"));
        assert!(a.cookie(true, 1_000).unwrap().ends_with("; Secure"));
        assert!(!plain.contains("s3cret"));
        assert_eq!(auth(None).cookie(true, 1), None);
    }

    #[test]
    fn host_values_parse() {
        assert_eq!(
            split_host_port("a.test:81"),
            Some(("a.test".into(), Some(81)))
        );
        assert_eq!(
            split_host_port("[::1]:3001"),
            Some(("::1".into(), Some(3001)))
        );
        assert_eq!(split_host_port("[::1]"), Some(("::1".into(), None)));
        assert_eq!(split_host_port("A.Test."), Some(("a.test".into(), None)));
        for bad in ["", "a.test:x", "a/b", "u@a.test", "::1", "a b"] {
            assert_eq!(split_host_port(bad), None, "{bad}");
        }
    }
}
