//! Upstream proxies a session's traffic can go through, configured per node
//! (`--proxies-file`, a JSON list) and chosen per session or stored on a profile.
//! Browsers never talk to them directly: they use the session's relay
//! (`relay.rs`), so the upstream can change while the session runs.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{OnceLock, RwLock};
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

/// Proxies that exist because a peer serves one (see `egress`), by the peer's
/// URL, with the number of polls in a row the peer missed: they come and go with
/// the peers, and are never in the proxies file.
fn mesh_proxies() -> &'static RwLock<BTreeMap<String, (Proxy, u32)>> {
    static PROXIES: OnceLock<RwLock<BTreeMap<String, (Proxy, u32)>>> = OnceLock::new();
    PROXIES.get_or_init(Default::default)
}

/// Id of the proxy that goes through a node.
pub fn mesh_id(node: &str) -> String {
    format!("node:{node}")
}

/// The proxy through the SOCKS5 listener a peer advertises, logged in with the
/// mesh's login. `None` when the advertised address is not one a peer may have.
pub fn mesh_proxy(
    node: &str,
    peer_url: &str,
    advert: &crate::egress::Advert,
    login: &(String, String),
) -> Option<Proxy> {
    let host = match advert.host {
        Some(ref h) => h.clone(),
        None => Url::parse(peer_url).ok()?.host_str()?.to_string(),
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    // Like a peer's own address: a node cannot send its peers to this machine.
    if !crate::relay::peer_url_ok(&format!("http://{}", bracketed(&host))) {
        return None;
    }
    let proxy = Proxy {
        id: mesh_id(node),
        name: format!("{node} (node)"),
        url: format!(
            "socks5://{}:{}@{}:{}",
            login.0,
            login.1,
            bracketed(&host),
            advert.port
        ),
    };
    parse(&proxy.url).ok()?;
    Some(proxy)
}

fn bracketed(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Polls in a row a peer may miss before its proxy goes: one slow answer must
/// not make a working proxy flicker out of the lists.
const MISSES_ALLOWED: u32 = 3;

/// A peer answered: sets (or, with `None`, removes) its proxy.
pub fn set_mesh_proxy(peer_url: &str, proxy: Option<Proxy>) {
    let mut all = mesh_proxies().write().unwrap();
    match proxy {
        Some(p) => all.insert(peer_url.to_string(), (p, 0)),
        None => all.remove(peer_url),
    };
}

/// A peer did not answer: its proxy stays for a few polls, then goes.
pub fn miss_mesh_proxy(peer_url: &str) {
    let mut all = mesh_proxies().write().unwrap();
    if let Some(entry) = all.get_mut(peer_url) {
        entry.1 += 1;
        if entry.1 >= MISSES_ALLOWED {
            all.remove(peer_url);
        }
    }
}

/// Forgets the proxies of peers that are not in `peer_urls` any more.
pub fn retain_mesh_proxies(peer_urls: &[String]) {
    mesh_proxies()
        .write()
        .unwrap()
        .retain(|url, _| peer_urls.contains(url));
}

/// The node's own proxies, then the mesh's (an id the file has wins).
pub fn all(own: &[Proxy]) -> Vec<Proxy> {
    let mut all = own.to_vec();
    for (p, _) in mesh_proxies().read().unwrap().values() {
        if !all.iter().any(|o| o.id == p.id) {
            all.push(p.clone());
        }
    }
    all
}

pub fn find(own: &[Proxy], id: &str) -> Option<Proxy> {
    all(own).into_iter().find(|p| p.id == id)
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
    /// Username and password from the URL, for SOCKS5 (RFC 1929) and HTTP (Basic).
    pub auth: Option<(String, String)>,
}

fn parse(url: &str) -> Result<UpstreamAddr, String> {
    let u = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let (kind, default_port) = match u.scheme() {
        "socks5" | "socks5h" | "socks" => (Upstream::Socks5, 1080),
        "socks4" | "socks4a" => (Upstream::Socks4, 1080),
        "http" => (Upstream::Http, 80),
        other => return Err(format!("unsupported scheme {other}")),
    };
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .map(|c| c.into_owned())
            .map_err(|_| "credentials are not UTF-8".to_string())
    };
    let auth = if u.username().is_empty() && u.password().is_none() {
        None
    } else if kind == Upstream::Socks4 {
        return Err("SOCKS4 has no credentials".to_string());
    } else {
        let (user, pass) = (decode(u.username())?, decode(u.password().unwrap_or(""))?);
        // RFC 1929 carries each in a field with a one-byte length.
        if user.len() > 255 || pass.len() > 255 {
            return Err("credentials are too long".to_string());
        }
        Some((user, pass))
    };
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
        auth,
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

/// An entry of the proxies file: the URL inline, or in a file of its own
/// (`url_file`), which keeps a login out of the file and, with NixOS, out of the
/// Nix store (a sops-nix or agenix secret).
#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    url: Option<String>,
    url_file: Option<std::path::PathBuf>,
}

impl Entry {
    fn resolve(self) -> Result<Proxy, String> {
        let url = match (self.url, self.url_file) {
            (Some(url), None) => url,
            (None, Some(file)) => std::fs::read_to_string(&file)
                .map_err(|e| format!("proxy {:?}: {}: {e}", self.id, file.display()))?
                .trim()
                .to_string(),
            _ => return Err(format!("proxy {:?}: set one of url and url_file", self.id)),
        };
        Ok(Proxy {
            id: self.id,
            name: self.name,
            url,
        })
    }
}

/// Load and validate the node's proxies. Errors name the offending entry.
pub fn load(path: &Path) -> Result<Vec<Proxy>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let entries: Vec<Entry> =
        serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    validate(
        entries
            .into_iter()
            .map(Entry::resolve)
            .collect::<Result<_, _>>()?,
    )
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
    fn credentials_are_decoded_and_never_exposed() {
        let p = proxy("c", "socks5://alice:p%40ss%3A@node.example:1080");
        assert_eq!(
            p.upstream().auth,
            Some(("alice".to_string(), "p@ss:".to_string()))
        );
        let info = serde_json::to_string(&p.info()).unwrap();
        assert!(!info.contains("alice") && !info.contains("p@ss"));
    }

    #[test]
    fn validation_rejects_bad_entries() {
        assert!(validate(vec![proxy("none", "socks5://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://a:1"), proxy("a", "socks5://b:1")]).is_err());
        assert!(validate(vec![proxy("a", "ftp://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "https://a:1")]).is_err());
        assert!(validate(vec![proxy("a", "socks5://u:p@a:1")]).is_ok());
        assert!(validate(vec![proxy("a", "socks4://u@a:1")]).is_err());
        assert!(validate(vec![proxy("a", "http://a")]).is_ok());
    }

    #[test]
    fn peers_proxies_come_and_go_and_never_point_at_this_machine() {
        let login = ("mesh".to_string(), "pw".to_string());
        let advert = |host: Option<&str>| crate::egress::Advert {
            host: host.map(str::to_string),
            port: 1080,
        };
        let own = [proxy("vpn", "socks5://a:1")];

        let p = mesh_proxy(
            "rofl-13",
            "https://browser.rofl-13.example",
            &advert(None),
            &login,
        )
        .unwrap();
        assert_eq!(p.id, "node:rofl-13");
        assert_eq!(p.url, "socks5://mesh:pw@browser.rofl-13.example:1080");
        let v6 = mesh_proxy("x", "https://a.example", &advert(Some("fd00::5")), &login).unwrap();
        assert_eq!(v6.upstream().host, "fd00::5");
        assert!(mesh_proxy("x", "https://a.example", &advert(Some("127.0.0.1")), &login).is_none());
        assert!(mesh_proxy("x", "https://a.example", &advert(Some("localhost")), &login).is_none());

        set_mesh_proxy("https://peer-a.test", Some(p));
        let ids: Vec<_> = all(&own).into_iter().map(|p| p.id).collect();
        assert!(ids.contains(&"vpn".to_string()) && ids.contains(&"node:rofl-13".to_string()));
        assert!(find(&own, "node:rofl-13").is_some());
        // A file's id beats a peer's.
        set_mesh_proxy("https://peer-b.test", Some(proxy("vpn", "socks5://evil:1")));
        assert_eq!(find(&own, "vpn").unwrap().url, "socks5://a:1");
        retain_mesh_proxies(&[]);
        assert!(find(&own, "node:rofl-13").is_none());
    }

    #[test]
    fn a_peer_that_misses_a_poll_keeps_its_proxy_for_a_while() {
        let url = "https://flaky-peer.test";
        let p = proxy("node:flaky", "socks5://mesh:pw@flaky.test:1080");
        set_mesh_proxy(url, Some(p.clone()));
        miss_mesh_proxy(url);
        miss_mesh_proxy(url);
        assert!(find(&[], "node:flaky").is_some());
        // An answer starts the count again.
        set_mesh_proxy(url, Some(p));
        miss_mesh_proxy(url);
        miss_mesh_proxy(url);
        assert!(find(&[], "node:flaky").is_some());
        miss_mesh_proxy(url);
        assert!(find(&[], "node:flaky").is_none());
        // Nothing to miss for a peer without a proxy.
        miss_mesh_proxy("https://never-had-one.test");
    }

    #[test]
    fn a_url_can_live_in_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("exit-url");
        std::fs::write(&secret, "socks5://u:p@node.example:1080\n").unwrap();
        let list = dir.path().join("proxies.json");
        let write = |json: serde_json::Value| std::fs::write(&list, json.to_string()).unwrap();

        write(serde_json::json!([
            {"id": "a", "name": "A", "url_file": secret},
            {"id": "b", "name": "B", "url": "http://b:3128"}
        ]));
        let loaded = load(&list).unwrap();
        assert_eq!(loaded[0].url, "socks5://u:p@node.example:1080");
        assert_eq!(loaded[0].upstream().auth, Some(("u".into(), "p".into())));
        assert_eq!(loaded[1].url, "http://b:3128");

        // Both, neither, a missing or an empty file: each names the entry.
        write(serde_json::json!([{"id": "a", "name": "A", "url": "http://a", "url_file": secret}]));
        assert!(load(&list).unwrap_err().contains("\"a\""));
        write(serde_json::json!([{"id": "a", "name": "A"}]));
        assert!(load(&list).is_err());
        write(serde_json::json!([{"id": "a", "name": "A", "url_file": dir.path().join("none")}]));
        assert!(load(&list).unwrap_err().contains("\"a\""));
        std::fs::write(&secret, "\n").unwrap();
        write(serde_json::json!([{"id": "a", "name": "A", "url_file": secret}]));
        assert!(load(&list).is_err());
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
