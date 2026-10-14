//! Who drives a session: the automation clients (CDP, WebDriver BiDi) connected to
//! it, so the dashboard can tell when an agent is at work. Live views attach as
//! observers and aren't listed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClientInfo {
    /// What it is, from its User-Agent: "Playwright", "Puppeteer", "Node.js",
    /// "Python", or "other".
    pub kind: String,
    /// Playwright and Puppeteer: automation libraries, i.e. most likely an agent.
    pub agent: bool,
    /// Where it connects from: a mesh node's name (this node's own included), or an address.
    pub from: String,
    /// The address it connects from, as nginx saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Unix seconds.
    pub since: u64,
    /// What the agent's browser MCP reports (see the browser-mcp wrapper): the
    /// agent and version, its conversation id, working directory and machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// What it is doing, as the agent says (browser-agent-topic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Default)]
pub struct Clients {
    next: AtomicU64,
    list: Mutex<HashMap<u64, ClientInfo>>,
}

impl Clients {
    pub fn add(&self, info: ClientInfo) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.list.lock().unwrap().insert(id, info);
        id
    }

    pub fn remove(&self, id: u64) {
        self.list.lock().unwrap().remove(&id);
    }

    /// Label the clients of `conversation`; how many there were.
    pub fn set_topic(&self, conversation: &str, topic: Option<&str>) -> usize {
        let mut list = self.list.lock().unwrap();
        let mut n = 0;
        for c in list
            .values_mut()
            .filter(|c| c.conversation.as_deref() == Some(conversation))
        {
            c.topic = topic.map(str::to_string);
            n += 1;
        }
        n
    }

    pub fn list(&self) -> Vec<ClientInfo> {
        let mut v: Vec<_> = self.list.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|c| c.since);
        v
    }
}

pub fn kind_of(user_agent: Option<&str>) -> (&'static str, bool) {
    let ua = user_agent.unwrap_or("").to_ascii_lowercase();
    if ua.contains("playwright") {
        ("Playwright", true)
    } else if ua.contains("puppeteer") {
        ("Puppeteer", true)
    } else if ua.contains("node") {
        ("Node.js", false)
    } else if ua.contains("python") {
        ("Python", false)
    } else {
        ("other", false)
    }
}

/// The client's address as nginx saw it (it proxies every connection here).
pub fn remote_addr(headers: &HeaderMap) -> Option<IpAddr> {
    let real = headers.get("x-real-ip").and_then(|v| v.to_str().ok());
    let fwd = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next());
    real.or(fwd).and_then(|s| s.trim().parse().ok())
}

/// A mesh node's name for `addr` (resolving the nodes' hosts), `local` (this node's
/// name) for loopback, else the address itself.
pub async fn describe_origin(
    addr: Option<IpAddr>,
    local: &str,
    peers: &[(String, String)],
) -> String {
    let Some(addr) = addr else {
        return local.into();
    };
    if addr.is_loopback() {
        return local.into();
    }
    for (name, url) in peers {
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        else {
            continue;
        };
        let found = tokio::net::lookup_host(format!("{host}:443")).await;
        if found.is_ok_and(|mut addrs| addrs.any(|a| a.ip() == addr)) {
            return name.clone();
        }
    }
    addr.to_string()
}

/// A reported value: trimmed, at most 200 characters, None if empty.
pub fn clean(value: Option<&str>) -> Option<String> {
    let v: String = value?
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    (!v.is_empty()).then_some(v)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_user_agents() {
        assert_eq!(
            kind_of(Some("Playwright/1.56.0 (x64; nixos) node/22.0")),
            ("Playwright", true)
        );
        assert_eq!(kind_of(Some("Mozilla/5.0 Puppeteer")), ("Puppeteer", true));
        assert_eq!(
            kind_of(Some("Python/3.13 websockets/15.0")),
            ("Python", false)
        );
        assert_eq!(kind_of(None), ("other", false));
    }

    #[test]
    fn origin_from_proxy_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "100.122.1.2, 10.0.0.1".parse().unwrap());
        assert_eq!(remote_addr(&h), Some("100.122.1.2".parse().unwrap()));
        h.insert("x-real-ip", "100.122.9.9".parse().unwrap());
        assert_eq!(remote_addr(&h), Some("100.122.9.9".parse().unwrap()));
    }
}
