//! `browser-daemon tabs`: every tab you can reach, from three sources:
//!
//! - `local`: all tabs of the browsers on this machine, from the local tab
//!   API (Unix socket, token; no network);
//! - `shared`: tabs of browsers paired with the daemon (the node extension);
//! - `sessions`: tabs of the daemon's browser sessions, mesh-wide.
//!
//! Ids are `<client>:<tab>`: `gk4-firefox:42` for a browser, and
//! `<session name>:<first 8 of the target id>` for a session tab. A browser's
//! tab has the same id locally and shared, so it is listed once.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use serde::Serialize;
use serde_json::Value;

use super::R;
use crate::ctl::{Conn, ExitError, EXIT_NOT_FOUND, EXIT_UNAUTHORIZED, EXIT_UNREACHABLE};

#[derive(Args, Debug)]
pub struct TabsArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(flatten)]
    local: LocalConn,
    #[command(flatten)]
    sources: Sources,
    #[command(flatten)]
    filter: Filter,
    #[command(subcommand)]
    command: Option<TabsCommand>,
}

/// How to reach the local tab API.
#[derive(Args, Debug, Clone, Default)]
struct LocalConn {
    /// The local API socket (default: $XDG_RUNTIME_DIR/browser-daemon/local.sock).
    #[arg(long, global = true, env = "BROWSER_DAEMON_LOCAL_SOCKET")]
    socket: Option<PathBuf>,
    /// File with the local API token (default: $XDG_CONFIG_HOME/browser-daemon/local-token).
    /// BROWSER_DAEMON_LOCAL_TOKEN, when set, wins.
    #[arg(long, global = true, env = "BROWSER_DAEMON_LOCAL_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

/// Which sources to read; none given means all of them.
#[derive(Args, Debug, Clone, Copy, Default)]
struct Sources {
    /// Only this machine's browsers (local tab API; no network, for scripts).
    #[arg(long, global = true)]
    local: bool,
    /// Only tabs of browsers paired with the daemon.
    #[arg(long, global = true)]
    shared: bool,
    /// Only tabs of the daemon's browser sessions (mesh-wide).
    #[arg(long, global = true)]
    sessions: bool,
}

impl Sources {
    fn all(self) -> bool {
        !(self.local || self.shared || self.sessions)
    }
    fn local(self) -> bool {
        self.all() || self.local
    }
    fn remote(self) -> bool {
        self.all() || self.shared || self.sessions
    }
    fn shared(self) -> bool {
        self.all() || self.shared
    }
    fn sessions(self) -> bool {
        self.all() || self.sessions
    }
}

#[derive(Args, Debug, Clone, Default)]
struct Filter {
    /// Only tabs playing sound.
    #[arg(long, global = true)]
    audible: bool,
    /// Only the active tab of each window.
    #[arg(long, global = true)]
    active: bool,
    /// Only pinned tabs.
    #[arg(long, global = true)]
    pinned: bool,
    /// Only tabs whose URL contains this (case-insensitive), e.g. `meet.jit.si`.
    #[arg(long, global = true)]
    site: Option<String>,
    /// Only tabs whose title contains this (case-insensitive).
    #[arg(long, global = true)]
    title: Option<String>,
    /// Only this client (`gk4-firefox`, a session name) or kind (`firefox`,
    /// `chromium`), or names starting with it.
    #[arg(long, global = true, add = ArgValueCompleter::new(complete_clients))]
    browser: Option<String>,
}

#[derive(Subcommand, Debug)]
enum TabsCommand {
    /// List tabs: id, title and URL, tab-separated (the default).
    #[command(visible_alias = "ls")]
    List {
        /// Title or URL contains this (case-insensitive).
        query: Option<String>,
    },
    /// The active tab of every window.
    Active,
    /// One tab by id (`gk4-firefox:42`).
    #[command(visible_alias = "show")]
    Get {
        #[arg(add = ArgValueCompleter::new(complete_tab_ids))]
        id: String,
    },
}

/// A tab from any source.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    pub id: String,
    /// `local`, `shared` or `session`.
    pub source: &'static str,
    /// `gk4-firefox`, or the session's name.
    pub client: String,
    /// `firefox`, `chromium`, or the session's engine.
    pub browser: String,
    /// The mesh node of a session, or the daemon a browser is paired with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub title: String,
    pub url: String,
    pub active: bool,
    pub audible: bool,
    pub muted: bool,
    pub pinned: bool,
    /// A local tab that is also shared with the daemon.
    pub shared: bool,
    /// The source's own id of the tab (browser tab id, CDP target id).
    pub tab: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<i64>,
}

fn fail<T>(code: i32, msg: impl Into<String>) -> R<T> {
    Err(Box::new(ExitError {
        code,
        msg: msg.into(),
    }))
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Tabs and newlines in a title would break the columns.
fn clean(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

// --- local tab API -----------------------------------------------------------

fn token(local: &LocalConn) -> R<String> {
    if let Ok(t) = std::env::var("BROWSER_DAEMON_LOCAL_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_owned());
        }
    }
    let path = local
        .token_file
        .clone()
        .unwrap_or_else(|| super::config_dir().join("local-token"));
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_owned()),
        Ok(_) => fail(EXIT_UNAUTHORIZED, format!("{} is empty", path.display())),
        Err(e) => fail(
            EXIT_UNAUTHORIZED,
            format!(
                "no local API token: set BROWSER_DAEMON_LOCAL_TOKEN or put one in {} ({e}); \
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

/// `GET path` on the local API with the token; the JSON body of a 2xx answer.
/// Blocking on purpose: one short request, and this starts fast.
fn local_get(local: &LocalConn, path: &str) -> R<Value> {
    let socket = local.socket.clone().unwrap_or_else(super::api_socket);
    let token = token(local)?;
    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            return fail(
                EXIT_UNREACHABLE,
                format!(
                    "the local tab API is not running ({}: {e}); start it with \
                     `browser-daemon local serve` or enable its socket",
                    socket.display()
                ),
            )
        }
    };
    // A shell waiting for completions must not hang on a stuck server.
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
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
        .ok_or("bad response from the local tab API")?;
    let head = std::str::from_utf8(&raw[..split])?;
    let body = &raw[split + 4..];
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad status line from the local tab API")?;
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
    let message = || s(&value, "error").to_owned();
    match status {
        200..=299 => Ok(value),
        401 => fail(
            EXIT_UNAUTHORIZED,
            format!("local tab API: unauthorized: {}", message()),
        ),
        403 => fail(
            EXIT_UNAUTHORIZED,
            format!("local tab API: forbidden: {}", message()),
        ),
        404 => fail(EXIT_NOT_FOUND, message()),
        _ => Err(format!("local tab API answered {status}: {}", message()).into()),
    }
}

fn local_rows(list: &Value) -> Vec<Row> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|t| Row {
            id: s(t, "id").to_owned(),
            source: "local",
            client: s(t, "client").to_owned(),
            browser: s(t, "browser").to_owned(),
            node: None,
            title: s(t, "title").to_owned(),
            url: s(t, "url").to_owned(),
            active: b(t, "active"),
            audible: b(t, "audible"),
            muted: b(t, "muted"),
            pinned: b(t, "pinned"),
            shared: false,
            tab: t.get("tab_id").map(|v| v.to_string()).unwrap_or_default(),
            window: t.get("window_id").and_then(Value::as_i64),
        })
        .collect()
}

/// The local tab API's clients, for `browser-daemon nodes`.
pub(crate) fn local_clients() -> R<Value> {
    let local = LocalConn {
        socket: None,
        token_file: std::env::var_os("BROWSER_DAEMON_LOCAL_TOKEN_FILE").map(PathBuf::from),
    };
    local_get(&local, "/v1/clients")
}

// --- the daemon: shared browsers and sessions ---------------------------------

/// Tabs of browsers paired with the daemon (`/api/extension/nodes`).
fn shared_rows(nodes: &Value, daemon: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for node in nodes["nodes"].as_array().into_iter().flatten() {
        if !b(node, "online") {
            continue;
        }
        let client = s(node, "name").to_owned();
        for t in node["tabs"].as_array().into_iter().flatten() {
            let tab = t.get("id").map(|v| v.to_string()).unwrap_or_default();
            rows.push(Row {
                id: format!("{client}:{tab}"),
                source: "shared",
                client: client.clone(),
                browser: s(node, "browser").to_owned(),
                node: Some(daemon.to_owned()),
                title: s(t, "title").to_owned(),
                url: s(t, "url").to_owned(),
                active: b(t, "active"),
                audible: false,
                muted: false,
                pinned: b(t, "pinned"),
                shared: true,
                tab,
                window: t.get("window_id").and_then(Value::as_i64),
            });
        }
    }
    rows
}

/// Page tabs of the mesh's sessions (`targets` of the mesh overview).
fn session_rows(targets: &Value, sessions: &Value) -> Vec<Row> {
    let name_of = |id: &str| {
        sessions
            .as_array()
            .into_iter()
            .flatten()
            .find(|session| s(session, "id") == id)
            .map(|session| s(session, "name").to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.chars().take(8).collect())
    };
    targets
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| s(t, "type") == "page" || s(t, "target_type") == "page")
        .filter_map(|t| {
            let session = s(t, "session_id");
            if session.is_empty() {
                return None;
            }
            let client = name_of(session);
            let target = s(t, "id");
            let short: String = target.chars().take(8).collect::<String>().to_lowercase();
            Some(Row {
                id: format!("{client}:{short}"),
                source: "session",
                client,
                browser: s(t, "engine").to_owned(),
                node: Some(s(t, "node_name").to_owned()).filter(|n| !n.is_empty()),
                title: s(t, "title").to_owned(),
                url: s(t, "url").to_owned(),
                active: false,
                audible: b(t, "audible"),
                muted: false,
                pinned: false,
                shared: false,
                tab: target.to_owned(),
                window: None,
            })
        })
        .collect()
}

/// A local tab and the same tab shared with the daemon are one row.
fn merge(mut local: Vec<Row>, shared: Vec<Row>, sessions: Vec<Row>) -> Vec<Row> {
    for row in shared {
        match local.iter_mut().find(|l| l.id == row.id) {
            Some(l) => {
                l.shared = true;
                l.node = row.node;
            }
            None => local.push(row),
        }
    }
    local.extend(sessions);
    local
}

impl Filter {
    fn matches(&self, row: &Row, query: Option<&str>) -> bool {
        let has = |hay: &str, needle: &Option<String>| {
            needle
                .as_ref()
                .is_none_or(|n| hay.to_lowercase().contains(&n.to_lowercase()))
        };
        (!self.audible || row.audible)
            && (!self.active || row.active)
            && (!self.pinned || row.pinned)
            && has(&row.url, &self.site)
            && has(&row.title, &self.title)
            && self.browser.as_ref().is_none_or(|b| {
                let b = b.to_lowercase();
                row.client == b || row.browser.to_lowercase() == b || row.client.starts_with(&b)
            })
            && query.is_none_or(|q| {
                let q = q.to_lowercase();
                row.title.to_lowercase().contains(&q) || row.url.to_lowercase().contains(&q)
            })
    }
}

/// Every row of the chosen sources. A source that fails is skipped with a
/// note on stderr when all sources were asked for, and is an error when it
/// was asked for by name.
async fn collect(args: &TabsArgs, quiet: bool) -> R<Vec<Row>> {
    let src = args.sources;
    let mut local = Vec::new();
    if src.local() {
        let local_conn = args.local.clone();
        let result = tokio::task::block_in_place(|| local_get(&local_conn, "/v1/tabs"));
        match result {
            Ok(list) => local = local_rows(&list),
            Err(e) if src.all() => {
                if !quiet {
                    eprintln!("note: skipping this machine's browsers: {e}");
                }
            }
            Err(e) => return Err(e),
        }
    }
    let (mut shared, mut sessions) = (Vec::new(), Vec::new());
    if src.remote() {
        match crate::ctl::remote_tabs(&args.conn).await {
            Ok((targets, mesh_sessions, nodes)) => {
                if src.shared() {
                    shared = shared_rows(&nodes, &args.conn.url);
                }
                if src.sessions() {
                    sessions = session_rows(&targets, &mesh_sessions);
                }
            }
            Err(e) if src.all() => {
                if !quiet {
                    eprintln!("note: skipping the daemon's tabs ({}): {e}", args.conn.url);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(merge(local, shared, sessions))
}

fn print_rows(rows: &[Row]) {
    for r in rows {
        println!("{}\t{}\t{}", r.id, clean(&r.title), r.url);
    }
}

pub async fn tabs(args: TabsArgs) -> R<()> {
    let json = args.conn.json;
    let rows = collect(&args, false).await?;
    let list = TabsCommand::List { query: None };
    match args.command.as_ref().unwrap_or(&list) {
        TabsCommand::List { query } => {
            let rows: Vec<Row> = rows
                .into_iter()
                .filter(|r| args.filter.matches(r, query.as_deref()))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print_rows(&rows);
            }
        }
        TabsCommand::Active => {
            let rows: Vec<Row> = rows
                .into_iter()
                .filter(|r| r.active && args.filter.matches(r, None))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print_rows(&rows);
            }
        }
        TabsCommand::Get { id } => {
            let Some(row) = rows.into_iter().find(|r| r.id == *id) else {
                return fail(EXIT_NOT_FOUND, format!("no tab {id}"));
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else {
                print_rows(&[row]);
            }
        }
    }
    Ok(())
}

// --- shell completion ---------------------------------------------------------

/// The arguments a completer has: none of the command line, so the
/// environment's defaults.
fn completion_args() -> TabsArgs {
    TabsArgs {
        conn: Conn {
            url: std::env::var("BROWSER_DAEMON_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3001".into()),
            secret_file: std::env::var_os("BROWSER_DAEMON_SECRET_FILE").map(PathBuf::from),
            json: false,
        },
        local: LocalConn {
            socket: None,
            token_file: std::env::var_os("BROWSER_DAEMON_LOCAL_TOKEN_FILE").map(PathBuf::from),
        },
        sources: Sources::default(),
        filter: Filter::default(),
        command: None,
    }
}

/// Rows for a completer: quiet, empty when nothing answers. Completion runs
/// before the async runtime does any other work, from `main`.
fn completion_rows() -> Vec<Row> {
    let args = completion_args();
    let run = async { collect(&args, true).await.unwrap_or_default() };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(run)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(run))
            .unwrap_or_default(),
    }
}

fn tab_candidates(rows: &[Row], prefix: &str) -> Vec<CompletionCandidate> {
    rows.iter()
        .filter(|r| r.id.starts_with(prefix))
        .map(|r| CompletionCandidate::new(&r.id).help(Some(clean(&r.title).into())))
        .collect()
}

/// Shell completion for tab ids, every source; titles as hints.
fn complete_tab_ids(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    tab_candidates(&completion_rows(), &current.to_string_lossy())
}

/// Shell completion for `--browser`: client names and browser kinds.
fn complete_clients(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let mut names: Vec<String> = Vec::new();
    for r in completion_rows() {
        for name in [r.client, r.browser] {
            if !name.is_empty() && name.starts_with(&*prefix) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.into_iter().map(CompletionCandidate::new).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, source: &'static str, title: &str) -> Row {
        let (client, tab) = id.split_once(':').unwrap();
        Row {
            id: id.into(),
            source,
            client: client.into(),
            browser: "firefox".into(),
            node: None,
            title: title.into(),
            url: format!("https://{tab}.example/"),
            active: false,
            audible: false,
            muted: false,
            pinned: false,
            shared: false,
            tab: tab.into(),
            window: None,
        }
    }

    #[test]
    fn dechunks() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
        assert!(dechunk(b"5\r\nab").is_err());
    }

    #[test]
    fn a_shared_local_tab_is_listed_once() {
        let local = vec![row("gk4-firefox:5", "local", "Mine")];
        let shared = vec![
            Row {
                node: Some("http://d".into()),
                shared: true,
                ..row("gk4-firefox:5", "shared", "Mine")
            },
            Row {
                shared: true,
                ..row("ge2-firefox:9", "shared", "Theirs")
            },
        ];
        let sessions = vec![row("jolly-otter:abcdef12", "session", "Agent")];
        let rows = merge(local, shared, sessions);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["gk4-firefox:5", "ge2-firefox:9", "jolly-otter:abcdef12"]
        );
        assert!(rows[0].shared);
        assert_eq!(rows[0].source, "local");
        assert_eq!(rows[0].node.as_deref(), Some("http://d"));
    }

    #[test]
    fn reads_shared_tabs_of_online_browsers_and_session_pages() {
        let nodes = json!({"nodes": [
            {"name": "ge2-firefox", "browser": "firefox", "online": true,
             "tabs": [{"id": 9, "title": "T", "url": "https://t/", "active": true}]},
            {"name": "x13-firefox", "browser": "firefox", "online": false, "tabs": []},
        ]});
        let shared = shared_rows(&nodes, "http://d");
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].id, "ge2-firefox:9");
        assert!(shared[0].active);

        let sessions =
            json!([{"id": "11111111-2222-3333-4444-555555555555", "name": "jolly-otter"}]);
        let targets = json!([
            {"id": "ABCDEF1234567890", "type": "page", "title": "P", "url": "https://p/",
             "session_id": "11111111-2222-3333-4444-555555555555", "audible": true, "node_name": "rofl-13"},
            {"id": "SW", "type": "service_worker", "session_id": "11111111-2222-3333-4444-555555555555"},
            {"id": "NOSESSION", "type": "page"},
        ]);
        let rows = session_rows(&targets, &sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "jolly-otter:abcdef12");
        assert_eq!(rows[0].node.as_deref(), Some("rofl-13"));
        assert!(rows[0].audible);
    }

    #[test]
    fn filters_by_client_kind_and_text() {
        let r = row("gk4-firefox:5", "local", "Some Video");
        let f = |browser: &str| Filter {
            browser: Some(browser.into()),
            ..Filter::default()
        };
        assert!(f("gk4-firefox").matches(&r, None));
        assert!(f("firefox").matches(&r, None));
        assert!(f("gk4").matches(&r, None));
        assert!(!f("chromium").matches(&r, None));
        assert!(Filter::default().matches(&r, Some("video")));
        assert!(!Filter::default().matches(&r, Some("nope")));
    }

    #[test]
    fn completes_tab_ids_with_titles() {
        let rows = vec![
            row("gk4-firefox:5", "local", "Tube\tvideo"),
            row("gk4-firefox:6", "local", "Meet"),
            row("ge2-firefox:7", "shared", "Other"),
        ];
        let got = tab_candidates(&rows, "gk4");
        let ids: Vec<String> = got
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        assert_eq!(ids, ["gk4-firefox:5", "gk4-firefox:6"]);
        assert_eq!(
            got[0].get_help().map(ToString::to_string).as_deref(),
            Some("Tube video")
        );
        assert_eq!(tab_candidates(&rows, "").len(), 3);
    }

    #[test]
    fn keeps_columns_intact() {
        assert_eq!(clean("a\tb\nc"), "a b c");
    }
}
