//! Upstream proxies a session's traffic can go through, configured per node
//! (`--proxies-file`, a JSON list) and chosen per session or stored on a profile.
//! Browsers never talk to them directly: they use the session's relay
//! (`relay.rs`), so the upstream can change while the session runs.

use serde::{Deserialize, Serialize};
use std::path::Path;
use url::Url;

/// Keys a request uses to ask for no proxy, overriding a profile's default.
pub const DIRECT: &[&str] = &["", "none", "direct"];

#[derive(Deserialize, Debug, Clone)]
pub struct Proxy {
    /// Stable key that sessions and profiles refer to.
    pub id: String,
    /// Shown in the dashboard.
    pub name: String,
    /// `socks5://host:port` (also `socks5h://`), `socks4://…` or `http://…`.
    pub url: String,
}

/// What the API exposes: never the credentials a URL might carry.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProxyInfo {
    pub id: String,
    pub name: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// Names are resolved by the proxy.
    Socks5,
    /// SOCKS4a: names are resolved by the proxy.
    Socks4,
    /// HTTP CONNECT tunnels.
    Http,
}

#[derive(Debug, Clone)]
pub struct UpstreamAddr {
    pub kind: Upstream,
    pub host: String,
    pub port: u16,
}

fn parse(url: &str) -> Result<UpstreamAddr, String> {
    let u = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let (kind, default_port) = match u.scheme() {
        "socks5" | "socks5h" | "socks" => (Upstream::Socks5, 1080),
        "socks4" | "socks4a" => (Upstream::Socks4, 1080),
        "http" => (Upstream::Http, 80),
        other => return Err(format!("unsupported scheme {other}")),
    };
    if !u.username().is_empty() || u.password().is_some() {
        return Err("proxy credentials are not supported".to_string());
    }
    let host = u
        .host_str()
        .ok_or("missing host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Ok(UpstreamAddr {
        kind,
        host,
        port: u.port().unwrap_or(default_port),
    })
}

impl Proxy {
    pub fn upstream(&self) -> UpstreamAddr {
        parse(&self.url).expect("validated on load")
    }

    pub fn info(&self) -> ProxyInfo {
        let p = self.upstream();
        ProxyInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            scheme: match p.kind {
                Upstream::Socks5 => "socks5",
                Upstream::Socks4 => "socks4",
                Upstream::Http => "http",
            }
            .to_string(),
            host: p.host,
            port: p.port,
        }
    }
}

/// Chromium flags sending all traffic through the session's relay.
pub fn chromium_args(relay_port: u16) -> Vec<String> {
    vec![
        format!("--proxy-server=socks5://127.0.0.1:{relay_port}"),
        // Chromium skips proxies for loopback unless told otherwise: the relay
        // must see it to refuse it (the daemon listens there).
        "--proxy-bypass-list=<-loopback>".to_string(),
        // WebRTC must not reach out around the relay and reveal the real address.
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
    ]
}

/// Firefox prefs (user.js lines) sending all traffic through the session's relay.
pub fn firefox_prefs(relay_port: u16) -> Vec<String> {
    vec![
        "user_pref(\"network.proxy.type\", 1);".to_string(),
        "user_pref(\"network.proxy.socks\", \"127.0.0.1\");".to_string(),
        format!("user_pref(\"network.proxy.socks_port\", {relay_port});"),
        "user_pref(\"network.proxy.socks_version\", 5);".to_string(),
        "user_pref(\"network.proxy.socks_remote_dns\", true);".to_string(),
        // Same for Firefox: loopback goes through the relay, which refuses it.
        "user_pref(\"network.proxy.allow_hijacking_localhost\", true);".to_string(),
        "user_pref(\"network.proxy.no_proxies_on\", \"\");".to_string(),
        "user_pref(\"media.peerconnection.ice.proxy_only\", true);".to_string(),
    ]
}

/// Load and validate the node's proxies. Errors name the offending entry.
pub fn load(path: &Path) -> Result<Vec<Proxy>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let proxies: Vec<Proxy> =
        serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    validate(proxies)
}

fn validate(proxies: Vec<Proxy>) -> Result<Vec<Proxy>, String> {
    let mut seen = std::collections::HashSet::new();
    for p in &proxies {
        if p.id.trim().is_empty() || DIRECT.contains(&p.id.trim()) {
            return Err(format!("proxy {:?}: reserved or empty id", p.id));
        }
        if !seen.insert(p.id.clone()) {
            return Err(format!("proxy {:?}: duplicate id", p.id));
        }
        parse(&p.url).map_err(|e| format!("proxy {:?}: {e}", p.id))?;
    }
    Ok(proxies)
}

/// Rewrite the browser-daemon block of a Firefox profile's user.js, leaving
/// anything else in the file alone.
pub fn firefox_user_js(existing: &str, prefs: &[String]) -> String {
    const BEGIN: &str = "// browser-daemon proxy: begin";
    const END: &str = "// browser-daemon proxy: end";
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line == BEGIN {
            skipping = true;
        } else if line == END {
            skipping = false;
        } else if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(BEGIN);
    out.push('\n');
    for p in prefs {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str(END);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(id: &str, url: &str) -> Proxy {
        Proxy {
            id: id.into(),
            name: id.into(),
            url: url.into(),
        }
    }

    #[test]
    fn upstreams_parse() {
        let p = proxy("m", "socks5h://turris.example:1080").upstream();
        assert_eq!(
            (p.kind, p.host.as_str(), p.port),
            (Upstream::Socks5, "turris.example", 1080)
        );
        let p = proxy("h", "http://[::1]").upstream();
        assert_eq!(
            (p.kind, p.host.as_str(), p.port),
            (Upstream::Http, "::1", 80)
        );
    }

    #[test]
    fn validation_rejects_bad_entries() {
        assert!(validate(vec![proxy("none", "socks5://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://a:1"), proxy("a", "socks5://b:1")]).is_err());
        assert!(validate(vec![proxy("a", "ftp://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "https://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://u:p@a:1")]).is_err());
        assert!(validate(vec![proxy("a", "http://a")]).is_ok());
    }

    #[test]
    fn user_js_block_is_replaced_not_appended() {
        let once = firefox_user_js("user_pref(\"x\", 1);\n", &firefox_prefs(1080));
        let twice = firefox_user_js(&once, &firefox_prefs(1080));
        assert_eq!(once, twice);
        assert!(once.starts_with("user_pref(\"x\", 1);\n"));
        let moved = firefox_user_js(&twice, &firefox_prefs(2000));
        assert!(moved.contains("socks_port\", 2000"));
        assert!(!moved.contains("socks_port\", 1080"));
    }
}
