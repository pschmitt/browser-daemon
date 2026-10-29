//! A page's login for an AI agent's tools: the cookies the browser would send
//! to some addresses, and the page's local and session storage. For
//! `browser-daemon mcp`'s `browser_export_auth` and `browser_fetch`, which keep
//! the values out of the agent's tool results.
//!
//! `POST /api/pages/{id}/auth` (a session's page; Chromium over CDP, Firefox
//! over BiDi) and `POST /api/extension/nodes/{id}/tabs/{tab}/auth` (a shared
//! tab with control; the extension reads them). Both need the cluster secret;
//! neither is reachable through the shared-browser command route, whose
//! answers go to agents unmasked. Nothing here is logged.

use serde_json::{json, Map, Value};

use crate::autofill::Cdp;

/// At most this many addresses to collect cookies for.
pub const MAX_URLS: usize = 20;

/// Reads both storages of the page's top document (from an isolated world, so
/// page script cannot change what is read). An opaque origin has none.
const STORAGE: &str = r#"(() => {
  const read = (s) => { try { return Object.fromEntries(Object.entries(s)); } catch { return {}; } };
  return JSON.stringify({ local: read(localStorage), session: read(sessionStorage) });
})()"#;

/// The http(s) addresses to collect cookies for: the page's own first.
pub fn urls(page: &str, extra: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for u in std::iter::once(page.to_string()).chain(extra.iter().cloned()) {
        let parsed = url::Url::parse(&u).map_err(|_| format!("Not an address: {u}"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            if u == page {
                return Err("The page has no web address".into());
            }
            return Err(format!("Not a web address: {u}"));
        }
        let s = parsed.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.len() > MAX_URLS {
        return Err(format!("At most {MAX_URLS} addresses"));
    }
    Ok(out)
}

/// The answer: {url, cookies: [{name, value, domain, path, expires, http_only,
/// secure, same_site}], local_storage, session_storage}.
fn answer(url: &str, cookies: Vec<Value>, storage: Option<Value>) -> Value {
    let pick = |k: &str| {
        storage
            .as_ref()
            .and_then(|s| s[k].as_object().cloned())
            .unwrap_or_default()
    };
    json!({
        "url": url,
        "cookies": cookies,
        "local_storage": Value::Object(pick("local")),
        "session_storage": Value::Object(pick("session")),
    })
}

/// A Chromium page, over its DevTools connection.
pub async fn chromium(
    ws_url: &str,
    page: &str,
    urls: &[String],
    storage: bool,
) -> Result<Value, String> {
    let mut cdp = Cdp::connect(ws_url).await?;
    let got = cdp
        .call("Network.getCookies", json!({ "urls": urls }))
        .await?;
    let cookies = got["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            json!({
                "name": c["name"], "value": c["value"], "domain": c["domain"], "path": c["path"],
                // -1: a session cookie.
                "expires": c["expires"].as_f64().filter(|e| *e > 0.0),
                "http_only": c["httpOnly"].as_bool().unwrap_or(false),
                "secure": c["secure"].as_bool().unwrap_or(false),
                "same_site": c["sameSite"],
            })
        })
        .collect();
    let storage = if storage {
        let tree = cdp.call("Page.getFrameTree", json!({})).await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let world = cdp
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": "bd-auth" }),
            )
            .await?;
        let read = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": STORAGE, "contextId": world["executionContextId"], "returnByValue": true }),
            )
            .await?;
        read["result"]["value"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
    } else {
        None
    };
    cdp.close().await;
    Ok(answer(page, cookies, storage))
}

/// Whether the browser sends `cookie` (a BiDi or extension cookie: domain,
/// path, secure) with a request to `url`.
pub fn sent_to(domain: &str, path: &str, secure: bool, url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    let domain_ok = match domain.strip_prefix('.') {
        Some(d) => host == d || host.ends_with(&format!(".{d}")),
        None => host == domain,
    };
    let req_path = url.path();
    let path_ok = req_path == path
        || (req_path.starts_with(path)
            && (path.ends_with('/') || req_path[path.len()..].starts_with('/')));
    domain_ok && path_ok && (!secure || url.scheme() == "https" || host == "localhost")
}

/// A Firefox page (a BiDi browsing context).
pub async fn firefox(
    hub: &crate::bidi::BidiHub,
    context: &str,
    page: &str,
    urls: &[String],
    storage: bool,
) -> Result<Value, String> {
    let got = hub
        .call(
            "storage.getCookies",
            json!({ "partition": { "type": "context", "context": context } }),
        )
        .await?;
    let parsed: Vec<url::Url> = urls
        .iter()
        .filter_map(|u| url::Url::parse(u).ok())
        .collect();
    let mut cookies: Vec<Value> = Vec::new();
    for c in got["cookies"].as_array().into_iter().flatten() {
        let domain = c["domain"].as_str().unwrap_or_default();
        let path = c["path"].as_str().unwrap_or("/");
        let secure = c["secure"].as_bool().unwrap_or(false);
        if !parsed.iter().any(|u| sent_to(domain, path, secure, u)) {
            continue;
        }
        // BiDi values are {type: "string", value} (or base64).
        let value = match c["value"]["type"].as_str() {
            Some("string") => c["value"]["value"].clone(),
            _ => continue,
        };
        cookies.push(json!({
            "name": c["name"], "value": value, "domain": domain, "path": path,
            "expires": c["expiry"].as_f64(),
            "http_only": c["httpOnly"].as_bool().unwrap_or(false),
            "secure": secure,
            "same_site": c["sameSite"],
        }));
    }
    let storage = if storage {
        let read = hub
            .call(
                "script.evaluate",
                json!({
                    "expression": STORAGE,
                    "target": { "context": context, "sandbox": "bd-auth" },
                    "awaitPromise": false,
                }),
            )
            .await?;
        read["result"]["value"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
    } else {
        None
    };
    Ok(answer(page, cookies, storage))
}

/// An answer of the browser extension, checked and brought into shape.
pub fn from_extension(page: &str, v: &Value) -> Result<Value, String> {
    let cookies: Vec<Value> = v["cookies"]
        .as_array()
        .ok_or("The browser sent no cookies")?
        .iter()
        .filter(|c| c["name"].is_string() && c["value"].is_string())
        .map(|c| {
            json!({
                "name": c["name"], "value": c["value"], "domain": c["domain"], "path": c["path"],
                "expires": c["expires"].as_f64().filter(|e| *e > 0.0),
                "http_only": c["http_only"].as_bool().unwrap_or(false),
                "secure": c["secure"].as_bool().unwrap_or(false),
                "same_site": c["same_site"],
            })
        })
        .collect();
    let strings = |k: &str| -> Map<String, Value> {
        v[k].as_object()
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| v.is_string())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(json!({
        "url": page,
        "cookies": cookies,
        "local_storage": Value::Object(strings("local_storage")),
        "session_storage": Value::Object(strings("session_storage")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_comes_first_and_only_web_addresses_count() {
        let u = urls(
            "https://a.test/x",
            &["https://b.test/".into(), "https://a.test/x".into()],
        )
        .unwrap();
        assert_eq!(
            u,
            vec![
                "https://a.test/x".to_string(),
                "https://b.test/".to_string()
            ]
        );
        assert!(urls("about:blank", &[]).is_err());
        assert!(urls("https://a.test/", &["file:///etc/passwd".into()]).is_err());
        let many: Vec<String> = (0..MAX_URLS)
            .map(|i| format!("https://h{i}.test/"))
            .collect();
        assert!(urls("https://a.test/", &many).is_err());
    }

    #[test]
    fn cookies_go_where_the_browser_sends_them() {
        let u = url::Url::parse("https://login.example.com/app/home").unwrap();
        assert!(sent_to(".example.com", "/", true, &u));
        assert!(sent_to("login.example.com", "/app", false, &u));
        assert!(
            !sent_to("example.com", "/", false, &u),
            "host-only cookie of the parent"
        );
        assert!(!sent_to(".other.test", "/", false, &u));
        assert!(
            !sent_to(".example.com", "/ap", false, &u),
            "path must end at a segment"
        );
        let plain = url::Url::parse("http://login.example.com/").unwrap();
        assert!(
            !sent_to(".example.com", "/", true, &plain),
            "secure cookie over http"
        );
    }

    #[test]
    fn extension_answers_keep_to_strings() {
        let v = json!({
            "cookies": [
                { "name": "a", "value": "1", "domain": ".x.test", "path": "/", "expires": -1, "http_only": true, "secure": true },
                { "name": "b", "value": 2 }
            ],
            "local_storage": { "k": "v", "n": 3 },
        });
        let out = from_extension("https://x.test/", &v).unwrap();
        assert_eq!(out["cookies"].as_array().unwrap().len(), 1);
        assert_eq!(out["cookies"][0]["expires"], Value::Null);
        assert_eq!(out["local_storage"], json!({ "k": "v" }));
        assert_eq!(out["session_storage"], json!({}));
        assert!(from_extension("https://x.test/", &json!({})).is_err());
    }
}
