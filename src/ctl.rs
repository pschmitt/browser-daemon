//! The command-line client: `browser-daemon session list`, `profile backup create`, ...
//! It talks to the HTTP API of one node (peers are reached through it) and covers
//! what the dashboard does.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::shares::{ShareKind, ShareView};
use crate::{MeshOverview, MeshProfile, MeshRecording, MeshSession};

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// Exit statuses (clap itself uses 2 for a bad command line).
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_FOUND: i32 = 3;
pub const EXIT_UNREACHABLE: i32 = 4;
pub const EXIT_CANCELLED: i32 = 5;
pub const EXIT_UNAUTHORIZED: i32 = 6;

/// An error with its own exit status (others exit with 1).
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ExitError {}

fn fail<T>(code: i32, msg: impl Into<String>) -> R<T> {
    Err(Box::new(ExitError {
        code,
        msg: msg.into(),
    }))
}

/// The exit status for an error of a command.
pub fn exit_code(e: &(dyn std::error::Error + 'static)) -> i32 {
    e.downcast_ref::<ExitError>().map_or(1, |e| e.code)
}

const UNAUTHORIZED: &str = "Unauthorized: pass --secret-file (or set BROWSER_DAEMON_SECRET_FILE)";

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

    /// File holding the cluster secret (default: the one the NixOS module's
    /// `clusterSecretFile` exports in `$BROWSER_DAEMON_SECRET_FILE`, else
    /// /run/secrets/browser-daemon/cluster-secret when readable).
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
    /// Share a session publicly: anyone with the printed link watches its front tab
    /// (view only, no sign-in) until it stops or is unshared.
    Share {
        session: String,
        /// The link stops working after this long (`30m`, `12h`, `7d`, or `never`).
        #[arg(long, value_parser = parse_expiry, default_value = "1h")]
        expires: u64,
        /// Whoever has the link may also control the session (click, scroll, type),
        /// not only watch it (Chromium only).
        #[arg(long, conflicts_with = "view_only")]
        control: bool,
        /// Only watching (the default for a new link; turns control off).
        #[arg(long)]
        view_only: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        session: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Stop sharing a session: its public link stops working at once.
    Unshare {
        session: String,
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
    /// List what a session's pages downloaded (kept on its node until it ends).
    Downloads {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Save a file a session's page downloaded.
    Download {
        session: String,
        /// Download id (or its start), or its file name.
        download: String,
        /// Where to save it (default: its own name, here).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace an existing file.
        #[arg(short, long)]
        force: bool,
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

/// A profile setting: on, off, or the node's default.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Toggle {
    On,
    Off,
    Default,
}

impl Toggle {
    fn json(self) -> Value {
        match self {
            Toggle::On => json!(true),
            Toggle::Off => json!(false),
            Toggle::Default => Value::Null,
        }
    }
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
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        profile: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
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
        /// Record its sessions (default: the node's setting).
        #[arg(long, value_enum)]
        record: Option<Toggle>,
        /// Its recordings take the sound (default: the node's setting).
        #[arg(long, value_enum)]
        record_audio: Option<Toggle>,
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
        /// Record its sessions: on, off, or the node's default.
        #[arg(long, value_enum)]
        record: Option<Toggle>,
        /// Its recordings take the sound: on, off, or the node's default.
        #[arg(long, value_enum)]
        record_audio: Option<Toggle>,
        /// Its picture on the dashboard: a png, svg, webp, jpeg or gif file.
        #[arg(long, value_name = "FILE")]
        icon: Option<String>,
        /// Remove its picture.
        #[arg(long, conflicts_with = "icon")]
        clear_icon: bool,
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
        /// Replace the file if it exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Create a profile from an archive made by `export`.
    Import {
        name: String,
        file: PathBuf,
        #[arg(long)]
        node: Option<String>,
        /// Replace a profile of that name (asks first).
        #[arg(long)]
        overwrite: bool,
        #[arg(short, long)]
        yes: bool,
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
        /// Replace a profile of that name on the target (asks first, as does --move).
        #[arg(long)]
        overwrite: bool,
        #[arg(short, long)]
        yes: bool,
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
        #[arg(short, long)]
        yes: bool,
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
    /// Keep recordings: the cleanup (age, count and storage limits) never removes them.
    /// Nothing is kept unless asked.
    Keep {
        #[arg(required = true)]
        recordings: Vec<String>,
    },
    /// Stop keeping recordings: the cleanup may remove them again.
    Unkeep {
        #[arg(required = true)]
        recordings: Vec<String>,
    },
    /// Share a recording publicly: anyone with the printed link can play it, without
    /// signing in. A shared recording is also kept.
    Share {
        recording: String,
        /// The link stops working after this long (`30m`, `12h`, `7d`, or `never`).
        #[arg(long, value_parser = parse_expiry, default_value = "1h")]
        expires: u64,
    },
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        recording: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
    },
    /// Stop sharing a recording: its public link stops working at once.
    Unshare { recording: String },
    /// Delete recordings.
    #[command(visible_alias = "rm")]
    Delete {
        #[arg(required = true)]
        recordings: Vec<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Merge recordings of one node into a new one, in the order given (sound too).
    Merge {
        #[arg(required = true, num_args = 2..)]
        recordings: Vec<String>,
        /// Delete the originals once the merge is written.
        #[arg(long)]
        delete: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Download a recording's frames (JSON lines), or a WebM video of it with its sound.
    Export {
        recording: String,
        /// File to write (default: <id>.jsonl, or <id>.webm).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// A WebM video (VP9, with the sound), encoded on its node.
        #[arg(long)]
        webm: bool,
        /// Replace the file if it exists.
        #[arg(short, long)]
        force: bool,
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

#[derive(Args, Debug)]
pub struct ExtensionArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: ExtensionCmd,
}

#[derive(Subcommand, Debug)]
pub enum ExtensionCmd {
    /// Make a single-use pairing code for the exact origin shown in the popup.
    Pair {
        #[arg(long)]
        origin: String,
        /// Seconds the browser may remain connected (60–28800; 0 disables expiry).
        #[arg(long, default_value_t = 3600)]
        lifetime_secs: u64,
        /// Remember this browser without key expiry, allowing reconnection after restart.
        #[arg(long)]
        remember: bool,
    },
    /// List connected browsers and explicitly shared tabs on this daemon.
    List,
    /// End a browser's grant now.
    Revoke { id: Uuid },
    /// Act on one shared tab. JSON params may be supplied on stdin with `-`.
    Command {
        id: Uuid,
        tab: u32,
        action: String,
        #[arg(default_value = "{}")]
        params: String,
    },
}

pub async fn extension(args: ExtensionArgs) -> R<()> {
    let c = Client::connect(&args.conn).await?;
    let req = match args.cmd {
        ExtensionCmd::Pair {
            origin,
            lifetime_secs,
            remember,
        } => c
            .req(Method::POST, "/api/extension/pair")
            .json(&json!({"origin": origin, "lifetime_secs": lifetime_secs, "remember": remember})),
        ExtensionCmd::List => c.req(Method::GET, "/api/extension/nodes"),
        ExtensionCmd::Revoke { id } => c.req(Method::DELETE, &format!("/api/extension/nodes/{id}")),
        ExtensionCmd::Command {
            id,
            tab,
            action,
            params,
        } => {
            let params = if params == "-" {
                let mut input = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
                input
            } else {
                params
            };
            let params: Value = serde_json::from_str(&params)?;
            c.req(Method::POST, &format!("/api/extension/nodes/{id}/command"))
                .json(&json!({"tab": tab, "action": action, "params": params}))
        }
    };
    c.print_json(&c.call(req).await?);
    Ok(())
}

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
        let secret = crate::tui::find_secret(&http, &base, conn.secret_file.as_ref()).await?;
        let mut req = http
            .get(format!("{base}/api/mesh/overview"))
            .timeout(Duration::from_secs(30));
        if let Some(ref s) = secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = match req.send().await {
            Ok(res) => res,
            Err(e) => return fail(EXIT_UNREACHABLE, format!("{base}: {e}")),
        };
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return fail(EXIT_UNAUTHORIZED, UNAUTHORIZED);
        }
        if !res.status().is_success() {
            return fail(EXIT_UNREACHABLE, format!("{base}: HTTP {}", res.status()));
        }
        let ov = res.json().await?;
        Ok(Self {
            http,
            base,
            secret,
            json: conn.json,
            ov,
        })
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
        let res = match req.send().await {
            Ok(res) => res,
            Err(e) => return fail(EXIT_UNREACHABLE, format!("{}: {e}", self.base)),
        };
        let status = res.status();
        let text = res.text().await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return fail(EXIT_UNAUTHORIZED, UNAUTHORIZED);
        }
        if !status.is_success() {
            let msg = value["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        status.to_string()
                    } else {
                        text
                    }
                });
            let code = if status == reqwest::StatusCode::NOT_FOUND {
                EXIT_NOT_FOUND
            } else {
                1
            };
            return fail(code, msg);
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
            fail(EXIT_NOT_FOUND, format!("Unknown node '{node}'"))
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
        let hits: Vec<&MeshSession> = self
            .ov
            .sessions
            .iter()
            .filter(|s| node.is_none_or(|n| s.node_name == n))
            .collect();
        resolve(
            hits,
            key,
            "session",
            |s| Some(s.session.id.to_string()),
            |s| Some(s.session.name.clone()),
            |s| format!("{} {} on {}", s.session.id, s.session.name, s.node_name),
        )
    }

    fn profile(&self, key: &str, node: Option<&str>) -> R<&MeshProfile> {
        let hits: Vec<&MeshProfile> = self
            .ov
            .profiles
            .iter()
            .filter(|p| node.is_none_or(|n| p.node_name == n))
            .collect();
        resolve(
            hits,
            key,
            "profile",
            |p| p.profile.id.map(|id| id.to_string()),
            |p| Some(p.profile.name.clone()),
            |p| format!("{} on {}", p.profile.name, p.node_name),
        )
    }

    fn recording(&self, key: &str) -> R<&MeshRecording> {
        let hits: Vec<&MeshRecording> = self.ov.recordings.iter().collect();
        resolve(
            hits,
            key,
            "recording",
            |r| Some(r.recording.id.to_string()),
            |_| None,
            |r| format!("{} on {}", r.recording.id, r.node_name),
        )
    }

    fn print_json(&self, v: &Value) {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    }

    /// One result: JSON with `--json` (a line per item when a command has several
    /// targets), the short text otherwise.
    fn say(&self, v: Value, text: impl std::fmt::Display) {
        if self.json {
            println!("{}", serde_json::to_string(&v).unwrap_or_default());
        } else {
            println!("{text}");
        }
    }
}

/// Shortest id prefix that counts as a match.
const MIN_PREFIX: usize = 4;

/// The item `key` names: an exact id wins over an exact name, which wins over an
/// id prefix. Within the deciding tier exactly one item must match, else the
/// error says what was ambiguous.
fn resolve<'a, T>(
    items: Vec<&'a T>,
    key: &str,
    what: &str,
    id: impl Fn(&T) -> Option<String>,
    name: impl Fn(&T) -> Option<String>,
    describe: impl Fn(&T) -> String,
) -> R<&'a T> {
    let matches = |tier: usize, x: &T| match tier {
        0 => id(x).is_some_and(|i| i == key),
        1 => name(x).is_some_and(|n| n == key),
        _ => key.len() >= MIN_PREFIX && id(x).is_some_and(|i| i.starts_with(key)),
    };
    for tier in 0..3 {
        let hits: Vec<&T> = items.iter().copied().filter(|x| matches(tier, x)).collect();
        match hits.as_slice() {
            [] => {}
            [x] => return Ok(x),
            many => {
                return fail(
                    1,
                    format!(
                        "'{key}' matches several {what}s (use --node, or more of the id):\n  {}",
                        many.iter()
                            .map(|x| describe(x))
                            .collect::<Vec<_>>()
                            .join("\n  ")
                    ),
                )
            }
        }
    }
    let hint = if key.len() < MIN_PREFIX {
        format!(" (an id prefix needs at least {MIN_PREFIX} characters)")
    } else {
        String::new()
    };
    fail(EXIT_NOT_FOUND, format!("No {what} '{key}'{hint}"))
}

/// Several targets: the ones that resolved, and how many did not (reported as they come).
struct Tally {
    failed: usize,
    total: usize,
    code: i32,
}

impl Tally {
    fn new(total: usize) -> Self {
        Self {
            failed: 0,
            total,
            code: 1,
        }
    }

    fn fail(&mut self, what: &str, e: &(dyn std::error::Error + 'static)) {
        eprintln!("{what}: {e}");
        if self.failed == 0 {
            self.code = exit_code(e);
        }
        self.failed += 1;
    }

    fn finish(self, verb: &str) -> R<()> {
        if self.failed == 0 {
            Ok(())
        } else {
            fail(
                self.code,
                format!("{} of {} could not be {verb}", self.failed, self.total),
            )
        }
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
        return fail(EXIT_CANCELLED, format!("{question} Pass --yes to confirm."));
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        fail(EXIT_CANCELLED, "Cancelled")
    }
}

/// The agents driving a session: their name and what they say they are doing.
pub fn agents(clients: &[crate::clients::ClientInfo]) -> String {
    let mut seen: Vec<String> = Vec::new();
    for c in clients {
        let label = match (c.agent_name.as_deref(), c.topic.as_deref()) {
            _ if c.integration.is_some() => c.integration.clone().unwrap_or_default(),
            (Some(a), Some(t)) => format!("{a}: {t}"),
            (None, Some(t)) => t.to_string(),
            (Some(a), None) => a.to_string(),
            (None, None) if c.agent => c.kind.clone(),
            (None, None) => continue,
        };
        if !seen.contains(&label) {
            seen.push(label);
        }
    }
    if seen.is_empty() {
        "-".to_string()
    } else {
        seen.join("; ")
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
            let sessions: Vec<&MeshSession> =
                c.ov.sessions
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
                        agents(&x.clients),
                    ]
                })
                .collect();
            table(
                &[
                    "ID", "NAME", "NODE", "ENGINE", "PROFILE", "UP", "TABS", "REC", "PROXY",
                    "AGENT",
                ],
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
            if let Some(v) = a.name {
                set("name", json!(v))
            }
            if let Some(v) = a.engine {
                set("engine", json!(format!("{v:?}").to_lowercase()))
            }
            if let Some(v) = a.profile {
                set("profile", json!(v))
            }
            if let Some(v) = a.width {
                set("width", json!(v))
            }
            if let Some(v) = a.height {
                set("height", json!(v))
            }
            if a.record {
                set("record", json!(true))
            }
            if a.no_record {
                set("record", json!(false))
            }
            if a.adblock {
                set("adblock", json!(true))
            }
            if a.no_adblock {
                set("adblock", json!(false))
            }
            if let Some(v) = a.proxy {
                set("proxy", json!(v))
            }
            if let Some(v) = a.color_scheme {
                set("color_scheme", json!(format!("{v:?}").to_lowercase()))
            }
            if let Some(v) = a.idle_timeout {
                set("idle_timeout_minutes", json!(v))
            }
            if let Some(v) = a.max_memory_mb {
                set("max_memory_mb", json!(v))
            }
            if let Some(v) = a.screen {
                set("screen", json!(format!("{v:?}").to_lowercase()))
            }
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
                    c.ov.peers
                        .iter()
                        .find(|p| p.name == node)
                        .map(|p| p.url.clone())
                        .unwrap_or_default()
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
            let mut tally = Tally::new(sessions.len());
            let mut targets = Vec::new();
            for key in &sessions {
                match c.session(key, node.as_deref()) {
                    Ok(s) => targets.push(s),
                    Err(e) => tally.fail(key, e.as_ref()),
                }
            }
            for s in targets {
                let path = c.api(&s.node_name, &format!("/sessions/{}", s.session.id));
                match c.call(c.req(Method::DELETE, &path)).await {
                    Ok(_) => c.say(
                        json!({ "stopped": s.session.id, "name": s.session.name, "node": s.node_name }),
                        format!("stopped {} ({})", s.session.name, s.node_name),
                    ),
                    Err(e) => tally.fail(&s.session.name, e.as_ref()),
                }
            }
            tally.finish("stopped")?;
        }
        SessionCmd::StopAll { node, yes } => {
            if let Some(ref n) = node {
                c.check_node(n)?;
            }
            let sessions: Vec<&MeshSession> =
                c.ov.sessions
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
                    node.as_deref()
                        .map(|n| format!(" on {n}"))
                        .unwrap_or_default()
                ),
                yes,
            )?;
            let client = &c;
            let results = futures_util::future::join_all(sessions.iter().map(|s| {
                let path = c.api(&s.node_name, &format!("/sessions/{}", s.session.id));
                async move { client.call(client.req(Method::DELETE, &path)).await }
            }))
            .await;
            let mut tally = Tally::new(sessions.len());
            for (s, r) in sessions.iter().zip(&results) {
                if let Err(e) = r {
                    tally.fail(&s.session.name, e.as_ref());
                }
            }
            println!(
                "Stopped {} of {}",
                sessions.len() - tally.failed,
                sessions.len()
            );
            tally.finish("stopped")?;
        }
        SessionCmd::Share {
            session,
            expires,
            control,
            view_only,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let mode = (control || view_only).then_some(control);
            share(
                &c,
                &s.node_name,
                ShareKind::Session,
                s.session.id,
                expires,
                mode,
            )
            .await?;
        }
        SessionCmd::Note {
            session,
            text,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            note(
                &c,
                &s.node_name,
                "sessions",
                &s.session.id.to_string(),
                text,
            )
            .await?;
        }
        SessionCmd::Unshare { session, node } => {
            let s = c.session(&session, node.as_deref())?;
            unshare(&c, &s.node_name, s.share.as_ref()).await?;
        }
        SessionCmd::Rename {
            session,
            name,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(&s.node_name, &format!("/sessions/{}/name", s.session.id));
            let v = c
                .call(c.req(Method::POST, &path).json(&json!({ "name": name })))
                .await?;
            done(&c, v, "renamed");
        }
        SessionCmd::Proxy {
            session,
            proxy,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(&s.node_name, &format!("/sessions/{}/proxy", s.session.id));
            let v = c
                .call(c.req(Method::POST, &path).json(&json!({ "proxy": proxy })))
                .await?;
            done(&c, v, "proxy switched");
        }
        SessionCmd::IdleTimeout {
            session,
            minutes,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(
                &s.node_name,
                &format!("/sessions/{}/idle-timeout?minutes={minutes}", s.session.id),
            );
            let v = c.call(c.req(Method::POST, &path)).await?;
            done(&c, v, "idle timeout set");
        }
        SessionCmd::Record {
            session,
            action,
            node,
        } => {
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
            let tabs: Vec<_> =
                c.ov.targets
                    .iter()
                    .filter(|t| {
                        t.target.session_id == Some(s.session.id) && t.target.target_type == "page"
                    })
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
        SessionCmd::Open {
            session,
            page,
            node,
        } => {
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
            c.say(json!({ "closed": tab }), "closed");
        }
        SessionCmd::SaveAsProfile {
            session,
            name,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(
                &s.node_name,
                &format!("/sessions/{}/save-as-profile", s.session.id),
            );
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
            let url = ws_url(c.session(&session, node.as_deref())?);
            c.say(json!({ "url": url }), &url);
        }
        SessionCmd::Downloads { session, node } => {
            let s = c.session(&session, node.as_deref())?;
            let path = c.api(
                &s.node_name,
                &format!("/sessions/{}/downloads", s.session.id),
            );
            let list = c.call(c.req(Method::GET, &path)).await?;
            if c.json {
                c.print_json(&list);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = list
                .as_array()
                .into_iter()
                .flatten()
                .map(|d| {
                    let size = d["received_bytes"].as_u64().unwrap_or(0);
                    vec![
                        short_id(d["id"].as_str().unwrap_or_default()),
                        d["state"].as_str().unwrap_or_default().replace('_', " "),
                        bytes(size),
                        d["name"].as_str().unwrap_or_default().to_string(),
                        d["url"].as_str().unwrap_or_default().to_string(),
                    ]
                })
                .collect();
            if rows.is_empty() {
                println!("No downloads");
            } else {
                table(&["ID", "STATE", "SIZE", "NAME", "URL"], &rows);
            }
        }
        SessionCmd::Download {
            session,
            download,
            output,
            force,
            node,
        } => {
            let s = c.session(&session, node.as_deref())?;
            let base = c.api(
                &s.node_name,
                &format!("/sessions/{}/downloads", s.session.id),
            );
            let list = c.call(c.req(Method::GET, &base)).await?;
            let d = pick_download(list.as_array().map_or(&[][..], Vec::as_slice), &download)?;
            let id = d["id"].as_str().unwrap_or_default();
            let file = output.unwrap_or_else(|| {
                PathBuf::from(safe_filename(d["name"].as_str().unwrap_or("download")))
            });
            export_to(&c, &format!("{base}/{}", enc(id)), &file, force).await?;
        }
    }
    Ok(())
}

/// The first characters of a download id, as tables show them.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A download by its id, its name (the newest of that name: the list comes newest
/// first), or the start of its id (4 characters or more).
fn pick_download<'a>(list: &'a [Value], key: &str) -> R<&'a Value> {
    let id = |d: &Value| d["id"].as_str().unwrap_or_default().to_string();
    if let Some(d) = list.iter().find(|d| id(d) == key) {
        return Ok(d);
    }
    if let Some(d) = list.iter().find(|d| d["name"].as_str() == Some(key)) {
        return Ok(d);
    }
    let hits: Vec<_> = list
        .iter()
        .filter(|d| key.len() >= 4 && id(d).starts_with(key))
        .collect();
    match hits.as_slice() {
        [one] => return Ok(one),
        [] => {}
        _ => {
            return fail(
                1,
                format!("'{key}' fits several downloads; use more of the id"),
            )
        }
    }
    fail(
        EXIT_NOT_FOUND,
        format!("No download '{key}' (an id prefix needs at least 4 characters)"),
    )
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
            let profiles: Vec<&MeshProfile> =
                c.ov.profiles
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
                &[
                    "NAME", "NODE", "ENGINE", "SIZE", "ACTIVE", "PROXY", "SCHEME", "TABS",
                ],
                &rows,
            );
        }
        ProfileCmd::Note {
            profile,
            text,
            node,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            let key = p
                .profile
                .id
                .map_or_else(|| p.profile.name.clone(), |id| id.to_string());
            note(&c, &p.node_name, "profiles", &key, text).await?;
        }
        ProfileCmd::Show { profile, node } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(&p.node_name, &format!("/profiles/{}", enc(&p.profile.name)));
            c.print_json(&c.call(c.req(Method::GET, &path)).await?);
        }
        ProfileCmd::New {
            name,
            node,
            engine,
            clone_from,
            proxy,
            color_scheme,
            tabs,
            record,
            record_audio,
        } => {
            let node = c.node_or_local(node.as_deref())?;
            let mut body = json!({ "name": name });
            if let Some(v) = engine {
                body["engine"] = json!(format!("{v:?}").to_lowercase())
            }
            if let Some(v) = clone_from {
                body["clone_from"] = json!(v)
            }
            if let Some(v) = proxy {
                body["proxy"] = json!(v)
            }
            if let Some(v) = color_scheme {
                body["color_scheme"] = json!(format!("{v:?}").to_lowercase())
            }
            if !tabs.is_empty() {
                body["tabs"] = json!(tabs)
            }
            if let Some(v) = record {
                body["record"] = v.json()
            }
            if let Some(v) = record_audio {
                body["record_audio"] = v.json()
            }
            let v = c
                .call(c.req(Method::POST, &c.api(&node, "/profiles")).json(&body))
                .await?;
            done(&c, v, &format!("created {name} on {node}"));
        }
        ProfileCmd::Set {
            profile,
            node,
            name,
            proxy,
            color_scheme,
            tabs,
            clear_tabs,
            record,
            record_audio,
            icon,
            clear_icon,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            let icon_given = icon.is_some() || clear_icon;
            let icon_path = c.api(
                &p.node_name,
                &format!("/profiles/{}/icon", enc(&p.profile.name)),
            );
            if let Some(file) = icon {
                let mime = match std::path::Path::new(&file)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("png") => "image/png",
                    Some("svg") => "image/svg+xml",
                    Some("webp") => "image/webp",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("gif") => "image/gif",
                    _ => {
                        return fail(
                            EXIT_USAGE,
                            "The picture must be a png, svg, webp, jpeg or gif file",
                        )
                    }
                };
                let bytes = std::fs::read(&file)?;
                let req = c
                    .req(Method::PUT, &icon_path)
                    .header("Content-Type", mime)
                    .body(bytes);
                let v = c.call(req).await?;
                done(&c, v, "picture set");
            }
            if clear_icon {
                let v = c.call(c.req(Method::DELETE, &icon_path)).await?;
                done(&c, v, "picture removed");
            }
            let mut body = json!({});
            if let Some(v) = name {
                body["name"] = json!(v)
            }
            if let Some(v) = proxy {
                body["proxy"] = json!(v)
            }
            if let Some(v) = color_scheme {
                body["color_scheme"] = json!(format!("{v:?}").to_lowercase())
            }
            if clear_tabs {
                body["tabs"] = json!([])
            } else if !tabs.is_empty() {
                body["tabs"] = json!(tabs)
            }
            if let Some(v) = record {
                body["record"] = v.json()
            }
            if let Some(v) = record_audio {
                body["record_audio"] = v.json()
            }
            if icon_given && body.as_object().is_some_and(|o| o.is_empty()) {
                return Ok(());
            }
            let path = c.api(&p.node_name, &format!("/profiles/{}", enc(&p.profile.name)));
            let v = c.call(c.req(Method::PATCH, &path).json(&body)).await?;
            done(&c, v, "updated");
        }
        ProfileCmd::Clone {
            profile,
            new_name,
            node,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/clone", enc(&p.profile.name)),
            );
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .json(&json!({ "target_name": new_name })),
                )
                .await?;
            done(&c, v, "cloned");
        }
        ProfileCmd::Delete {
            profile,
            node,
            force,
            yes,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!(
                    "Delete profile {} on {}? Cookies and logins are wiped.",
                    p.profile.name, p.node_name
                ),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}?force={force}", enc(&p.profile.name)),
            );
            let v = c.call(c.req(Method::DELETE, &path)).await?;
            done(&c, v, "deleted");
        }
        ProfileCmd::Clear {
            profile,
            node,
            force,
            no_backup,
            yes,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!(
                    "Wipe the data of profile {} on {}?",
                    p.profile.name, p.node_name
                ),
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
        ProfileCmd::Export {
            profile,
            node,
            output,
            force,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/export", enc(&p.profile.name)),
            );
            let file = output.unwrap_or_else(|| {
                PathBuf::from(format!("{}.tar.gz", safe_filename(&p.profile.name)))
            });
            export_to(&c, &path, &file, force).await?;
        }
        ProfileCmd::Import {
            name,
            file,
            node,
            overwrite,
            yes,
        } => {
            let node = c.node_or_local(node.as_deref())?;
            if overwrite {
                confirm(
                    &format!(
                        "Replace any profile named {name} on {node} with {}?",
                        file.display()
                    ),
                    yes,
                )?;
            }
            let data = std::fs::read(&file)?;
            let path = c.api(
                &node,
                &format!("/profiles/{}/import?overwrite={overwrite}", enc(&name)),
            );
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .timeout(Duration::from_secs(3600))
                        .body(data),
                )
                .await?;
            done(&c, v, &format!("imported {name} on {node}"));
        }
        ProfileCmd::Transfer {
            profile,
            to,
            node,
            name,
            move_,
            overwrite,
            yes,
        } => {
            c.check_node(&to)?;
            let p = c.profile(&profile, node.as_deref())?;
            if move_ || overwrite {
                let target = name.as_deref().unwrap_or(&p.profile.name);
                let mut what = Vec::new();
                if move_ {
                    what.push(format!(
                        "delete {} on {} once copied",
                        p.profile.name, p.node_name
                    ));
                }
                if overwrite {
                    what.push(format!("replace any profile named {target} on {to}"));
                }
                confirm(&format!("This will {}. Continue?", what.join(" and ")), yes)?;
            }
            let mut body =
                json!({ "target_node": to, "delete_source": move_, "overwrite": overwrite });
            if let Some(n) = name {
                body["target_name"] = json!(n);
            }
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/transfer", enc(&p.profile.name)),
            );
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .timeout(Duration::from_secs(3600))
                        .json(&body),
                )
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
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/backups", enc(&p.profile.name)),
            );
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
                        if b["automatic"].as_bool().unwrap_or(false) {
                            "auto"
                        } else {
                            "manual"
                        }
                        .to_string(),
                        b["label"].as_str().unwrap_or("").to_string(),
                    ]
                })
                .collect();
            table(&["ID", "CREATED", "SIZE", "KIND", "LABEL"], &rows);
        }
        BackupCmd::Create {
            profile,
            label,
            node,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            let path = c.api(
                &p.node_name,
                &format!("/profiles/{}/backups", enc(&p.profile.name)),
            );
            let v = c
                .call(c.req(Method::POST, &path).json(&json!({ "label": label })))
                .await?;
            if c.json {
                c.print_json(&v);
            } else {
                println!("{}", v["id"].as_str().unwrap_or("created"));
            }
        }
        BackupCmd::Restore {
            profile,
            backup,
            node,
            yes,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!(
                    "Replace the data of {} on {} with backup {backup}?",
                    p.profile.name, p.node_name
                ),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!(
                    "/profiles/{}/backups/{}/restore",
                    enc(&p.profile.name),
                    enc(&backup)
                ),
            );
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .timeout(Duration::from_secs(3600)),
                )
                .await?;
            done(c, v, "restored");
        }
        BackupCmd::Delete {
            profile,
            backup,
            node,
            yes,
        } => {
            let p = c.profile(&profile, node.as_deref())?;
            confirm(
                &format!(
                    "Delete backup {backup} of {} on {}?",
                    p.profile.name, p.node_name
                ),
                yes,
            )?;
            let path = c.api(
                &p.node_name,
                &format!(
                    "/profiles/{}/backups/{}",
                    enc(&p.profile.name),
                    enc(&backup)
                ),
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
            let recs: Vec<&MeshRecording> =
                c.ov.recordings
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
                        if x.keep { "kept" } else { "" }.to_string(),
                        x.title
                            .clone()
                            .or_else(|| x.final_url.clone())
                            .unwrap_or_default(),
                    ]
                })
                .collect();
            table(
                &[
                    "ID", "NODE", "STARTED", "LENGTH", "FRAMES", "SIZE", "STATUS", "KEEP", "TITLE",
                ],
                &rows,
            );
        }
        RecordingCmd::Show { recording } => {
            c.print_json(&serde_json::to_value(c.recording(&recording)?)?);
        }
        RecordingCmd::Share { recording, expires } => {
            let r = c.recording(&recording)?;
            share(
                &c,
                &r.node_name,
                ShareKind::Recording,
                r.recording.id,
                expires,
                None,
            )
            .await?;
        }
        RecordingCmd::Note { recording, text } => {
            let r = c.recording(&recording)?;
            note(
                &c,
                &r.node_name,
                "recordings",
                &r.recording.id.to_string(),
                text,
            )
            .await?;
        }
        RecordingCmd::Unshare { recording } => {
            let r = c.recording(&recording)?;
            unshare(&c, &r.node_name, r.share.as_ref()).await?;
        }
        RecordingCmd::Keep { recordings } => keep_recordings(&c, &recordings, true).await?,
        RecordingCmd::Unkeep { recordings } => keep_recordings(&c, &recordings, false).await?,
        RecordingCmd::Delete { recordings, yes } => {
            let mut tally = Tally::new(recordings.len());
            let mut targets = Vec::new();
            for key in &recordings {
                match c.recording(key) {
                    Ok(r) => targets.push(r),
                    Err(e) => tally.fail(key, e.as_ref()),
                }
            }
            if targets.is_empty() {
                return tally.finish("deleted");
            }
            let kept = targets.iter().filter(|r| r.recording.keep).count();
            confirm(
                &format!(
                    "Delete {} recording{}{}?",
                    targets.len(),
                    if targets.len() == 1 { "" } else { "s" },
                    if kept > 0 {
                        format!(" ({kept} marked to be kept)")
                    } else {
                        String::new()
                    }
                ),
                yes,
            )?;
            for r in targets {
                let path = c.api(&r.node_name, &format!("/recordings/{}", r.recording.id));
                match c.call(c.req(Method::DELETE, &path)).await {
                    Ok(_) => c.say(
                        json!({ "deleted": r.recording.id, "node": r.node_name }),
                        format!("deleted {}", short(r.recording.id)),
                    ),
                    Err(e) => tally.fail(&short(r.recording.id), e.as_ref()),
                }
            }
            tally.finish("deleted")?;
        }
        RecordingCmd::Merge {
            recordings,
            delete,
            yes,
        } => {
            let mut targets = Vec::new();
            for key in &recordings {
                targets.push(c.recording(key)?);
            }
            let node = targets[0].node_name.clone();
            if targets.iter().any(|r| r.node_name != node) {
                return Err(
                    "Merging works within one node: pick recordings of the same node".into(),
                );
            }
            if delete {
                let kept = targets.iter().filter(|r| r.recording.keep).count();
                confirm(
                    &format!(
                        "Merge {} recordings and delete them{}?",
                        targets.len(),
                        if kept > 0 {
                            format!(" ({kept} marked to be kept)")
                        } else {
                            String::new()
                        }
                    ),
                    yes,
                )?;
            }
            let ids: Vec<_> = targets.iter().map(|r| r.recording.id).collect();
            let body = json!({ "ids": ids, "delete": delete });
            let path = c.api(&node, "/recordings/merge");
            let v = c
                .call(
                    c.req(Method::POST, &path)
                        .timeout(Duration::from_secs(1800))
                        .json(&body),
                )
                .await?;
            let text = format!(
                "merged into {}",
                v["id"]
                    .as_str()
                    .map_or_else(String::new, |id| id.chars().take(8).collect())
            );
            c.say(v, text);
        }
        RecordingCmd::Export {
            recording,
            output,
            webm,
            force,
        } => {
            let r = c.recording(&recording)?;
            let what = if webm { "export.webm" } else { "frames" };
            let path = c.api(
                &r.node_name,
                &format!("/recordings/{}/{what}", r.recording.id),
            );
            let ext = if webm { "webm" } else { "jsonl" };
            let file = output.unwrap_or_else(|| PathBuf::from(format!("{}.{ext}", r.recording.id)));
            export_to(&c, &path, &file, force).await?;
        }
        RecordingCmd::Vacuum { node } => {
            let node = c.node_or_local(node.as_deref())?;
            let v = c
                .call(c.req(Method::POST, &c.api(&node, "/recordings/vacuum")))
                .await?;
            c.print_json(&v);
        }
    }
    Ok(())
}

/// File name made of the characters that are safe everywhere.
pub fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    if cleaned.is_empty() {
        "export".to_string()
    } else {
        cleaned.chars().take(100).collect()
    }
}

/// What the API said was wrong with a failed download.
pub async fn download_error(res: reqwest::Response) -> String {
    let status = res.status();
    let why = res
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string));
    format!(
        "export failed: {}",
        why.unwrap_or_else(|| format!("HTTP {status}"))
    )
}

/// Stream a successful download to `file` through a temporary file next to it,
/// which replaces `file` only once complete. An existing file is kept unless `force`.
pub async fn save_stream(res: reqwest::Response, file: &std::path::Path, force: bool) -> R<u64> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    if !force && file.exists() {
        return fail(
            1,
            format!("{} exists (use --force to replace it)", file.display()),
        );
    }
    let dir = match file.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => std::path::Path::new("."),
    };
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let mut out = tokio::fs::File::from_std(tmp.as_file().try_clone()?);
    let mut stream = res.bytes_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    drop(out);
    if force {
        tmp.persist(file).map_err(|e| e.error)?;
    } else {
        tmp.persist_noclobber(file).map_err(|e| e.error)?;
    }
    Ok(size)
}

async fn export_to(c: &Client, path: &str, file: &std::path::Path, force: bool) -> R<()> {
    let res = c
        .req(Method::GET, path)
        .timeout(Duration::from_secs(3600))
        .send()
        .await
        .map_err(|e| format!("{}: {e}", c.base))?;
    if !res.status().is_success() {
        return fail(1, download_error(res).await);
    }
    let size = save_stream(res, file, force).await?;
    c.say(
        json!({ "file": file, "bytes": size }),
        format!("{} ({})", file.display(), bytes(size)),
    );
    Ok(())
}

/// `30m`, `12h`, `7d`, `2w` (or plain seconds) as seconds; `never` is 0.
fn parse_expiry(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(0);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| format!("'{s}' is not a duration like 30m, 12h or 7d"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("unknown unit in '{s}' (s, m, h, d or w)")),
    };
    match n.checked_mul(mult) {
        Some(0) | None => Err(format!("'{s}' is not a usable duration")),
        Some(secs) => Ok(secs),
    }
}

/// Share a recording or session publicly; prints the link.
async fn share(
    c: &Client,
    node: &str,
    kind: ShareKind,
    id: Uuid,
    expires: u64,
    control: Option<bool>,
) -> R<()> {
    let path = c.api(node, "/shares");
    let v = c
        .call(c.req(Method::POST, &path).json(&json!({
            "kind": kind,
            "id": id,
            "expires_in_secs": (expires > 0).then_some(expires),
            "control": control,
        })))
        .await?;
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    c.say(v, url);
    Ok(())
}

/// Add a note to a session, profile or recording, or list its notes.
async fn note(c: &Client, node: &str, kind: &str, key: &str, text: Option<String>) -> R<()> {
    let path = c.api(node, &format!("/notes/{kind}/{}", enc(key)));
    let Some(mut text) = text else {
        let v = c.call(c.req(Method::GET, &path)).await?;
        if c.json {
            c.print_json(&v);
            return Ok(());
        }
        let notes = v.as_array().cloned().unwrap_or_default();
        if notes.is_empty() {
            println!("no notes");
        }
        for n in notes {
            println!(
                "{}  {}\n{}\n",
                short(n["id"].as_str().unwrap_or_default()),
                ago(n["created_at"].as_u64().unwrap_or_default()),
                n["text"].as_str().unwrap_or_default()
            );
        }
        return Ok(());
    };
    if text == "-" {
        text = std::io::read_to_string(std::io::stdin())?;
    }
    let v = c
        .call(c.req(Method::POST, &path).json(&json!({ "text": text })))
        .await?;
    done(c, v, "noted");
    Ok(())
}

/// Remove a public link.
async fn unshare(c: &Client, node: &str, share: Option<&ShareView>) -> R<()> {
    let Some(share) = share else {
        return fail(3, "It is not shared");
    };
    let path = c.api(node, &format!("/shares/{}", share.share.id));
    let v = c.call(c.req(Method::DELETE, &path)).await?;
    done(c, v, "unshared");
    Ok(())
}

/// Mark recordings to be kept, or not.
async fn keep_recordings(c: &Client, recordings: &[String], keep: bool) -> R<()> {
    let mut tally = Tally::new(recordings.len());
    for key in recordings {
        let r = match c.recording(key) {
            Ok(r) => r,
            Err(e) => {
                tally.fail(key, e.as_ref());
                continue;
            }
        };
        let path = c.api(
            &r.node_name,
            &format!("/recordings/{}/keep", r.recording.id),
        );
        match c
            .call(c.req(Method::POST, &path).json(&json!({ "keep": keep })))
            .await
        {
            Ok(_) => c.say(
                json!({ "id": r.recording.id, "node": r.node_name, "keep": keep }),
                format!(
                    "{} {}",
                    if keep { "keeping" } else { "not keeping" },
                    short(r.recording.id)
                ),
            ),
            Err(e) => tally.fail(key, e.as_ref()),
        }
    }
    tally.finish(if keep { "kept" } else { "released" })
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
                    p.error
                        .clone()
                        .map_or_else(|| p.status.clone(), |e| format!("{} ({e})", p.status)),
                    format!("{}/{}", p.active_sessions, p.max_sessions),
                    p.profiles_count.to_string(),
                    p.latency_ms.map_or("-".into(), |ms| format!("{ms} ms")),
                    p.version.clone().unwrap_or_default(),
                    duration(p.uptime_seconds),
                ]);
            }
            table(
                &[
                    "NODE", "STATUS", "SESSIONS", "PROFILES", "LATENCY", "VERSION", "UP",
                ],
                &rows,
            );
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
        NodeCmd::Set {
            node,
            default_proxy,
            engine,
            adblock,
            color_scheme,
            show_touches,
        } => {
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
                    return Err(format!("{node}'s engine settings are unknown (peer unreachable?); not changing them blind").into());
                }
                if !engines[&key].is_object() {
                    engines[&key] = json!({});
                }
                if let Some(v) = adblock {
                    engines[&key]["adblock"] = json!(v)
                }
                if let Some(v) = color_scheme {
                    engines[&key]["color_scheme"] = json!(format!("{v:?}").to_lowercase())
                }
                if let Some(v) = show_touches {
                    engines[&key]["show_touches"] = json!(v)
                }
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
                .call(
                    c.req(Method::POST, "/api/peers")
                        .json(&json!({ "url": peer, "name": name })),
                )
                .await?;
            done(&c, v, "added");
        }
        NodeCmd::Remove { node } => {
            let v = c
                .call(c.req(Method::DELETE, &format!("/api/peers/{}", enc(&node))))
                .await?;
            done(&c, v, "removed");
        }
    }
    Ok(())
}

/// `browser-daemon completions <shell>`: a completion script on stdout.
pub fn completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = crate::Cli::command();
    clap_complete::generate(shell, &mut cmd, "browser-daemon", &mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_download_is_found_by_id_name_or_id_start() {
        let list = vec![
            serde_json::json!({"id": "aaaa1111", "name": "a.txt"}),
            serde_json::json!({"id": "aaaa2222", "name": "a.txt"}),
            serde_json::json!({"id": "bbbb3333", "name": "b.txt"}),
        ];
        assert_eq!(
            super::pick_download(&list, "a.txt").unwrap()["id"],
            "aaaa1111",
            "newest of the name"
        );
        assert_eq!(
            super::pick_download(&list, "bbbb").unwrap()["name"],
            "b.txt"
        );
        assert!(
            super::pick_download(&list, "aaaa").is_err(),
            "an ambiguous id start"
        );
        assert!(super::pick_download(&list, "bb").is_err(), "too short");
    }

    use super::*;

    #[test]
    fn link_expiry_durations() {
        assert_eq!(parse_expiry("1h"), Ok(3600));
        assert_eq!(parse_expiry("30m"), Ok(1800));
        assert_eq!(parse_expiry("7d"), Ok(7 * 86_400));
        assert_eq!(parse_expiry("never"), Ok(0));
        assert!(parse_expiry("0").is_err());
        assert!(parse_expiry("1y").is_err());
        assert!(parse_expiry("soon").is_err());
    }
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

    #[derive(Debug)]
    struct Item(&'static str, &'static str);

    fn find<'a>(items: &'a [Item], key: &str) -> R<&'a Item> {
        resolve(
            items.iter().collect(),
            key,
            "thing",
            |i| Some(i.0.to_string()),
            |i| Some(i.1.to_string()),
            |i| format!("{} {}", i.0, i.1),
        )
    }

    #[test]
    fn exact_id_beats_name_beats_prefix() {
        let items = [
            Item("abcd1111", "web"),
            Item("abcd2222", "abcd1111"),
            Item("web99999", "x"),
        ];
        // An exact id wins over another item named like it.
        assert_eq!(find(&items, "abcd1111").unwrap().1, "web");
        // An exact name wins over an id prefix.
        assert_eq!(find(&items, "web").unwrap().0, "abcd1111");
        // A prefix works when unambiguous.
        assert_eq!(find(&items, "web9").unwrap().0, "web99999");
    }

    #[test]
    fn ambiguity_and_misses_are_explained() {
        let items = [Item("abcd1111", "a"), Item("abcd2222", "b")];
        let err = find(&items, "abcd").unwrap_err().to_string();
        assert!(
            err.contains("abcd1111") && err.contains("abcd2222"),
            "{err}"
        );
        let err = find(&items, "ab").unwrap_err();
        assert!(err.to_string().contains("at least 4"), "{err}");
        assert_eq!(exit_code(err.as_ref()), EXIT_NOT_FOUND);
        assert_eq!(
            exit_code(find(&items, "zzzzzz").unwrap_err().as_ref()),
            EXIT_NOT_FOUND
        );
    }

    #[test]
    fn filenames_are_made_safe() {
        assert_eq!(safe_filename("work/../x y"), "work_.._x_y");
        assert_eq!(safe_filename("..hidden"), "hidden");
        assert_eq!(safe_filename("///"), "___");
        assert_eq!(safe_filename(""), "export");
        assert_eq!(safe_filename("..."), "export");
    }

    #[test]
    fn agents_show_name_and_topic() {
        let client = |agent_name: Option<&str>, topic: Option<&str>, agent: bool| {
            crate::clients::ClientInfo {
                kind: "Playwright".into(),
                agent,
                from: "n".into(),
                addr: None,
                user_agent: None,
                since: 0,
                agent_name: agent_name.map(str::to_string),
                conversation: None,
                cwd: None,
                machine: None,
                integration: None,
                topic: topic.map(str::to_string),
            }
        };
        assert_eq!(agents(&[]), "-");
        assert_eq!(agents(&[client(None, None, false)]), "-");
        assert_eq!(agents(&[client(None, None, true)]), "Playwright");
        assert_eq!(
            agents(&[
                client(Some("claude"), Some("netbox"), true),
                client(Some("claude"), Some("netbox"), true)
            ]),
            "claude: netbox"
        );
    }
}
