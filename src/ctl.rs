//! The command-line client: `browser-daemon session list`, `profile backup create`, ...
//! It talks to the HTTP API of one node (peers are reached through it) and covers
//! what the dashboard does.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

use crate::{MeshOverview, MeshProfile, MeshRecording, MeshSession};

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// Which daemon to talk to, and how.
#[derive(Args, Debug, Clone)]
pub struct Conn {
    /// Daemon to talk to; its peers are reachable through it.
    #[arg(
        long,
        global = true,
        default_value = "http://127.0.0.1:3001",
        env = "BROWSER_DAEMON_URL"
    )]
    pub url: String,

    /// File holding the cluster secret (default: the NixOS module's, when readable;
    /// otherwise the one the dashboard hands to its viewers).
    #[arg(long, global = true, env = "BROWSER_DAEMON_SECRET_FILE")]
    pub secret_file: Option<PathBuf>,

    /// Print JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,
}

// ------------------------------------------------------------------ commands

#[derive(Args, Debug)]
pub struct SessionArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: SessionCmd,
}

#[derive(Subcommand, Debug)]
pub enum SessionCmd {
    /// List the sessions of the mesh.
    #[command(visible_alias = "ls")]
    List {
        /// Only this node.
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one session.
    Show {
        /// Id (or its start), or name.
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Start a session; prints its id and WebSocket URL.
    New(NewSession),
    /// Stop sessions.
    #[command(visible_aliases = ["rm", "stop"])]
    Delete {
        #[arg(required = true)]
        sessions: Vec<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Stop every session (of one node).
    StopAll {
        #[arg(long)]
        node: Option<String>,
        /// Do not ask.
        #[arg(short, long)]
        yes: bool,
    },
    /// Rename a session (empty: back to the generated name).
    Rename {
        session: String,
        name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Switch a session's proxy now (`none`: direct).
    Proxy {
        session: String,
        proxy: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Minutes of inactivity before a session stops (0: never).
    IdleTimeout {
        session: String,
        minutes: u64,
        #[arg(long)]
        node: Option<String>,
    },
    /// Start, stop, pause or resume a session's recording.
    Record {
        session: String,
        action: RecordAction,
        #[arg(long)]
        node: Option<String>,
    },
    /// List a session's tabs.
    Tabs {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Open a URL in a new tab.
    Open {
        session: String,
        /// Page to open.
        #[arg(value_name = "URL")]
        page: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Close a tab (by target id, see `session tabs`).
    CloseTab {
        session: String,
        tab: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Keep a throwaway session's data (logins, cookies, storage) as a new profile.
    /// The browser restarts on the same pages as a session of the profile; prints its id.
    #[command(visible_alias = "save")]
    SaveAsProfile {
        session: String,
        /// Name of the new profile.
        name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Print a session's WebSocket URL (for Playwright or Puppeteer).
    Url {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum RecordAction {
    Start,
    Stop,
    Pause,
    Resume,
}

#[derive(Args, Debug)]
pub struct NewSession {
    /// Node to start it on (default: the one asked).
    #[arg(long)]
    pub node: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_enum)]
    pub engine: Option<Engine>,
    /// Profile to use (name); its cookies and logins persist.
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    /// Record the session (default: the node's setting).
    #[arg(long, overrides_with = "no_record")]
    pub record: bool,
    #[arg(long)]
    pub no_record: bool,
    #[arg(long, overrides_with = "no_adblock")]
    pub adblock: bool,
    #[arg(long)]
    pub no_adblock: bool,
    /// Proxy id, or `none` for direct.
    #[arg(long)]
    pub proxy: Option<String>,
    #[arg(long, value_enum)]
    pub color_scheme: Option<Scheme>,
    /// Minutes of inactivity before it stops (0: never).
    #[arg(long)]
    pub idle_timeout: Option<u64>,
    /// Memory limit of the browser in MB.
    #[arg(long)]
    pub max_memory_mb: Option<u64>,
    #[arg(long, value_enum)]
    pub screen: Option<Screen>,
    /// Stop the session when its last client disconnects.
    #[arg(long)]
    pub ephemeral: bool,
    /// Do not open the profile's default tabs (a blank tab instead).
    #[arg(long)]
    pub no_default_tabs: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Engine {
    Chromium,
    Firefox,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Scheme {
    Dark,
    Light,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Screen {
    Window,
    Native,
    Phone,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: ProfileCmd,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// List the profiles of the mesh.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one profile.
    Show {
        profile: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Create an empty profile (or a copy of one).
    New {
        name: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long, value_enum)]
        engine: Option<Engine>,
        /// Copy this profile (on the same node).
        #[arg(long)]
        clone_from: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Page its sessions open with (repeat for several tabs).
        #[arg(long = "tab")]
        tabs: Vec<String>,
    },
    /// Change a profile's name or defaults.
    Set {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Proxy id for new sessions, or `none`.
        #[arg(long)]
        proxy: Option<String>,
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Default tabs: pages its sessions open with (repeat for several; replaces the list).
        #[arg(long = "tab")]
        tabs: Vec<String>,
        /// Remove all default tabs.
        #[arg(long)]
        clear_tabs: bool,
    },
    /// Copy a profile under a new name (same node).
    Clone {
        profile: String,
        new_name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Delete a profile.
    #[command(visible_alias = "rm")]
    Delete {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// Stop its session first.
        #[arg(long)]
        force: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Wipe a profile's cookies and logins (a safety backup is kept).
    Clear {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// Stop its session first.
        #[arg(long)]
        force: bool,
        /// Skip the safety backup.
        #[arg(long)]
        no_backup: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Server-side backups of a profile.
    #[command(subcommand)]
    Backup(BackupCmd),
    /// Download a profile as a .tar.gz archive.
    Export {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// File to write (default: <profile>.tar.gz).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Create a profile from an archive made by `export`.
    Import {
        name: String,
        file: PathBuf,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Copy or move a profile to another node.
    Transfer {
        profile: String,
        /// Target node.
        #[arg(long)]
        to: String,
        #[arg(long)]
        node: Option<String>,
        /// Name on the target (default: the same).
        #[arg(long)]
        name: Option<String>,
        /// Delete the source afterwards.
        #[arg(long = "move")]
        move_: bool,
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    /// List a profile's backups.
    #[command(visible_alias = "ls")]
    List {
        profile: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Back a profile up (it must not be in use).
    Create {
        profile: String,
        label: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Replace the profile's data with a backup (a safety backup of the current data is kept).
    Restore {
        profile: String,
        backup: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a backup.
    #[command(visible_alias = "rm")]
    Delete {
        profile: String,
        backup: String,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct RecordingArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: RecordingCmd,
}

#[derive(Subcommand, Debug)]
pub enum RecordingCmd {
    /// List the recordings of the mesh.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one recording's details.
    Show { recording: String },
    /// Delete recordings.
    #[command(visible_alias = "rm")]
    Delete {
        #[arg(required = true)]
        recordings: Vec<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Download a recording's frames (JSON lines).
    Export {
        recording: String,
        /// File to write (default: <id>.jsonl).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Apply the retention limits now.
    Vacuum {
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct NodeArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: NodeCmd,
}

#[derive(Subcommand, Debug)]
pub enum NodeCmd {
    /// List the nodes of the mesh.
    #[command(visible_alias = "ls")]
    List,
    /// A node's settings, version and proxies.
    Show { node: Option<String> },
    /// Change a node's settings (proxy default, per-engine defaults).
    Set {
        /// Node (default: the one asked).
        node: Option<String>,
        /// Proxy id sessions use unless they choose another (`none`: direct).
        #[arg(long)]
        default_proxy: Option<String>,
        /// Engine the options below apply to.
        #[arg(long, value_enum)]
        engine: Option<Engine>,
        /// Block ads in new sessions of the engine.
        #[arg(long)]
        adblock: Option<bool>,
        /// Appearance of new sessions of the engine.
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Live views mark taps and clicks for the engine.
        #[arg(long)]
        show_touches: Option<bool>,
    },
    /// Add a peer.
    Add {
        /// The peer's base URL.
        #[arg(value_name = "URL")]
        peer: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a peer.
    #[command(visible_alias = "rm")]
    Remove { node: String },
}

// -------------------------------------------------------------------- client

struct Client {
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
    json: bool,
    ov: MeshOverview,
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

impl Client {
    async fn connect(conn: &Conn) -> R<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let base = conn.url.trim_end_matches('/').to_string();
        let secret = crate::tui::find_secret(&http, &base, conn.secret_file.as_ref()).await;
        let mut req = http
            .get(format!("{base}/api/mesh/overview"))
            .timeout(Duration::from_secs(30));
        if let Some(ref s) = secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req.send().await.map_err(|e| format!("{base}: {e}"))?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized: pass --secret-file (or run where the dashboard is reachable)".into());
        }
        if !res.status().is_success() {
            return Err(format!("{base}: HTTP {}", res.status()).into());
        }
        let ov = res.json().await?;
        Ok(Self { http, base, secret, json: conn.json, ov })
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .timeout(Duration::from_secs(60));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        req
    }

    /// Send, fail with the API's own message, return the JSON answer (null when empty).
    async fn call(&self, req: reqwest::RequestBuilder) -> R<Value> {
        let res = req.send().await.map_err(|e| format!("{}: {e}", self.base))?;
        let status = res.status();
        let text = res.text().await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized: pass --secret-file (or run where the dashboard is reachable)".into());
        }
        if !status.is_success() {
            let msg = value["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| if text.is_empty() { status.to_string() } else { text });
            return Err(msg.into());
        }
        Ok(value)
    }

    /// API path of `path` on `node`: local, or through the daemon's peer proxy.
    fn api(&self, node: &str, path: &str) -> String {
        if node == self.ov.local_node.name {
            format!("/api{path}")
        } else {
            format!("/api/peers/{}{path}", enc(node))
        }
    }

    /// DevTools HTTP path (`/json/...`) on `node`.
    fn devtools(&self, node: &str, path: &str) -> String {
        if node == self.ov.local_node.name {
            path.to_string()
        } else {
            format!("/api/peers/{}{path}", enc(node))
        }
    }

    fn check_node(&self, node: &str) -> R<()> {
        let known = node == self.ov.local_node.name || self.ov.peers.iter().any(|p| p.name == node);
        if known {
            Ok(())
        } else {
            Err(format!("Unknown node '{node}'").into())
        }
    }

    /// The node to act on: the one named, else the one asked.
    fn node_or_local(&self, node: Option<&str>) -> R<String> {
        match node {
            Some(n) => {
                self.check_node(n)?;
                Ok(n.to_string())
            }
            None => Ok(self.ov.local_node.name.clone()),
        }
    }

    fn session(&self, key: &str, node: Option<&str>) -> R<&MeshSession> {
        let in_node = |s: &&MeshSession| node.is_none_or(|n| s.node_name == n);
        let hits: Vec<&MeshSession> = self
            .ov
            .sessions
            .iter()
            .filter(in_node)
            .filter(|s| {
                let id = s.session.id.to_string();
                id == key || s.session.name == key || (key.len() >= 4 && id.starts_with(key))
            })
            .collect();
        one(hits, "session", key, |s| {
            format!("{} {} on {}", s.session.id, s.session.name, s.node_name)
        })
    }

    fn profile(&self, key: &str, node: Option<&str>) -> R<&MeshProfile> {
        let hits: Vec<&MeshProfile> = self
            .ov
            .profiles
            .iter()
            .filter(|p| node.is_none_or(|n| p.node_name == n))
            .filter(|p| {
                p.profile.name == key
                    || p.profile.id.is_some_and(|id| {
                        let id = id.to_string();
                        id == key || (key.len() >= 4 && id.starts_with(key))
                    })
            })
            .collect();
        one(hits, "profile", key, |p| {
            format!("{} on {}", p.profile.name, p.node_name)
        })
    }

    fn recording(&self, key: &str) -> R<&MeshRecording> {
        let hits: Vec<&MeshRecording> = self
            .ov
            .recordings
            .iter()
            .filter(|r| {
                let id = r.recording.id.to_string();
                id == key || (key.len() >= 4 && id.starts_with(key))
            })
            .collect();
        one(hits, "recording", key, |r| {
            format!("{} on {}", r.recording.id, r.node_name)
        })
    }

    fn print_json(&self, v: &Value) {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    }
}

/// Exactly one match, or an error that says what was ambiguous.
fn one<'a, T>(hits: Vec<&'a T>, what: &str, key: &str, describe: impl Fn(&T) -> String) -> R<&'a T> {
    match hits.as_slice() {
        [] => Err(format!("No {what} '{key}'").into()),
        [x] => Ok(x),
        many => Err(format!(
            "'{key}' matches several {what}s (use --node, or more of the id):\n  {}",
            many.iter().map(|x| describe(x)).collect::<Vec<_>>().join("\n  ")
        )
        .into()),
    }
}

// -------------------------------------------------------------------- output

fn table(head: &[&str], rows: &[Vec<String>]) {
    let mut width: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, c) in row.iter().enumerate() {
            width[i] = width[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(c);
            } else {
                out.push_str(&format!("{:<w$}  ", c, w = width[i]));
            }
        }
        println!("{}", out.trim_end());
    };
    line(head.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..=86399 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d{}h", secs / 86400, secs % 86400 / 3600),
    }
}

fn ago(epoch: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{} ago", duration(now.saturating_sub(epoch)))
}

fn short(id: impl ToString) -> String {
    id.to_string().chars().take(8).collect()
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "-" }.to_string()
}

fn confirm(question: &str, yes: bool) -> R<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!("{question} Pass --yes to confirm.").into());
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err("Cancelled".into())
    }
}

fn ws_url(s: &MeshSession) -> String {
    let engine = if s.session.engine == crate::BrowserEngine::Firefox {
        "firefox"
    } else {
        "chromium"
    };
    format!(
        "{}/{engine}?session_id={}",
        s.node_url.trim_end_matches('/').replacen("http", "ws", 1),
        s.session.id
    )
}

// ------------------------------------------------------------------ sessions

pub async fn session(args: SessionArgs) -> R<()> {
    let c = Client::connect(&args.conn).await?;
    match args.cmd {
        SessionCmd::List { node } => {
            if let Some(ref n) = node {
                c.check_node(n)?;
            }
            let sessions: Vec<&MeshSession> = c
                .ov
                .sessions
                .iter()
                .filter(|s| node.as_deref().is_none_or(|n| s.node_name == n))
                .collect();
            if c.json {
                c.print_json(&serde_json::to_value(&sessions)?);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .map(|s| {
                    let x = &s.session;
                    let rec = match (x.is_recording, x.recording_paused) {
                        (true, true) => "paused",
                        (true, false) => "yes",
                        _ => "-",
                    };
                    vec![
                        short(x.id),
                        x.name.clone(),
                        s.node_name.clone(),
                        format!("{:?}", x.engine).to_lowercase(),
                        x.profile.clone().unwrap_or_else(|| "-".into()),
                        duration(x.uptime_seconds),
                        x.tabs_count.to_string(),
                        rec.to_string(),
                        x.proxy.clone().unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            table(
                &["ID", "NAME", "NODE", "ENGINE", "PROFILE", "UP", "TABS", "REC", "PROXY"],
                &rows,
            );
        }
        SessionCmd::Show { session, node } => {
            let s = c.session(&session, node.as_deref())?;
            let mut v = serde_json::to_value(s)?;
            v["websocket_url"] = json!(ws_url(s));
            c.print_json(&v);
        }
        SessionCmd::New(a) => {
            let node = c.node_or_local(a.node.as_deref())?;
            let mut body = json!({ "persistent": !a.ephemeral });
            if a.no_default_tabs {
                body["default_tabs"] = json!(false);
            }
            let mut set = |k: &str, v: Value| body[k] = v;
            if let Some(v) = a.name { set("name", json!(v)) }
            if let Some(v) = a.engine { set("engine", json!(format!("{v:?}").to_lowercase())) }
            if let Some(v) = a.profile { set("profile", json!(v)) }
            if let Some(v) = a.width { set("width", json!(v)) }
            if let Some(v) = a.height { set("height", json!(v)) }
            if a.record { set("record", json!(true)) }
            if a.no_record { set("record", json!(false)) }
            if a.adblock { set("adblock", json!(true)) }
            if a.no_adblock { set("adblock", json!(false)) }
            if let Some(v) = a.proxy { set("proxy", json!(v)) }
            if let Some(v) = a.color_scheme { set("color_scheme", json!(format!("{v:?}").to_lowercase())) }
            if let Some(v) = a.idle_timeout { set("idle_timeout_minutes", json!(v)) }
            if let Some(v) = a.max_memory_mb { set("max_memory_mb", json!(v)) }
            if let Some(v) = a.screen { set("screen", json!(format!("{v:?}").to_lowercase())) }
            let created = c
                .call(c.req(Method::POST, &c.api(&node, "/sessions")).json(&body))
                .await?;
            if c.json {
                c.print_json(&created);
            } else {
                let id = created["id"].as_str().unwrap_or("?");
                let engine = created["engine"].as_str().unwrap_or("chromium");
                let base = if node == c.ov.local_node.name {
                    c.ov.local_node.url.clone()
                } else {
                    c.ov.peers.iter().find(|p| p.name == node).map(|p| p.url.clone()).unwrap_or_default()
                };
                println!("{id}");
                eprintln!(
                    "{} on {node}: {}/{engine}?session_id={id}",
                    created["name"].as_str().unwrap_or(""),
                    base.trim_end_matches('/').replacen("http", "ws", 1)
                );
            }
        }
        SessionCmd::Delete { sessions, node } => {
            let mut failed = 0;
            for key in &sessions {
                let s = c.session(key, node.as_deref())?;
                let path = c.api(&s.node_name, &format!("/sessions/{}", s.session.id));
                match c.call(c.req(Method::DELETE, &path)).await {
                    Ok(_) => println!("stopped {} ({})", s.session.name, s.node_name),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {e}", s.session.name);
                    }
                }
            }
            if failed > 0 {
                return Err(format!("{failed} session(s) could not be stopped").into());
            }
        }
        SessionCmd::StopAll { node, yes } => {
            if let Some(ref n) = node {
                c.check_node(n)?;
            }
            let sessions: Vec<&MeshSession> = c
                .ov
                .sessions
                .iter()
                .filter(|s| node.as_deref().is_none_or(|n| s.node_name == n))
                .collect();
            if sessions.is_empty() {
                println!("No sessions");
                return Ok(());
            }
            confirm(
                &format!(
                    "Stop {} session{}{}?",
                    sessions.len(),
                    if sessions.len() == 1 { "" } else { "s" },
                    node.as_deref().map(|n| format!(" on {n}")).unwrap_or_default()
                ),
                yes,
            )?;
            let client = &c;
            let results = futures_util::future::join_all(sessions.iter().map(|s| {
                let path = c.api(&s.node_name, &format!("/sessions/{}", s.session.id));
                async move { client.call(client.req(Method::DELETE, &path)).await }
            }))
            .await;
            let failed = results.iter().filter(|r| r.is_err()).count();
            println!("Stopped {} of {}", sessions.len() - failed, sessions.len());
            if failed > 0 {
                return Err(format!("{failed} session(s) could not be stopped").into());
            }
        }
        SessionCmd::Rename { session, name, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(&s.node_name, &format!("/sessions/{}/name", s.session.id));
            let v = c.call(c.req(Method::POST, &path).json(&json!({ "name": name }))).await?;
            done(&c, v, "renamed");
        }
        SessionCmd::Proxy { session, proxy, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(&s.node_name, &format!("/sessions/{}/proxy", s.session.id));
            let v = c.call(c.req(Method::POST, &path).json(&json!({ "proxy": proxy }))).await?;
            done(&c, v, "proxy switched");
        }
        SessionCmd::IdleTimeout { session, minutes, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(
                &s.node_name,
                &format!("/sessions/{}/idle-timeout?minutes={minutes}", s.session.id),
            );
            let v = c.call(c.req(Method::POST, &path)).await?;
            done(&c, v, "idle timeout set");
        }
        SessionCmd::Record { session, action, node } => {
            let s = c.session(&session, node.as_deref())?;
            let verb = match action {
                RecordAction::Start => "record",
                RecordAction::Stop => "stop-record",
                RecordAction::Pause => "pause-record",
                RecordAction::Resume => "resume-record",
            };
            let path = c.api(&s.node_name, &format!("/sessions/{}/{verb}", s.session.id));
            let v = c.call(c.req(Method::POST, &path)).await?;
            done(&c, v, verb);
        }
        SessionCmd::Tabs { session, node } => {
            let s = c.session(&session, node.as_deref())?;
            let tabs: Vec<_> = c
                .ov
                .targets
                .iter()
                .filter(|t| t.target.session_id == Some(s.session.id) && t.target.target_type == "page")
                .collect();
            if c.json {
                c.print_json(&serde_json::to_value(&tabs)?);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = tabs
                .iter()
                .map(|t| {
                    vec![
                        t.target.id.clone(),
                        yes_no(t.target.audible),
                        t.target.title.clone().unwrap_or_default(),
                        t.target.url.clone(),
                    ]
                })
                .collect();
            table(&["TAB", "SOUND", "TITLE", "URL"], &rows);
        }
        SessionCmd::Open { session, page, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = format!(
                "{}?url={}&session_id={}",
                c.devtools(&s.node_name, "/json/new"),
                enc(&page),
                s.session.id
            );
            let v = c.call(c.req(Method::PUT, &path)).await?;
            if c.json {
                c.print_json(&v);
            } else {
                println!("{}", v["id"].as_str().unwrap_or("opened"));
            }
        }
        SessionCmd::CloseTab { session, tab, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.devtools(&s.node_name, &format!("/json/close/{}", enc(&tab)));
            c.call(c.req(Method::PUT, &path)).await?;
            println!("closed");
        }
        SessionCmd::SaveAsProfile { session, name, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(&s.node_name, &format!("/sessions/{}/save-as-profile", s.session.id));
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .timeout(Duration::from_secs(300))
                        .json(&json!({ "name": name })),
                )
                .await?;
            if c.json {
                c.print_json(&v);
            } else {
                println!("{}", v["session"]["id"].as_str().unwrap_or("saved"));
                eprintln!("saved as profile {name}");
            }
        }
        SessionCmd::Url { session, node } => {
            println!("{}", ws_url(c.session(&session, node.as_deref())?));
        }
    }
    Ok(())
}

/// Print the answer as JSON, or a short confirmation.
fn done(c: &Client, v: Value, what: &str) {
    if c.json {
        c.print_json(&v);
    } else {
        println!("{what}");
    }
}

// ------------------------------------------------------------------ profiles

pub async fn profile(args: ProfileArgs) -> R<()> {
    let c = Client::connect(&args.conn).await?;
    match args.cmd {
        ProfileCmd::List { node } => {
            if let Some(ref n) = node {
                c.check_node(n)?;
            }
            let profiles: Vec<&MeshProfile> = c
                .ov
                .profiles
                .iter()
                .filter(|p| node.as_deref().is_none_or(|n| p.node_name == n))
                .collect();
            if c.json {
                c.print_json(&serde_json::to_value(&profiles)?);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = profiles
                .iter()
                .map(|p| {
                    let x = &p.profile;
                    vec![
                        x.name.clone(),
                        p.node_name.clone(),
                        format!("{:?}", x.engine).to_lowercase(),
                        bytes(x.size_bytes),
                        yes_no(x.is_active),
                        x.proxy.clone().unwrap_or_else(|| "-".into()),
                        format!("{:?}", x.color_scheme).to_lowercase(),
                        if x.tabs.is_empty() {
                            "-".to_string()
                        } else {
                            x.tabs.join(" ")
                        },
                    ]
                })
                .collect();
            table(
                &["NAME", "NODE", "ENGINE", "SIZE", "ACTIVE", "PROXY", "SCHEME", "TABS"],
                &rows,
            );
        }
        ProfileCmd::Show { profile, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}", enc(&p.profile.name)));
            c.print_json(&c.call(c.req(Method::GET, &path)).await?);
        }
        ProfileCmd::New { name, node, engine, clone_from, proxy, color_scheme, tabs } => {
            let node = c.node_or_local(node.as_deref())?;
            let mut body = json!({ "name": name });
            if let Some(v) = engine { body["engine"] = json!(format!("{v:?}").to_lowercase()) }
            if let Some(v) = clone_from { body["clone_from"] = json!(v) }
            if let Some(v) = proxy { body["proxy"] = json!(v) }
            if let Some(v) = color_scheme { body["color_scheme"] = json!(format!("{v:?}").to_lowercase()) }
            if !tabs.is_empty() { body["tabs"] = json!(tabs) }
            let v = c.call(c.req(Method::POST, &c.api(&node, "/profiles")).json(&body)).await?;
            done(&c, v, &format!("created {name} on {node}"));
        }
        ProfileCmd::Set { profile, node, name, proxy, color_scheme, tabs, clear_tabs } => {
            let p = c.profile(&profile, node.as_deref())?;
            let mut body = json!({});
            if let Some(v) = name { body["name"] = json!(v) }
            if let Some(v) = proxy { body["proxy"] = json!(v) }
            if let Some(v) = color_scheme { body["color_scheme"] = json!(format!("{v:?}").to_lowercase()) }
            if clear_tabs { body["tabs"] = json!([]) } else if !tabs.is_empty() { body["tabs"] = json!(tabs) }
            let path = c.api(&p.node_name, &format!("/profiles/{}", enc(&p.profile.name)));
            let v = c.call(c.req(Method::PATCH, &path).json(&body)).await?;
            done(&c, v, "updated");
        }
        ProfileCmd::Clone { profile, new_name, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}/clone", enc(&p.profile.name)));
            let v = c.call(c.req(Method::POST, &path).json(&json!({ "target_name": new_name }))).await?;
            done(&c, v, "cloned");
        }
        ProfileCmd::Delete { profile, node, force, yes } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!("Delete profile {} on {}? Cookies and logins are wiped.", p.profile.name, p.node_name),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}?force={force}", enc(&p.profile.name)),
            );
            let v = c.call(c.req(Method::DELETE, &path)).await?;
            done(&c, v, "deleted");
        }
        ProfileCmd::Clear { profile, node, force, no_backup, yes } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!("Wipe the data of profile {} on {}?", p.profile.name, p.node_name),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!(
                    "/profiles/{}/clear?force={force}&backup={}",
                    enc(&p.profile.name),
                    !no_backup
                ),
            );
            let v = c.call(c.req(Method::POST, &path)).await?;
            done(&c, v, "cleared");
        }
        ProfileCmd::Backup(cmd) => backup(&c, cmd).await?,
        ProfileCmd::Export { profile, node, output } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}/export", enc(&p.profile.name)));
            let res = c
                .req(Method::GET, &path)
                .timeout(Duration::from_secs(3600))
                .send()
                .await?;
            if !res.status().is_success() {
                return Err(format!("export failed: HTTP {}", res.status()).into());
            }
            let file = output.unwrap_or_else(|| PathBuf::from(format!("{}.tar.gz", p.profile.name)));
            let data = res.bytes().await?;
            std::fs::write(&file, &data)?;
            println!("{} ({})", file.display(), bytes(data.len() as u64));
        }
        ProfileCmd::Import { name, file, node, overwrite } => {
            let node = c.node_or_local(node.as_deref())?;
            let data = std::fs::read(&file)?;
            let path = c.api(&node, &format!("/profiles/{}/import?overwrite={overwrite}", enc(&name)));
            let v = c
                .call(c.req(Method::POST, &path).timeout(Duration::from_secs(3600)).body(data))
                .await?;
            done(&c, v, &format!("imported {name} on {node}"));
        }
        ProfileCmd::Transfer { profile, to, node, name, move_, overwrite } => {
            c.check_node(&to)?;
            let p = c.profile(&profile, node.as_deref())?;
            let mut body = json!({ "target_node": to, "delete_source": move_, "overwrite": overwrite });
            if let Some(n) = name {
                body["target_name"] = json!(n);
            }
            let path = c.api(&p.node_name, &format!("/profiles/{}/transfer", enc(&p.profile.name)));
            let v = c
                .call(c.req(Method::POST, &path).timeout(Duration::from_secs(3600)).json(&body))
                .await?;
            done(&c, v, "transferred");
        }
    }
    Ok(())
}

async fn backup(c: &Client, cmd: BackupCmd) -> R<()> {
    match cmd {
        BackupCmd::List { profile, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}/backups", enc(&p.profile.name)));
            let v = c.call(c.req(Method::GET, &path)).await?;
            if c.json {
                c.print_json(&v);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = v
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    vec![
                        b["id"].as_str().unwrap_or("").to_string(),
                        ago(b["created_at_secs"].as_u64().unwrap_or(0)),
                        bytes(b["size_bytes"].as_u64().unwrap_or(0)),
                        if b["automatic"].as_bool().unwrap_or(false) { "auto" } else { "manual" }.to_string(),
                        b["label"].as_str().unwrap_or("").to_string(),
                    ]
                })
                .collect();
            table(&["ID", "CREATED", "SIZE", "KIND", "LABEL"], &rows);
        }
        BackupCmd::Create { profile, label, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}/backups", enc(&p.profile.name)));
            let v = c.call(c.req(Method::POST, &path).json(&json!({ "label": label }))).await?;
            if c.json {
                c.print_json(&v);
            } else {
                println!("{}", v["id"].as_str().unwrap_or("created"));
            }
        }
        BackupCmd::Restore { profile, backup, node, yes } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!("Replace the data of {} on {} with backup {backup}?", p.profile.name, p.node_name),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/backups/{}/restore", enc(&p.profile.name), enc(&backup)),
            );
            let v = c.call(c.req(Method::POST, &path).timeout(Duration::from_secs(3600))).await?;
            done(c, v, "restored");
        }
        BackupCmd::Delete { profile, backup, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/backups/{}", enc(&p.profile.name), enc(&backup)),
            );
            let v = c.call(c.req(Method::DELETE, &path)).await?;
            done(c, v, "deleted");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- recordings

pub async fn recording(args: RecordingArgs) -> R<()> {
    let c = Client::connect(&args.conn).await?;
    match args.cmd {
        RecordingCmd::List { node } => {
            if let Some(ref n) = node {
                c.check_node(n)?;
            }
            let recs: Vec<&MeshRecording> = c
                .ov
                .recordings
                .iter()
                .filter(|r| node.as_deref().is_none_or(|n| r.node_name == n))
                .collect();
            if c.json {
                c.print_json(&serde_json::to_value(&recs)?);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = recs
                .iter()
                .map(|r| {
                    let x = &r.recording;
                    vec![
                        short(x.id),
                        r.node_name.clone(),
                        ago(x.started_at_secs),
                        duration(x.duration_seconds),
                        x.frame_count.to_string(),
                        bytes(x.size_bytes),
                        x.status.clone(),
                        x.title.clone().or_else(|| x.final_url.clone()).unwrap_or_default(),
                    ]
                })
                .collect();
            table(
                &["ID", "NODE", "STARTED", "LENGTH", "FRAMES", "SIZE", "STATUS", "TITLE"],
                &rows,
            );
        }
        RecordingCmd::Show { recording } => {
            c.print_json(&serde_json::to_value(c.recording(&recording)?)?);
        }
        RecordingCmd::Delete { recordings, yes } => {
            let mut targets = Vec::new();
            for key in &recordings {
                targets.push(c.recording(key)?);
            }
            confirm(
                &format!("Delete {} recording{}?", targets.len(), if targets.len() == 1 { "" } else { "s" }),
                yes,
            )?;
            for r in targets {
                let path = c.api(&r.node_name, &format!("/recordings/{}", r.recording.id));
                c.call(c.req(Method::DELETE, &path)).await?;
                println!("deleted {}", short(r.recording.id));
            }
        }
        RecordingCmd::Export { recording, output } => {
            let r = c.recording(&recording)?;
            let path = c.api(&r.node_name, &format!("/recordings/{}/frames", r.recording.id));
            let res = c
                .req(Method::GET, &path)
                .timeout(Duration::from_secs(3600))
                .send()
                .await?;
            if !res.status().is_success() {
                return Err(format!("export failed: HTTP {}", res.status()).into());
            }
            let file = output.unwrap_or_else(|| PathBuf::from(format!("{}.jsonl", r.recording.id)));
            let data = res.bytes().await?;
            std::fs::write(&file, &data)?;
            println!("{} ({})", file.display(), bytes(data.len() as u64));
        }
        RecordingCmd::Vacuum { node } => {
            let node = c.node_or_local(node.as_deref())?;
            let v = c.call(c.req(Method::POST, &c.api(&node, "/recordings/vacuum"))).await?;
            c.print_json(&v);
        }
    }
    Ok(())
}

// --------------------------------------------------------------------- nodes

pub async fn node(args: NodeArgs) -> R<()> {
    let c = Client::connect(&args.conn).await?;
    match args.cmd {
        NodeCmd::List => {
            if c.json {
                c.print_json(&json!({ "local": c.ov.local_node, "peers": c.ov.peers }));
                return Ok(());
            }
            let l = &c.ov.local_node;
            let mut rows = vec![vec![
                l.name.clone(),
                "online (asked)".to_string(),
                format!("{}/{}", l.active_sessions, l.max_sessions),
                l.profiles_count.to_string(),
                "-".to_string(),
                l.version.clone(),
                duration(l.uptime_seconds),
            ]];
            for p in &c.ov.peers {
                rows.push(vec![
                    p.name.clone(),
                    p.error.clone().map_or_else(|| p.status.clone(), |e| format!("{} ({e})", p.status)),
                    format!("{}/{}", p.active_sessions, p.max_sessions),
                    p.profiles_count.to_string(),
                    p.latency_ms.map_or("-".into(), |ms| format!("{ms} ms")),
                    p.version.clone().unwrap_or_default(),
                    duration(p.uptime_seconds),
                ]);
            }
            table(&["NODE", "STATUS", "SESSIONS", "PROFILES", "LATENCY", "VERSION", "UP"], &rows);
        }
        NodeCmd::Show { node } => {
            let node = c.node_or_local(node.as_deref())?;
            if node == c.ov.local_node.name {
                c.print_json(&serde_json::to_value(&c.ov.local_node)?);
            } else {
                let p = c.ov.peers.iter().find(|p| p.name == node);
                c.print_json(&serde_json::to_value(p)?);
            }
        }
        NodeCmd::Set { node, default_proxy, engine, adblock, color_scheme, show_touches } => {
            let node = c.node_or_local(node.as_deref())?;
            let path = c.api(&node, "/settings");
            // The overview carries every node's settings (a peer's route only takes PATCH).
            let mut settings = if node == c.ov.local_node.name {
                serde_json::to_value(&c.ov.local_node.settings)?
            } else {
                let peer = c.ov.peers.iter().find(|p| p.name == node);
                serde_json::to_value(peer.and_then(|p| p.settings.as_ref()))?
            };
            let mut body = json!({});
            if let Some(p) = default_proxy {
                body["default_proxy"] = json!(if p == "none" { String::new() } else { p });
            }
            if adblock.is_some() || color_scheme.is_some() || show_touches.is_some() {
                let engine = engine.ok_or("--engine says which engine's defaults to change")?;
                let key = format!("{engine:?}").to_lowercase();
                let mut engines = settings["engines"].take();
                if !engines.is_object() {
                    engines = json!({});
                }
                if !engines[&key].is_object() {
                    engines[&key] = json!({});
                }
                if let Some(v) = adblock { engines[&key]["adblock"] = json!(v) }
                if let Some(v) = color_scheme { engines[&key]["color_scheme"] = json!(format!("{v:?}").to_lowercase()) }
                if let Some(v) = show_touches { engines[&key]["show_touches"] = json!(v) }
                body["engines"] = engines;
            }
            if body.as_object().is_some_and(|o| o.is_empty()) {
                return Err("Nothing to change (see --help)".into());
            }
            let v = c.call(c.req(Method::PATCH, &path).json(&body)).await?;
            c.print_json(&v);
        }
        NodeCmd::Add { peer, name } => {
            let v = c
                .call(c.req(Method::POST, "/api/peers").json(&json!({ "url": peer, "name": name })))
                .await?;
            done(&c, v, "added");
        }
        NodeCmd::Remove { node } => {
            let v = c.call(c.req(Method::DELETE, &format!("/api/peers/{}", enc(&node)))).await?;
            done(&c, v, "removed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_line_is_consistent() {
        // Catches clashing option names (e.g. a positional `url` against the global --url).
        crate::Cli::command().debug_assert();
    }

    #[test]
    fn durations_and_sizes_read_well() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(125), "2m05s");
        assert_eq!(duration(7500), "2h05m");
        assert_eq!(duration(200_000), "2d7h");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KB");
    }

    #[test]
    fn one_match_or_an_explanation() {
        let items = ["a1", "b2", "b3"];
        let hits = |p: &str| items.iter().filter(|s| s.starts_with(p)).collect::<Vec<_>>();
        assert_eq!(*one(hits("a"), "thing", "a", |s| s.to_string()).unwrap(), "a1");
        assert!(one(hits("z"), "thing", "z", |s| s.to_string()).is_err());
        let err = one(hits("b"), "thing", "b", |s| s.to_string()).unwrap_err().to_string();
        assert!(err.contains("b2") && err.contains("b3"), "{err}");
    }
}

/// `browser-daemon completions <shell>`: a completion script on stdout.
pub fn completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = crate::Cli::command();
    clap_complete::generate(shell, &mut cmd, "browser-daemon", &mut std::io::stdout());
}
