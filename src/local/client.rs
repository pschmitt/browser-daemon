//! `browser-daemon tabs ...`: asks the local server over its Unix socket.
//! Plain blocking HTTP/1.1: a call is one short request, and this starts fast.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use super::R;
use crate::ctl::{ExitError, EXIT_NOT_FOUND, EXIT_UNAUTHORIZED, EXIT_UNREACHABLE};

#[derive(Args, Debug)]
pub struct TabsArgs {
    /// Print JSON instead of tab-separated lines.
    #[arg(long, global = true)]
    json: bool,
    /// The API socket (default: $XDG_RUNTIME_DIR/browser-daemon/local.sock).
    #[arg(long, global = true, env = "BROWSER_DAEMON_LOCAL_SOCKET")]
    socket: Option<PathBuf>,
    /// File with the API token (default: $XDG_CONFIG_HOME/browser-daemon/local-token).
    /// BROWSER_DAEMON_LOCAL_TOKEN, when set, wins.
    #[arg(long, global = true, env = "BROWSER_DAEMON_LOCAL_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: TabsCommand,
}

#[derive(Args, Debug, Default)]
struct Filter {
    /// Only tabs playing sound.
    #[arg(long)]
    audible: bool,
    /// Only the active tab of each window.
    #[arg(long)]
    active: bool,
    /// Only pinned tabs.
    #[arg(long)]
    pinned: bool,
    /// Only tabs whose URL contains this (case-insensitive).
    #[arg(long)]
    url: Option<String>,
    /// Only tabs whose title contains this (case-insensitive).
    #[arg(long)]
    title: Option<String>,
    /// Only tabs of this browser: firefox or chromium.
    #[arg(long)]
    browser: Option<String>,
    /// Title or URL contains this (case-insensitive).
    query: Option<String>,
}

#[derive(Subcommand, Debug)]
enum TabsCommand {
    /// List tabs: id, title and URL, tab-separated.
    #[command(visible_alias = "ls")]
    List(Filter),
    /// The active tab of every window.
    Active,
    /// One tab by id (`a.12.345`).
    Get { id: String },
    /// Windows: id, tab count, whether focused.
    Windows,
    /// Connected browsers and profiles.
    Clients,
}

fn fail<T>(code: i32, msg: impl Into<String>) -> R<T> {
    Err(Box::new(ExitError {
        code,
        msg: msg.into(),
    }))
}

fn token(args: &TabsArgs) -> R<String> {
    if let Ok(t) = std::env::var("BROWSER_DAEMON_LOCAL_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_owned());
        }
    }
    let path = args
        .token_file
        .clone()
        .unwrap_or_else(|| super::config_dir().join("local-token"));
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_owned()),
        Ok(_) => fail(EXIT_UNAUTHORIZED, format!("{} is empty", path.display())),
        Err(e) => fail(
            EXIT_UNAUTHORIZED,
            format!(
                "no token: set BROWSER_DAEMON_LOCAL_TOKEN or put one in {} ({e}); \
                 create one with `browser-daemon local token create --name NAME --scope tabs:read`",
                path.display()
            ),
        ),
    }
}

/// Undo `Transfer-Encoding: chunked`.
fn dechunk(mut body: &[u8]) -> R<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunked response")?;
        let size_text = std::str::from_utf8(&body[..line_end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err("truncated chunked response".into());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// `GET path` with the token; the JSON body of a 2xx answer.
fn get(args: &TabsArgs, path: &str) -> R<Value> {
    let socket = args.socket.clone().unwrap_or_else(super::api_socket);
    let token = token(args)?;
    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            return fail(
                EXIT_UNREACHABLE,
                format!(
                    "the local server is not running ({}: {e}); start it with `browser-daemon local serve` \
                     or enable its socket",
                    socket.display()
                ),
            )
        }
    };
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad response from the local server")?;
    let head = std::str::from_utf8(&raw[..split])?;
    let body = &raw[split + 4..];
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad status line from the local server")?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let message = || {
        value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match status {
        200..=299 => Ok(value),
        401 => fail(EXIT_UNAUTHORIZED, format!("unauthorized: {}", message())),
        403 => fail(EXIT_UNAUTHORIZED, format!("forbidden: {}", message())),
        404 => fail(EXIT_NOT_FOUND, message()),
        _ => Err(format!("local server answered {status}: {}", message()).into()),
    }
}

fn query_string(f: &Filter) -> String {
    let mut parts = Vec::new();
    let enc = |v: &str| utf8_percent_encode(v, NON_ALPHANUMERIC).to_string();
    if f.audible {
        parts.push("audible=true".to_owned());
    }
    if f.active {
        parts.push("active=true".to_owned());
    }
    if f.pinned {
        parts.push("pinned=true".to_owned());
    }
    for (key, value) in [
        ("url", &f.url),
        ("title", &f.title),
        ("browser", &f.browser),
        ("q", &f.query),
    ] {
        if let Some(v) = value {
            parts.push(format!("{key}={}", enc(v)));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Tabs and newlines in a title would break the columns.
fn clean(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

fn print_tabs(list: &Value) {
    for t in list.as_array().into_iter().flatten() {
        println!("{}\t{}\t{}", s(t, "id"), clean(s(t, "title")), s(t, "url"));
    }
}

pub async fn tabs(args: TabsArgs) -> R<()> {
    // Blocking I/O on purpose: tell the runtime this thread is busy.
    tokio::task::block_in_place(|| run(&args))
}

fn run(args: &TabsArgs) -> R<()> {
    let value = match &args.command {
        TabsCommand::List(filter) => get(args, &format!("/v1/tabs{}", query_string(filter)))?,
        TabsCommand::Active => get(args, "/v1/tabs?active=true")?,
        TabsCommand::Get { id } => get(
            args,
            &format!("/v1/tabs/{}", utf8_percent_encode(id, NON_ALPHANUMERIC)),
        )?,
        TabsCommand::Windows => get(args, "/v1/windows")?,
        TabsCommand::Clients => get(args, "/v1/clients")?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    match &args.command {
        TabsCommand::List(_) | TabsCommand::Active => print_tabs(&value),
        TabsCommand::Get { .. } => print_tabs(&Value::Array(vec![value])),
        TabsCommand::Windows => {
            for w in value.as_array().into_iter().flatten() {
                println!(
                    "{}\t{} tabs{}",
                    s(w, "id"),
                    w.get("tabs").and_then(Value::as_u64).unwrap_or(0),
                    if w.get("focused").and_then(Value::as_bool) == Some(true) {
                        "\tfocused"
                    } else {
                        ""
                    }
                );
            }
        }
        TabsCommand::Clients => {
            for c in value.as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{} {}\t{} tabs",
                    s(c, "prefix"),
                    s(c, "browser"),
                    s(c, "name"),
                    s(c, "version"),
                    c.get("tabs").and_then(Value::as_u64).unwrap_or(0)
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunks() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
        assert!(dechunk(b"5\r\nab").is_err());
    }

    #[test]
    fn builds_queries() {
        let f = Filter {
            audible: true,
            url: Some("zoom.us/wc".into()),
            ..Filter::default()
        };
        assert_eq!(query_string(&f), "?audible=true&url=zoom%2Eus%2Fwc");
        assert_eq!(query_string(&Filter::default()), "");
    }

    #[test]
    fn keeps_columns_intact() {
        assert_eq!(clean("a\tb\nc"), "a b c");
    }
}
