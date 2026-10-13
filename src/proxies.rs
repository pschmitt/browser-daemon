//! Upstream proxies a session's traffic can go through, configured per node
//! (`--proxies-file`, a JSON list) and chosen per session or stored on a profile.

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
    /// `socks5://host:port`, `socks5h://…`, `socks4://…`, `http://…` or `https://…`.
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

struct Parsed {
    scheme: &'static str,
    host: String,
    port: u16,
}

fn parse(url: &str) -> Result<Parsed, String> {
    let u = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let (scheme, default_port) = match u.scheme() {
        // Chromium and Firefox both resolve names through a SOCKS5 proxy.
        "socks5" | "socks5h" | "socks" => ("socks5", 1080),
        "socks4" | "socks4a" => ("socks4", 1080),
        "http" => ("http", 80),
        "https" => ("https", 443),
        other => return Err(format!("unsupported scheme {other}")),
    };
    if !u.username().is_empty() || u.password().is_some() {
        // Neither browser takes proxy credentials on the command line or in prefs.
        return Err("proxy credentials are not supported".to_string());
    }
    let host = u.host_str().ok_or("missing host")?.to_string();
    Ok(Parsed {
        scheme,
        host,
        port: u.port().unwrap_or(default_port),
    })
}

impl Proxy {
    pub fn info(&self) -> ProxyInfo {
        let p = parse(&self.url).expect("validated on load");
        ProxyInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            scheme: p.scheme.to_string(),
            host: p.host,
            port: p.port,
        }
    }

    /// Chromium command-line flags routing all traffic through this proxy.
    pub fn chromium_args(&self) -> Vec<String> {
        let p = parse(&self.url).expect("validated on load");
        let host = if p.host.contains(':') {
            format!("[{}]", p.host)
        } else {
            p.host
        };
        vec![
            format!("--proxy-server={}://{}:{}", p.scheme, host, p.port),
            // Keep WebRTC from reaching out around the proxy and revealing the real address.
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
        ]
    }

    /// Firefox prefs (user.js lines) routing all traffic through this proxy.
    pub fn firefox_prefs(&self) -> Vec<String> {
        let p = parse(&self.url).expect("validated on load");
        let host = serde_json::to_string(&p.host).unwrap_or_default();
        let mut prefs = vec!["user_pref(\"network.proxy.type\", 1);".to_string()];
        match p.scheme {
            "socks5" | "socks4" => prefs.extend([
                format!("user_pref(\"network.proxy.socks\", {host});"),
                format!("user_pref(\"network.proxy.socks_port\", {});", p.port),
                format!(
                    "user_pref(\"network.proxy.socks_version\", {});",
                    if p.scheme == "socks5" { 5 } else { 4 }
                ),
                "user_pref(\"network.proxy.socks_remote_dns\", true);".to_string(),
            ]),
            _ => {
                for key in ["http", "ssl"] {
                    prefs.push(format!("user_pref(\"network.proxy.{key}\", {host});"));
                    prefs.push(format!(
                        "user_pref(\"network.proxy.{key}_port\", {});",
                        p.port
                    ));
                }
            }
        }
        prefs.push("user_pref(\"media.peerconnection.ice.proxy_only\", true);".to_string());
        // Only the proxy's own name is resolved locally (pages resolve through the
        // proxy), and Firefox does not fall back to IPv4 when IPv6 to the proxy is
        // silently dropped: it waits for the connection to time out.
        prefs.push("user_pref(\"network.dns.disableIPv6\", true);".to_string());
        prefs
    }
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
/// anything else in the file alone. `prefs` empty means direct connections.
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
    if prefs.is_empty() {
        out.push_str("user_pref(\"network.proxy.type\", 0);\n");
    } else {
        for p in prefs {
            out.push_str(p);
            out.push('\n');
        }
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
    fn socks5h_maps_to_chromium_socks5() {
        let p = proxy("m", "socks5h://turris.example:1080");
        assert_eq!(
            p.chromium_args()[0],
            "--proxy-server=socks5://turris.example:1080"
        );
        assert_eq!(p.info().scheme, "socks5");
    }

    #[test]
    fn validation_rejects_bad_entries() {
        assert!(validate(vec![proxy("none", "socks5://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://a:1"), proxy("a", "socks5://b:1")]).is_err());
        assert!(validate(vec![proxy("a", "ftp://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://u:p@a:1")]).is_err());
        assert!(validate(vec![proxy("a", "http://a")]).is_ok());
    }

    #[test]
    fn user_js_block_is_replaced_not_appended() {
        let p = proxy("m", "socks5://h:1080");
        let once = firefox_user_js("user_pref(\"x\", 1);\n", &p.firefox_prefs());
        let twice = firefox_user_js(&once, &p.firefox_prefs());
        assert_eq!(once, twice);
        assert!(once.starts_with("user_pref(\"x\", 1);\n"));
        let direct = firefox_user_js(&twice, &[]);
        assert!(direct.contains("\"network.proxy.type\", 0"));
        assert!(!direct.contains("socks_port"));
    }
}
