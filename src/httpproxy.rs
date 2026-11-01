//! An HTTP proxy for the browsers of paired extensions: their traffic leaves
//! through one of this node's proxies (the proxies file, or a peer's SOCKS5
//! egress, `node:<name>`) instead of straight from the machine they run on.
//! Opt-in (`--http-proxy-listen`). The browser logs in with Basic credentials:
//! the user names the proxy (percent-encoded; empty or `direct` for none), the
//! password is the node's login (derived from the cluster secret, handed to a
//! paired extension over its connection). A proxy that is unknown or down fails
//! the connection, never falls back to a direct one. Destinations are limited
//! like a session relay's (`relay::Policy::strict`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

use crate::egress::{self, Network};
use crate::proxies::{self, Proxy};
use crate::relay::{self, Policy};

const HEAD_LIMIT: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// What the extension needs to point its browser at the listener.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct Offer {
    /// Where the browser reaches it; absent: the host it knows the daemon by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub port: u16,
    pub password: String,
}

/// The password a mesh's nodes accept on their HTTP proxies: derived from the
/// cluster secret, and useless without it.
pub fn login_password(cluster_secret: &str) -> String {
    egress::derive_password(cluster_secret, b"browser-daemon http proxy login")
}

pub struct Config {
    pub listen: SocketAddr,
    /// Clients outside these networks are dropped without a word.
    pub allow: Vec<Network>,
    /// What clients log in with; none: only loopback may listen without one.
    pub password: Option<String>,
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        if self.password.is_some() || self.listen.ip().is_loopback() {
            return Ok(());
        }
        Err(format!(
            "--http-proxy-listen {} would be an open proxy: it needs a cluster secret or --http-proxy-password-file",
            self.listen
        ))
    }
}

/// Binds the listener and serves it until the returned task is dropped; `own`
/// is the node's proxies file (peers' proxies are looked up live).
pub async fn start(
    config: Config,
    own: Vec<Proxy>,
    policy: Policy,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    config
        .check()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let listener = TcpListener::bind(config.listen).await?;
    info!(
        listen = %listener.local_addr()?,
        login = config.password.is_some(),
        allowed = config.allow.len(),
        "HTTP proxy listening"
    );
    let (config, own) = (Arc::new(config), Arc::new(own));
    Ok(tokio::spawn(async move {
        while let Ok((client, peer)) = listener.accept().await {
            if !config.allow.is_empty() && !config.allow.iter().any(|n| n.contains(peer.ip())) {
                debug!(peer = %peer, "HTTP proxy client outside the allowed networks");
                continue;
            }
            let (config, own) = (config.clone(), own.clone());
            tokio::spawn(async move {
                let _ = client.set_nodelay(true);
                if let Err(e) = serve(client, &config, &own, policy).await {
                    info!(peer = %peer, error = %e, "HTTP proxy connection refused or failed");
                }
            });
        }
    }))
}

/// A request's first line and headers, and what was read past them.
struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_head(client: &mut TcpStream) -> std::io::Result<Head> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > HEAD_LIMIT {
            return Err(invalid("request head too large"));
        }
        let mut chunk = [0u8; 2048];
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Err(invalid("closed before a request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let rest = buf.split_off(end + 4);
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| invalid("request is not text"))?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(invalid("bad request line"));
    };
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        rest,
    })
}

/// The proxy's name from the login, if the password is right. The password is
/// known, so it is taken off the end: an id with a colon (`node:name`) works
/// whether the client percent-encoded it or sent it as it is.
fn login(head: &Head, password: Option<&str>) -> Option<String> {
    let raw = head.header("proxy-authorization").and_then(basic);
    let Some(password) = password else {
        // Loopback without a password: whoever is there may use it; the user
        // part still names the proxy, if the browser sent one.
        let user = raw
            .as_deref()
            .and_then(|r| r.split_once(':'))
            .map_or("", |(u, _)| u);
        return decode(user);
    };
    let raw = raw?;
    let at = raw.len().checked_sub(password.len() + 1)?;
    let ok = raw.as_bytes()[at] == b':'
        && relay::constant_time_eq(&raw.as_bytes()[at + 1..], password.as_bytes());
    ok.then(|| decode(raw.get(..at)?)).flatten()
}

/// `user:password` of a Basic header.
fn basic(header: &str) -> Option<String> {
    let token = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(token.trim())
        .ok()?;
    String::from_utf8(raw).ok()
}

fn decode(user: &str) -> Option<String> {
    percent_encoding::percent_decode_str(user)
        .decode_utf8()
        .ok()
        .map(|u| u.into_owned())
}

/// An error answer that says what went wrong (no login or address in it), in a
/// header and in the body, so a browser's error page tells which hop failed.
async fn reply(
    client: &mut TcpStream,
    code: u16,
    reason: &str,
    detail: &str,
) -> std::io::Result<()> {
    let challenge = if code == 407 {
        "Proxy-Authenticate: Basic realm=\"browser-daemon\"\r\n"
    } else {
        ""
    };
    let detail: String = detail
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(200)
        .collect();
    client
        .write_all(
            format!(
                "HTTP/1.1 {code} {reason}\r\n{challenge}X-Browser-Daemon-Error: {detail}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{detail}",
                detail.len()
            )
            .as_bytes(),
        )
        .await
}

async fn serve(
    mut client: TcpStream,
    config: &Config,
    own: &[Proxy],
    policy: Policy,
) -> std::io::Result<()> {
    let head = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "request head"))??;
    let Some(user) = login(&head, config.password.as_deref()) else {
        reply(
            &mut client,
            407,
            "Proxy Authentication Required",
            "wrong or missing login",
        )
        .await?;
        return Err(invalid("wrong credentials"));
    };
    let upstream = if proxies::DIRECT.contains(&user.trim()) {
        None
    } else if let Some(proxy) = proxies::find(own, user.trim()) {
        Some(proxy)
    } else {
        reply(
            &mut client,
            502,
            "Unknown Proxy",
            &format!("this node does not know the proxy {:?}", user.trim()),
        )
        .await?;
        return Err(invalid("unknown proxy"));
    };

    let connect = head.method.eq_ignore_ascii_case("CONNECT");
    let (host, port) = if connect {
        split_authority(&head.target, 443)
    } else {
        forward_authority(&head.target)
    }
    .ok_or_else(|| invalid("bad destination"))?;

    let remote = match relay::open_host(upstream.as_ref(), &host, port, policy).await {
        Ok(s) => s,
        Err(e) => {
            let (code, reason) = match e.kind() {
                std::io::ErrorKind::PermissionDenied => (403, "Forbidden"),
                std::io::ErrorKind::TimedOut => (504, "Gateway Timeout"),
                _ => (502, "Bad Gateway"),
            };
            reply(&mut client, code, reason, &format!("{host}:{port}: {e}")).await?;
            return Err(e);
        }
    };
    let mut remote = remote;
    if connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        // A client that spoke first (TLS hello) is not lost.
        remote.write_all(&head.rest).await?;
    } else {
        remote.write_all(&forwarded_head(&head)).await?;
        remote.write_all(&head.rest).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut remote)
        .await
        .map(|_| ())
}

/// `host:port` or `[v6]:port` (port optional, then `default`).
fn split_authority(authority: &str, default: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') || h.starts_with('[') => {
            (h.to_string(), p.parse::<u16>().ok()?)
        }
        _ => (authority.to_string(), default),
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    (!host.is_empty() && port != 0).then_some((host, port))
}

/// Where an absolute-form request (`GET http://host/path`) goes. Only plain
/// HTTP comes this way; HTTPS arrives as CONNECT.
fn forward_authority(target: &str) -> Option<(String, u16)> {
    let url = url::Url::parse(target).ok()?;
    (url.scheme() == "http").then_some(())?;
    Some((
        url.host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        url.port_or_known_default()?,
    ))
}

/// The request as the origin server wants it: origin form, no proxy headers,
/// and closed after one answer, so the tunnel never carries a second request
/// the proxy has not looked at.
fn forwarded_head(head: &Head) -> Vec<u8> {
    let path = url::Url::parse(&head.target)
        .map(|u| match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        })
        .unwrap_or_else(|_| "/".to_string());
    let mut out = format!("{} {path} {}\r\n", head.method, head.version);
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
        ) {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    async fn echo_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    /// Answers every request with `ok`, after sending what it got to `seen`.
    async fn origin(seen: tokio::sync::mpsc::UnboundedSender<String>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let _ = seen.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        port
    }

    async fn listener(password: Option<&str>, own: Vec<Proxy>, policy: Policy) -> u16 {
        let port = free_port().await;
        let config = Config {
            listen: ([127, 0, 0, 1], port).into(),
            allow: vec![],
            password: password.map(String::from),
        };
        start(config, own, policy).await.unwrap();
        port
    }

    fn basic(user: &str, pass: &str) -> String {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Proxy-Authorization: Basic {token}\r\n")
    }

    /// Sends a request head, returns what comes back first.
    async fn ask(port: u16, request: String) -> (TcpStream, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .expect("an answer")
            .unwrap();
        (s, String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    fn connect_to(port: u16, login: &str) -> String {
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{login}\r\n")
    }

    #[test]
    fn the_login_follows_the_secret() {
        assert_eq!(login_password("one"), login_password("one"));
        assert_ne!(login_password("one"), login_password("two"));
        // Not the SOCKS5 login of the same mesh.
        assert_ne!(login_password("one"), egress::mesh_login("one").1);
    }

    #[test]
    fn a_listener_beyond_loopback_needs_a_password() {
        let mut c = Config {
            listen: "0.0.0.0:3128".parse().unwrap(),
            allow: vec![],
            password: None,
        };
        assert!(c.check().is_err());
        c.password = Some("x".into());
        assert!(c.check().is_ok());
        c.password = None;
        c.listen = "127.0.0.1:3128".parse().unwrap();
        assert!(c.check().is_ok());
    }

    #[test]
    fn authorities_are_split() {
        assert_eq!(split_authority("a.b:8443", 443), Some(("a.b".into(), 8443)));
        assert_eq!(split_authority("a.b", 443), Some(("a.b".into(), 443)));
        assert_eq!(split_authority("[::1]:80", 443), Some(("::1".into(), 80)));
        assert_eq!(split_authority("a.b:0", 443), None);
        assert_eq!(split_authority(":80", 443), None);
        assert_eq!(
            forward_authority("http://a.b/x?y=1"),
            Some(("a.b".into(), 80))
        );
        assert_eq!(forward_authority("https://a.b/"), None);
    }

    #[tokio::test]
    async fn without_the_password_the_browser_is_asked_for_one() {
        let echo = echo_server().await;
        let port = listener(Some("secret"), vec![], Policy::permissive()).await;
        let (_s, answer) = ask(port, connect_to(echo, "")).await;
        assert!(answer.starts_with("HTTP/1.1 407"), "{answer}");
        assert!(answer.contains("Proxy-Authenticate: Basic"));
        let (_s, answer) = ask(port, connect_to(echo, &basic("direct", "wrong"))).await;
        assert!(answer.starts_with("HTTP/1.1 407"), "{answer}");
    }

    #[tokio::test]
    async fn a_direct_tunnel_carries_bytes() {
        let echo = echo_server().await;
        let port = listener(Some("secret"), vec![], Policy::permissive()).await;
        let (mut s, answer) = ask(port, connect_to(echo, &basic("direct", "secret"))).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
    }

    #[tokio::test]
    async fn the_user_picks_a_proxy_and_an_unknown_one_never_goes_direct() {
        let echo = echo_server().await;
        // A SOCKS5 egress stands in for a peer.
        let socks = free_port().await;
        egress::start(
            egress::Config {
                listen: ([127, 0, 0, 1], socks).into(),
                allow: vec![],
                credentials: vec![],
            },
            Policy::permissive(),
        )
        .await
        .unwrap();
        let own = vec![Proxy {
            id: "node:peer".into(),
            name: "peer (node)".into(),
            url: format!("socks5://127.0.0.1:{socks}"),
        }];
        let port = listener(Some("secret"), own, Policy::permissive()).await;
        // The colon of the id travels percent-encoded.
        let (mut s, answer) = ask(port, connect_to(echo, &basic("node%3Apeer", "secret"))).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        s.write_all(b"via").await.unwrap();
        let mut back = [0u8; 3];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"via");
        let (_s, answer) = ask(port, connect_to(echo, &basic("node%3Anobody", "secret"))).await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    }

    #[tokio::test]
    async fn an_id_with_a_colon_works_encoded_or_not() {
        let echo = echo_server().await;
        let socks = free_port().await;
        egress::start(
            egress::Config {
                listen: ([127, 0, 0, 1], socks).into(),
                allow: vec![],
                credentials: vec![],
            },
            Policy::permissive(),
        )
        .await
        .unwrap();
        let own = vec![Proxy {
            id: "node:peer".into(),
            name: "peer (node)".into(),
            url: format!("socks5://127.0.0.1:{socks}"),
        }];
        let port = listener(Some("se:cret"), own, Policy::permissive()).await;
        for user in ["node:peer", "node%3Apeer"] {
            let (_s, answer) = ask(port, connect_to(echo, &basic(user, "se:cret"))).await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{user}: {answer}");
        }
        let (_s, answer) = ask(port, connect_to(echo, &basic("node:peer", "se:wrong"))).await;
        assert!(answer.starts_with("HTTP/1.1 407"), "{answer}");
    }

    #[tokio::test]
    async fn a_strict_listener_refuses_loopback_and_machine_names() {
        let echo = echo_server().await;
        let port = listener(Some("secret"), vec![], Policy::strict()).await;
        let (_s, answer) = ask(port, connect_to(echo, &basic("direct", "secret"))).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    }

    #[tokio::test]
    async fn plain_http_is_forwarded_in_origin_form_without_the_login() {
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let origin = origin(tx).await;
        let port = listener(Some("secret"), vec![], Policy::permissive()).await;
        let request = format!(
            "GET http://127.0.0.1:{origin}/a/b?c=1 HTTP/1.1\r\nHost: 127.0.0.1:{origin}\r\n{}Proxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n",
            basic("direct", "secret")
        );
        let (_s, answer) = ask(port, request).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.ends_with("ok"));
        let got = seen.recv().await.unwrap();
        assert!(got.starts_with("GET /a/b?c=1 HTTP/1.1\r\n"), "{got}");
        assert!(got.contains("Accept: */*"));
        assert!(got.contains("Connection: close"));
        assert!(!got.to_lowercase().contains("proxy-"), "{got}");
        assert!(!got.contains("secret"));
    }

    #[tokio::test]
    async fn a_loopback_listener_without_a_password_still_honours_the_user() {
        let echo = echo_server().await;
        let port = listener(None, vec![], Policy::permissive()).await;
        let (_s, answer) = ask(port, connect_to(echo, "")).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        let (_s, answer) = ask(port, connect_to(echo, &basic("nope", "x"))).await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    }
}
