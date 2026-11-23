//! The native messaging host (`browser-daemon-native-host`): the browser
//! starts it when the node extension connects, one per browser profile. It
//! relays the extension's messages to the local server's feeder socket, one
//! JSON object per line, and the server's lines back to the extension.
//!
//! Native messaging frames are a 32-bit length in native byte order followed
//! by that much JSON, on stdin and stdout; stdout carries nothing else.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::R;

/// Browsers send at most 4 GiB; anything near that is not a tab list.
const MAX_FRAME: usize = 64 << 20;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

async fn read_frame<Rd: AsyncReadExt + Unpin>(input: &mut Rd) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message of {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

async fn write_frame<W: AsyncWriteExt + Unpin>(output: &mut W, json: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(json.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "message too long"))?;
    output.write_all(&len.to_ne_bytes()).await?;
    output.write_all(json).await?;
    output.flush().await
}

enum Event {
    Browser(Option<Value>),
    Server(std::io::Result<Option<String>>),
    Retry,
}

struct Server {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

/// Start `browser-daemon local serve` in the background when nothing listens
/// (no systemd socket activation): once per host.
fn spawn_server() {
    if std::env::var_os("BROWSER_DAEMON_LOCAL_NO_SPAWN").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let result = std::process::Command::new(exe)
        .args(["local", "serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match result {
        Ok(_) => eprintln!("browser-daemon-native-host: started the local server"),
        Err(e) => eprintln!("browser-daemon-native-host: could not start the local server: {e}"),
    }
}

pub async fn run() -> R<()> {
    let (tx, mut from_browser) = mpsc::channel::<Value>(64);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        loop {
            match read_frame(&mut stdin).await {
                Ok(Some(buf)) => match serde_json::from_slice::<Value>(&buf) {
                    Ok(v) => {
                        if tx.send(v).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("browser-daemon-native-host: bad message: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    eprintln!("browser-daemon-native-host: {e}");
                    break;
                }
            }
        }
    });

    let mut stdout = tokio::io::stdout();
    let feeder = super::feeder_socket();
    // Replayed after every (re)connect, so the server always has the state.
    let mut hello: Option<String> = None;
    let mut snapshot: Option<String> = None;
    let mut server: Option<Server> = None;
    let mut backoff = Duration::from_secs(1);
    let mut next_attempt = Instant::now();
    let mut spawned = false;

    loop {
        if server.is_none() && Instant::now() >= next_attempt {
            match UnixStream::connect(&feeder).await {
                Ok(stream) => {
                    let (read, mut writer) = stream.into_split();
                    let mut ok = true;
                    for line in [&hello, &snapshot].into_iter().flatten() {
                        ok &= writer
                            .write_all(format!("{line}\n").as_bytes())
                            .await
                            .is_ok();
                    }
                    if ok {
                        server = Some(Server {
                            lines: BufReader::new(read).lines(),
                            writer,
                        });
                        backoff = Duration::from_secs(1);
                    }
                }
                Err(e) => {
                    if !spawned
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        )
                    {
                        spawned = true;
                        spawn_server();
                        backoff = Duration::from_millis(300);
                    }
                }
            }
            if server.is_none() {
                next_attempt = Instant::now() + backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }

        let waiting = server.is_none();
        let event = {
            let from_server = async {
                match server.as_mut() {
                    Some(s) => s.lines.next_line().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                msg = from_browser.recv() => Event::Browser(msg),
                line = from_server => Event::Server(line),
                () = tokio::time::sleep_until(next_attempt), if waiting => Event::Retry,
            }
        };
        match event {
            Event::Browser(None) => break,
            Event::Browser(Some(msg)) => {
                let line = serde_json::to_string(&msg)?;
                match msg.get("type").and_then(Value::as_str) {
                    Some("hello") => hello = Some(line.clone()),
                    Some("snapshot") => snapshot = Some(line.clone()),
                    _ => {}
                }
                if let Some(s) = server.as_mut() {
                    if s.writer
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        server = None;
                        next_attempt = Instant::now();
                    }
                }
            }
            Event::Server(Ok(Some(line))) => write_frame(&mut stdout, line.as_bytes()).await?,
            Event::Server(_) => {
                server = None;
                next_attempt = Instant::now() + backoff;
            }
            Event::Retry => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"type":"hello"}"#).await.unwrap();
        assert_eq!(&buf[..4], &16u32.to_ne_bytes());
        let mut input = &buf[..];
        let frame = read_frame(&mut input).await.unwrap().unwrap();
        assert_eq!(frame, br#"{"type":"hello"}"#);
        assert!(read_frame(&mut input).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refuses_huge_frames() {
        let len = (MAX_FRAME as u32 + 1).to_ne_bytes();
        let mut input = &len[..];
        assert!(read_frame(&mut input).await.is_err());
    }
}
