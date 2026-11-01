//! A SOCKS5 listener other nodes (or any SOCKS5 client) can use as their
//! upstream proxy: the connections leave from this node's network. Opt-in
//! (`--socks-listen`); only clients from the allowed networks get an answer, and
//! with credentials only those who log in. Destinations are limited like a
//! session relay's (`relay::Policy::strict`): never this machine, link-local
//! addresses or the mesh's daemons.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::relay::{self, Policy};

/// An address range: `10.0.0.0/8`, `fd00::/8`, or a single address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    addr: IpAddr,
    prefix: u8,
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .parse::<IpAddr>()
            .map_err(|_| format!("{s}: not an address or range"))?
            .to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("{s}: bad prefix length"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
}

impl Network {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_matches(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn prefix_matches(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let rest = prefix % 8;
    net[..full] == ip[..full] && (rest == 0 || (net[full] ^ ip[full]) >> (8 - rest) == 0)
}

/// `user:password`, as in a credentials file.
pub fn parse_credentials(raw: &str) -> Result<(String, String), String> {
    let (user, pass) = raw
        .trim_end_matches(['\r', '\n'])
        .split_once(':')
        .ok_or("expected user:password")?;
    if user.is_empty() || pass.is_empty() {
        return Err("user and password must not be empty".to_string());
    }
    if user.len() > 255 || pass.len() > 255 {
        return Err("user and password must each fit in 255 bytes".to_string());
    }
    Ok((user.to_string(), pass.to_string()))
}

/// What a node tells its peers about its listener (in its node info).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Advert {
    /// Where peers reach it; absent: the host they know the node by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub port: u16,
}

/// The login the nodes of a mesh use on each other's listeners: derived from the
/// cluster secret, so nothing has to be configured per node, and useless without it.
pub fn mesh_login(cluster_secret: &str) -> (String, String) {
    (
        "mesh".to_string(),
        derive_password(cluster_secret, b"browser-daemon socks5 egress login"),
    )
}

/// A password for one purpose (`label`) from the cluster secret: 16 bytes of
/// HMAC-SHA256, hex.
pub fn derive_password(cluster_secret: &str, label: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(cluster_secret.as_bytes()).expect("any key length");
    mac.update(label);
    let tag = mac.finalize().into_bytes();
    tag.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

pub struct Config {
    pub listen: SocketAddr,
    /// Clients outside these networks are dropped without a word.
    pub allow: Vec<Network>,
    /// Logins that are accepted; none: no login asked for.
    pub credentials: Vec<(String, String)>,
}

impl Config {
    /// What a listener needs to be safe to start: anyone who can reach it must
    /// be limited by network or by login, unless it only listens on loopback.
    pub fn check(&self) -> Result<(), String> {
        if self.listen.ip().is_loopback() || !self.allow.is_empty() || !self.credentials.is_empty()
        {
            return Ok(());
        }
        Err(format!(
            "--socks-listen {} would be an open proxy: give --socks-allow or --socks-credentials-file",
            self.listen
        ))
    }
}

/// Binds the listener and serves it until the returned task is dropped.
pub async fn start(config: Config, policy: Policy) -> std::io::Result<tokio::task::JoinHandle<()>> {
    config
        .check()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let listener = TcpListener::bind(config.listen).await?;
    info!(
        listen = %listener.local_addr()?,
        login = !config.credentials.is_empty(),
        allowed = config.allow.len(),
        "SOCKS5 egress listening"
    );
    let config = Arc::new(config);
    Ok(tokio::spawn(async move {
        // Direct connections only, and the route never changes: the sender
        // stays alive here so the receivers never report a change.
        let (_direct, rx) = watch::channel(None);
        while let Ok((client, peer)) = listener.accept().await {
            if !config.allow.is_empty() && !config.allow.iter().any(|n| n.contains(peer.ip())) {
                debug!(peer = %peer, "SOCKS5 egress client outside the allowed networks");
                continue;
            }
            let (rx, config) = (rx.clone(), config.clone());
            tokio::spawn(async move {
                let _ = client.set_nodelay(true);
                if let Err(e) = relay::serve_as(client, rx, policy, &config.credentials).await {
                    debug!(peer = %peer, error = %e, "SOCKS5 egress connection ended");
                }
            });
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn net(s: &str) -> Network {
        s.parse().unwrap()
    }

    #[test]
    fn networks_match_by_prefix() {
        assert!(net("10.0.0.0/8").contains("10.200.1.2".parse().unwrap()));
        assert!(!net("10.0.0.0/8").contains("11.0.0.1".parse().unwrap()));
        assert!(net("192.168.4.0/22").contains("192.168.7.255".parse().unwrap()));
        assert!(!net("192.168.4.0/22").contains("192.168.8.0".parse().unwrap()));
        assert!(net("203.0.113.9").contains("203.0.113.9".parse().unwrap()));
        assert!(!net("203.0.113.9").contains("203.0.113.8".parse().unwrap()));
        assert!(net("fd00::/8").contains("fd12::1".parse().unwrap()));
        assert!(!net("fd00::/8").contains("fe80::1".parse().unwrap()));
        // A v4-mapped client matches its v4 range; families never mix otherwise.
        assert!(net("10.0.0.0/8").contains("::ffff:10.1.1.1".parse().unwrap()));
        assert!(!net("10.0.0.0/8").contains("fd00::1".parse().unwrap()));
        assert!(net("0.0.0.0/0").contains("8.8.8.8".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Network>().is_err());
        assert!("nope".parse::<Network>().is_err());
    }

    #[test]
    fn credentials_need_both_parts() {
        assert_eq!(
            parse_credentials("a:b:c\n").unwrap(),
            ("a".to_string(), "b:c".to_string())
        );
        assert!(parse_credentials("nocolon").is_err());
        assert!(parse_credentials("user:").is_err());
    }

    #[test]
    fn the_mesh_login_follows_the_secret() {
        let a = mesh_login("one");
        assert_eq!(a, mesh_login("one"));
        assert_ne!(a.1, mesh_login("two").1);
        assert_eq!(a.0, "mesh");
        assert_eq!(a.1.len(), 32);
    }

    #[test]
    fn an_open_listener_is_refused() {
        let mut c = Config {
            listen: "0.0.0.0:1080".parse().unwrap(),
            allow: vec![],
            credentials: vec![],
        };
        assert!(c.check().is_err());
        c.allow.push(net("10.0.0.0/8"));
        assert!(c.check().is_ok());
        c.allow.clear();
        c.credentials = vec![("u".into(), "p".into())];
        assert!(c.check().is_ok());
        c.credentials.clear();
        c.listen = "127.0.0.1:1080".parse().unwrap();
        assert!(c.check().is_ok());
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

    async fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    /// Logs in (or not) and asks for `127.0.0.1:port`; returns the reply code.
    async fn connect_via(
        listen: u16,
        login: Option<(&str, &str)>,
        port: u16,
    ) -> std::io::Result<u8> {
        let mut s = TcpStream::connect(("127.0.0.1", listen)).await?;
        match login {
            None => s.write_all(&[5, 1, 0]).await?,
            Some((user, pass)) => {
                s.write_all(&[5, 1, 2]).await?;
                let mut m = [0u8; 2];
                s.read_exact(&mut m).await?;
                if m != [5, 2] {
                    return Ok(0xff);
                }
                let mut l = vec![1, user.len() as u8];
                l.extend(user.as_bytes());
                l.push(pass.len() as u8);
                l.extend(pass.as_bytes());
                s.write_all(&l).await?;
                let mut ok = [0u8; 2];
                s.read_exact(&mut ok).await?;
                if ok[1] != 0 {
                    return Ok(0xfe);
                }
                return finish(&mut s, port).await;
            }
        }
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await?;
        if m != [5, 0] {
            return Ok(0xff);
        }
        finish(&mut s, port).await
    }

    async fn finish(s: &mut TcpStream, port: u16) -> std::io::Result<u8> {
        let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
        req.extend(port.to_be_bytes());
        s.write_all(&req).await?;
        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await?;
        if rep[1] == 0 {
            s.write_all(b"ping").await?;
            let mut echo = [0u8; 4];
            s.read_exact(&mut echo).await?;
            assert_eq!(&echo, b"ping");
        }
        Ok(rep[1])
    }

    #[tokio::test]
    async fn serves_logged_in_clients_from_allowed_networks_only() {
        let echo = echo_server().await;
        let listen = free_port().await;
        let config = |allow: Vec<Network>| Config {
            listen: format!("127.0.0.1:{listen}").parse().unwrap(),
            allow,
            credentials: vec![("node".into(), "s3cret".into())],
        };

        // Loopback targets are for tests only: a permissive policy.
        let task = start(config(vec![net("127.0.0.0/8")]), Policy::permissive())
            .await
            .unwrap();
        assert_eq!(
            connect_via(listen, Some(("node", "s3cret")), echo)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            connect_via(listen, Some(("node", "wrong")), echo)
                .await
                .unwrap(),
            0xfe
        );
        assert_eq!(
            connect_via(listen, Some(("other", "s3cret")), echo)
                .await
                .unwrap(),
            0xfe
        );
        // No login offered: refused.
        assert_eq!(connect_via(listen, None, echo).await.unwrap(), 0xff);
        task.abort();
        let _ = task.await;

        // Outside the allowed networks: dropped before the handshake.
        let task = start(config(vec![net("10.0.0.0/8")]), Policy::permissive())
            .await
            .unwrap();
        assert!(connect_via(listen, Some(("node", "s3cret")), echo)
            .await
            .is_err());
        task.abort();
    }

    #[tokio::test]
    async fn a_relay_logs_in_to_a_node_through_its_proxy_url() {
        let echo = echo_server().await;
        let listen = free_port().await;
        let serve = |login: Vec<(String, String)>| Config {
            listen: format!("127.0.0.1:{listen}").parse().unwrap(),
            allow: vec![],
            credentials: login,
        };
        let task = start(
            serve(vec![("node".into(), "s3cret".into())]),
            Policy::permissive(),
        )
        .await
        .unwrap();
        let through = |url: String| async move {
            let proxy = crate::proxies::Proxy {
                id: "n".into(),
                name: "n".into(),
                url,
            };
            let relay = relay::Relay::start_with(0, Some(proxy), Policy::permissive())
                .await
                .unwrap();
            // The relay's own clients are plain SOCKS5.
            connect_via(relay.port(), None, echo).await
        };
        let good = format!("socks5://node:s3cret@127.0.0.1:{listen}");
        assert_eq!(through(good).await.unwrap(), 0);
        let bad = format!("socks5://node:nope@127.0.0.1:{listen}");
        assert_eq!(through(bad).await.unwrap(), 5);
        let anonymous = format!("socks5://127.0.0.1:{listen}");
        assert_eq!(through(anonymous).await.unwrap(), 5);
        task.abort();
    }

    #[tokio::test]
    async fn a_strict_listener_refuses_this_machine() {
        let echo = echo_server().await;
        let listen = free_port().await;
        let task = start(
            Config {
                listen: format!("127.0.0.1:{listen}").parse().unwrap(),
                allow: vec![],
                credentials: vec![],
            },
            Policy::strict(),
        )
        .await
        .unwrap();
        // 0x02: not allowed by the ruleset.
        assert_eq!(connect_via(listen, None, echo).await.unwrap(), 2);
        task.abort();
    }
}
