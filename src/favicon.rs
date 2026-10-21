//! Site icons for the dashboard and the live view. The node fetches them, so the
//! viewer's browser never talks to the sites (or to an icon service) itself.
//!
//! The request names a page; the icon is looked up per origin: `/favicon.ico`
//! first, then the `<link rel="icon">` of the page. Only public http(s) hosts are
//! contacted (this is an authenticated fetch-by-URL, so loopback, private and
//! link-local addresses are refused, also after redirects and DNS), the answer
//! is capped in size, and what is served is sniffed to be a raster image. Results
//! (and failures, for a shorter time) are remembered per origin.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use url::Url;

const MAX_ICON_BYTES: usize = 256 * 1024;
const MAX_HTML_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 3;
const MAX_CANDIDATES: usize = 4;
const MAX_ENTRIES: usize = 512;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const OK_TTL: Duration = Duration::from_secs(6 * 3600);
const FAIL_TTL: Duration = Duration::from_secs(10 * 60);

pub struct Icon {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

struct Entry {
    at: Instant,
    icon: Option<(&'static str, std::sync::Arc<Vec<u8>>)>,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gate() -> &'static Semaphore {
    static GATE: OnceLock<Semaphore> = OnceLock::new();
    GATE.get_or_init(|| Semaphore::new(4))
}

/// The icon of the site a page belongs to; `None` when there is none (or the
/// page is not a public web address). Failures are remembered for a while.
pub async fn lookup(page_url: &str) -> Option<Icon> {
    let url = parse_public_url(page_url)?;
    let key = origin_key(&url);
    {
        let cache = cache().lock().ok()?;
        if let Some(e) = cache.get(&key) {
            let ttl = if e.icon.is_some() { OK_TTL } else { FAIL_TTL };
            if e.at.elapsed() < ttl {
                return e.icon.as_ref().map(|(mime, b)| Icon {
                    mime,
                    bytes: b.as_ref().clone(),
                });
            }
        }
    }
    let _permit = gate().acquire().await.ok()?;
    let found = find(&url).await;
    if let Ok(mut cache) = cache().lock() {
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, e| e.at.elapsed() < FAIL_TTL);
        }
        if cache.len() >= MAX_ENTRIES {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            Entry {
                at: Instant::now(),
                icon: found
                    .as_ref()
                    .map(|i| (i.mime, std::sync::Arc::new(i.bytes.clone()))),
            },
        );
    }
    found
}

/// Fetch a browser-reported favicon URL through the same public-address checks as
/// page-derived icons. Extension tab metadata is untrusted, so validate it here too.
pub async fn lookup_icon(icon_url: &str) -> Option<Icon> {
    let url = parse_public_url(icon_url)?;
    let _permit = gate().acquire().await.ok()?;
    fetch_icon(&url).await
}

fn origin_key(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or(0)
    )
}

/// http(s) only, no credentials, a host.
fn parse_public_url(raw: &str) -> Option<Url> {
    if raw.len() > 2048 {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.host_str()?;
    Some(url)
}

async fn find(page: &Url) -> Option<Icon> {
    if let Ok(ico) = page.join("/favicon.ico") {
        if let Some(icon) = fetch_icon(&ico).await {
            return Some(icon);
        }
    }
    let (final_url, html) = fetch(page, MAX_HTML_BYTES).await?;
    let html = String::from_utf8_lossy(&html);
    for href in icon_links(&html).into_iter().take(MAX_CANDIDATES) {
        let Ok(url) = final_url.join(&href) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        if let Some(icon) = fetch_icon(&url).await {
            return Some(icon);
        }
    }
    None
}

async fn fetch_icon(url: &Url) -> Option<Icon> {
    let (_, bytes) = fetch(url, MAX_ICON_BYTES).await?;
    let mime = sniff(&bytes)?;
    Some(Icon { mime, bytes })
}

/// GET with the size cap and a manual redirect loop: every hop is checked, and the
/// connection goes to the address that was checked.
async fn fetch(start: &Url, max: usize) -> Option<(Url, Vec<u8>)> {
    let mut url = start.clone();
    for _ in 0..=MAX_REDIRECTS {
        let host = url.host_str()?.to_string();
        let port = url.port_or_known_default()?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .user_agent("Mozilla/5.0 (compatible; browser-daemon favicon)");
        match url.host()? {
            url::Host::Ipv4(ip) => {
                if !is_public(IpAddr::V4(ip)) {
                    return None;
                }
            }
            url::Host::Ipv6(ip) => {
                if !is_public(IpAddr::V6(ip)) {
                    return None;
                }
            }
            url::Host::Domain(d) => {
                let addrs: Vec<SocketAddr> = tokio::time::timeout(
                    FETCH_TIMEOUT,
                    tokio::net::lookup_host((d.to_string(), port)),
                )
                .await
                .ok()?
                .ok()?
                .collect();
                if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
                    return None;
                }
                builder = builder.resolve_to_addrs(&host, &addrs);
            }
        }
        let mut res = builder.build().ok()?.get(url.clone()).send().await.ok()?;
        if res.status().is_redirection() {
            let next = res
                .headers()
                .get(reqwest::header::LOCATION)?
                .to_str()
                .ok()?;
            url = parse_public_url(url.join(next).ok()?.as_str())?;
            continue;
        }
        if !res.status().is_success() {
            return None;
        }
        if res.content_length().is_some_and(|n| n > max as u64) {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if body.len() + chunk.len() > max {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        return Some((url, body));
    }
    None
}

/// The raster formats browsers use as site icons, by their magic bytes (not by what
/// the server claims). SVG is left out: it can carry script.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if b.starts_with(b"GIF8") {
        Some("image/gif")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10 (CGNAT, tailscale)
        || (o[0] == 198 && (o[1] & 0xFE) == 18) // 198.18.0.0/15 (benchmarking)
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xFE00) == 0xFC00 // fc00::/7 unique local
        || (s[0] & 0xFFC0) == 0xFE80 // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b)) // NAT64
}

/// The `href`s of the page's `<link rel="... icon ...">` tags, regular icons
/// before touch icons.
fn icon_links(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut plain = Vec::new();
    let mut touch = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<link") {
        let start = pos + i + 5;
        let Some(end) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + end];
        pos = start + end;
        let (mut rel, mut href) = (None, None);
        for (name, value) in attributes(tag) {
            match name.as_str() {
                "rel" => rel = Some(value.to_ascii_lowercase()),
                "href" => href = Some(value),
                _ => {}
            }
        }
        let (Some(rel), Some(href)) = (rel, href) else {
            continue;
        };
        let tokens: Vec<&str> = rel.split_whitespace().collect();
        if tokens.contains(&"icon") {
            plain.push(href);
        } else if tokens.contains(&"apple-touch-icon") {
            touch.push(href);
        }
    }
    plain.extend(touch);
    plain
}

/// `name="value"` pairs of a tag's attributes (quoted or bare; names lower-cased).
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let s = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = tag[s..i].to_string();
                i += 1;
            } else {
                let s = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = tag[s..i].to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.5",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn only_plain_web_addresses() {
        assert!(parse_public_url("https://example.com/a?b=1").is_some());
        assert!(parse_public_url("http://example.com:8080/").is_some());
        assert!(parse_public_url("file:///etc/passwd").is_none());
        assert!(parse_public_url("javascript:alert(1)").is_none());
        assert!(parse_public_url("about:blank").is_none());
        assert!(parse_public_url("https://user:pw@example.com/").is_none());
    }

    #[test]
    fn icon_links_are_found() {
        let html = r#"<html><head><LINK rel="apple-touch-icon" href="/t.png">
            <link href='/fav.png' rel='shortcut icon' type=image/png><link rel=stylesheet href=/a.css>
            <link rel=icon href=/b.ico></head>"#;
        assert_eq!(icon_links(html), vec!["/fav.png", "/b.ico", "/t.png"]);
    }

    #[test]
    fn images_are_sniffed_not_trusted() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0]), Some("image/x-icon"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"<html>"), None);
    }
}
