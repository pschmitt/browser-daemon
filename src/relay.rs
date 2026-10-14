//! A per-session SOCKS5 relay on 127.0.0.1. The browser always uses it as its
//! proxy; the relay connects directly or through the session's upstream proxy,
//! which can change at any time: open connections are dropped on a change, so
//! the browser reconnects through the new route.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::proxies::{Proxy, Upstream};

/// Per address tried: an address that silently drops packets must not stall the rest.
const CONNECT_ATTEMPT: Duration = Duration::from_secs(4);
const HANDSHAKE: Duration = Duration::from_secs(15);

pub struct Relay {
    port: u16,
    upstream: watch::Sender<Option<Proxy>>,
    accept: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Relay {
    pub async fn start(upstream: Option<Proxy>) -> io::Result<Arc<Self>> {
        Self::start_on(0, upstream).await
    }

    /// On a given port (0: any): a browser kept running across a daemon restart
    /// still points at the old one.
    pub async fn start_on(port: u16, upstream: Option<Proxy>) -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = watch::channel(upstream);
        let accept = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let rx = rx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(client, rx).await {
                        debug!(error = %e, "Relay connection ended");
                    }
                });
            }
        });
        Ok(Arc::new(Self {
            port,
            upstream: tx,
            accept,
        }))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Id of the upstream proxy, `None` for direct connections.
    pub fn current(&self) -> Option<String> {
        self.upstream.borrow().as_ref().map(|p| p.id.clone())
    }

    /// Switch the upstream; every open connection is closed.
    pub fn set(&self, upstream: Option<Proxy>) {
        info!(port = self.port, upstream = ?upstream.as_ref().map(|p| &p.id), "Switching session proxy");
        self.upstream.send_replace(upstream);
    }
}

/// Where the browser wants to go.
enum Target {
    Ip(IpAddr),
    Name(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]"),
            Target::Ip(ip) => write!(f, "{ip}"),
            Target::Name(n) => f.write_str(n),
        }
    }
}

async fn serve(mut client: TcpStream, mut rx: watch::Receiver<Option<Proxy>>) -> io::Result<()> {
    let (target, port) = tokio::time::timeout(HANDSHAKE, socks5_accept(&mut client))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client handshake"))??;
    let upstream = rx.borrow_and_update().clone();
    let connected = tokio::time::timeout(HANDSHAKE, open(upstream.as_ref(), &target, port)).await;
    let mut remote = match connected {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // 0x05: connection refused; the browser does not fall back to direct.
            client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(e);
        }
        Err(_) => {
            client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect"));
        }
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::select! {
        res = tokio::io::copy_bidirectional(&mut client, &mut remote) => res.map(|_| ()),
        // The route changed: drop the connection so the browser opens a new one.
        _ = rx.changed() => Ok(()),
    }
}

/// SOCKS5 server side, no authentication, CONNECT only.
async fn socks5_accept(s: &mut TcpStream) -> io::Result<(Target, u16)> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(invalid("not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).await?;
        return Err(invalid("client wants authentication"));
    }
    s.write_all(&[5, 0]).await?;

    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[1] != 1 {
        // 0x07: command not supported (BIND, UDP ASSOCIATE).
        s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(invalid("only CONNECT is supported"));
    }
    let target = read_addr(s, req[3]).await?;
    let port = s.read_u16().await?;
    Ok((target, port))
}

async fn read_addr<R: AsyncReadExt + Unpin>(s: &mut R, atyp: u8) -> io::Result<Target> {
    Ok(match atyp {
        1 => {
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await?;
            Target::Ip(IpAddr::from(b))
        }
        4 => {
            let mut b = [0u8; 16];
            s.read_exact(&mut b).await?;
            Target::Ip(IpAddr::from(b))
        }
        3 => {
            let len = s.read_u8().await? as usize;
            let mut b = vec![0u8; len];
            s.read_exact(&mut b).await?;
            Target::Name(String::from_utf8(b).map_err(|_| invalid("bad host name"))?)
        }
        _ => return Err(invalid("bad address type")),
    })
}

/// Connect to `host:port`, IPv4 addresses first, each attempt bounded.
async fn connect_any(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    addrs.sort_by_key(SocketAddr::is_ipv6);
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no addresses"));
    for addr in addrs {
        match tokio::time::timeout(CONNECT_ATTEMPT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e,
            Err(_) => last = io::Error::new(io::ErrorKind::TimedOut, format!("{addr}: timed out")),
        }
    }
    Err(last)
}

async fn open(upstream: Option<&Proxy>, target: &Target, port: u16) -> io::Result<TcpStream> {
    let Some(proxy) = upstream else {
        return match target {
            Target::Ip(ip) => {
                tokio::time::timeout(CONNECT_ATTEMPT * 2, TcpStream::connect((*ip, port)))
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect"))?
            }
            Target::Name(n) => connect_any(n, port).await,
        };
    };
    let up = proxy.upstream();
    let mut s = connect_any(&up.host, up.port).await?;
    match up.kind {
        Upstream::Socks5 => socks5_connect(&mut s, target, port).await?,
        Upstream::Socks4 => socks4_connect(&mut s, target, port).await?,
        Upstream::Http => http_connect(&mut s, target, port).await?,
    }
    Ok(s)
}

async fn socks5_connect(s: &mut TcpStream, target: &Target, port: u16) -> io::Result<()> {
    s.write_all(&[5, 1, 0]).await?;
    let mut r = [0u8; 2];
    s.read_exact(&mut r).await?;
    if r != [5, 0] {
        return Err(invalid("upstream SOCKS5 wants authentication"));
    }
    let mut req = vec![5, 1, 0];
    match target {
        Target::Ip(IpAddr::V4(ip)) => {
            req.push(1);
            req.extend(ip.octets());
        }
        Target::Ip(IpAddr::V6(ip)) => {
            req.push(4);
            req.extend(ip.octets());
        }
        Target::Name(n) => {
            let len = u8::try_from(n.len()).map_err(|_| invalid("host name too long"))?;
            req.push(3);
            req.push(len);
            req.extend(n.as_bytes());
        }
    }
    req.extend(port.to_be_bytes());
    s.write_all(&req).await?;
    let mut rep = [0u8; 4];
    s.read_exact(&mut rep).await?;
    if rep[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("upstream SOCKS5 refused (code {})", rep[1]),
        ));
    }
    read_addr(s, rep[3]).await?;
    s.read_u16().await?;
    Ok(())
}

async fn socks4_connect(s: &mut TcpStream, target: &Target, port: u16) -> io::Result<()> {
    let mut req = vec![4, 1];
    req.extend(port.to_be_bytes());
    match target {
        Target::Ip(IpAddr::V4(ip)) => {
            req.extend(ip.octets());
            req.push(0);
        }
        Target::Ip(IpAddr::V6(_)) => return Err(invalid("SOCKS4 cannot reach IPv6")),
        // SOCKS4a: the proxy resolves the name.
        Target::Name(n) => {
            req.extend([0, 0, 0, 1, 0]);
            req.extend(n.as_bytes());
            req.push(0);
        }
    }
    s.write_all(&req).await?;
    let mut rep = [0u8; 8];
    s.read_exact(&mut rep).await?;
    if rep[1] != 0x5a {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "upstream SOCKS4 refused",
        ));
    }
    Ok(())
}

async fn http_connect(s: &mut TcpStream, target: &Target, port: u16) -> io::Result<()> {
    let authority = format!("{target}:{port}");
    s.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await?;
    // Read the response head byte-wise-buffered, without consuming tunnel data.
    let mut reader = BufReader::new(&mut *s);
    let mut status = String::new();
    reader.read_line(&mut status).await?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    if !reader.buffer().is_empty() {
        return Err(invalid("upstream proxy sent data before the tunnel opened"));
    }
    match status.split_whitespace().nth(1) {
        Some(code) if code.starts_with('2') => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("upstream proxy: {}", status.trim()),
        )),
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot TCP echo server.
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

    async fn via_relay(relay: u16, host: &str, port: u16) -> io::Result<TcpStream> {
        let mut s = TcpStream::connect(("127.0.0.1", relay)).await?;
        socks5_connect(&mut s, &Target::Name(host.into()), port).await?;
        Ok(s)
    }

    #[tokio::test]
    async fn relays_directly_and_through_an_upstream_and_switches() {
        let echo = echo_server().await;
        // An upstream SOCKS5 proxy: another relay, direct.
        let upstream = Relay::start(None).await.unwrap();
        let relay = Relay::start(None).await.unwrap();

        let mut s = via_relay(relay.port(), "localhost", echo).await.unwrap();
        s.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        relay.set(Some(Proxy {
            id: "up".into(),
            name: "up".into(),
            url: format!("socks5://127.0.0.1:{}", upstream.port()),
        }));
        assert_eq!(relay.current().as_deref(), Some("up"));
        // The open connection is dropped on the switch.
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);

        let mut s = via_relay(relay.port(), "127.0.0.1", echo).await.unwrap();
        s.write_all(b"yo").await.unwrap();
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"yo");
    }

    #[tokio::test]
    async fn unreachable_upstream_is_refused_not_bypassed() {
        let echo = echo_server().await;
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let relay = Relay::start(Some(Proxy {
            id: "dead".into(),
            name: "dead".into(),
            url: format!("socks5://127.0.0.1:{dead_port}"),
        }))
        .await
        .unwrap();
        assert!(via_relay(relay.port(), "127.0.0.1", echo).await.is_err());
    }
}
