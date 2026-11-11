//! `browser-daemon mcp`: one MCP server for every node and profile of the mesh.
//!
//! The browser tools are Playwright MCP's, run as a child on a CDP endpoint of
//! a node. Playwright MCP cannot change that endpoint once it runs, so this
//! server swaps the child when the agent asks for another profile or node
//! (`browser_use`), and adds `browser_status` to see what exists. Without a
//! choice the agent gets a throwaway browser on the node with the most room.
//!
//! The profiles an agent may open are limited (`--allow-profile`, `*` for all):
//! a profile stays signed in to whatever the user signed it in to.
//!
//! `browser_fill_secret` types a field of an rbw vault entry into the page
//! without it passing through the agent: the value is read here, checked
//! against the page's site, sent to the node over its API (never in a Playwright
//! script, which Playwright MCP would echo) and masked in later tool results.
//! Only the entries named with `--allow-secret` (`*` for all) can be filled.
//!
//! `browser_passkey` arms the agent's browser for passkey sign-ins on the sites
//! named with `--allow-passkey`: the node has the secret agent sign them (see
//! agent_passkeys.rs). `browser_ask_human` runs `--ask-human-cmd` with a link
//! to the live view of the agent's own page, for what only a person may do.
//!
//! `browser_export_auth` and `browser_fetch` hand a page's login (cookies, a
//! storage token) to the agent's other tools without the agent seeing it: on the
//! sites named with `--allow-export`, written to a private file that is deleted
//! after a while, or used for one HTTP request here. The values are masked in
//! all later results, as filled secrets are.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child as Proc, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::ctl::Conn;
use crate::secrets;
use crate::MeshOverview;

type R<T> = Result<T, Box<dyn std::error::Error>>;
type Rpc = Result<Value, (i64, String)>;

#[derive(Args, Debug)]
pub struct McpArgs {
    #[command(flatten)]
    pub conn: Conn,

    /// A profile agents may open (repeat for several, `*` for all). Throwaway browsers need no permission.
    #[arg(long = "allow-profile")]
    pub allow: Vec<String>,

    /// An rbw vault entry agents may fill into pages with browser_fill_secret (repeat for several, `*` for all).
    #[arg(long = "allow-secret")]
    pub allow_secrets: Vec<String>,

    /// Card entry and exact merchant and frame HTTPS origins, separated by `|` (repeatable).
    #[arg(long = "allow-card")]
    pub allow_cards: Vec<String>,

    /// A site (relying party, with its subdomains) agents may sign in to with a vault passkey through browser_passkey (repeat for several, `*` for all).
    #[arg(long = "allow-passkey")]
    pub allow_passkeys: Vec<String>,

    /// Program browser_ask_human runs to ask the user for help, called as `PROGRAM MESSAGE` with BD_HANDOFF_URL (the live view of the agent's page), BD_PAGE, BD_NODE, BD_NODE_URL, BD_TITLE and BD_AGENT (the MCP client's name) in its environment.
    #[arg(long)]
    pub ask_human_cmd: Option<PathBuf>,

    /// A site (with its subdomains) whose login agents may export with browser_export_auth or use with browser_fetch (repeat for several, `*` for all).
    #[arg(long = "allow-export")]
    pub allow_exports: Vec<String>,

    /// RBW agent HTTPS API origin.
    #[arg(long, requires_all = ["rbw_api_ca", "rbw_api_cert", "rbw_api_key"])]
    pub rbw_api_url: Option<String>,

    /// RBW API CA certificate.
    #[arg(long, requires = "rbw_api_url")]
    pub rbw_api_ca: Option<PathBuf>,
    #[arg(long, requires = "rbw_api_url")]
    pub rbw_api_cert: Vec<PathBuf>,
    #[arg(long, requires = "rbw_api_url")]
    pub rbw_api_key: Vec<PathBuf>,

    /// Program that runs Playwright MCP on a CDP endpoint, called as `PROGRAM ENDPOINT OUTPUT_DIR`.
    #[arg(long, default_value = "browser-mcp")]
    pub playwright_cmd: PathBuf,

    /// Where Playwright MCP keeps its output (default: ~/.cache/playwright-mcp/browser).
    #[arg(long)]
    pub output_dir: Option<PathBuf>,

    /// Seconds to wait for a bot wall (Cloudflare and the like) to clear by itself after a navigation or click; 0 only reports it.
    #[arg(long, default_value_t = 15)]
    pub challenge_wait: u64,

    /// Let agents make public links to their session or a recording with browser_share (off by default).
    #[arg(long)]
    pub allow_share: bool,

    /// Answer browser_take_screenshot with the censored screenshot (passwords, cards, one-time codes, filled inputs and secrets blacked out).
    #[arg(long)]
    pub censor_screenshots: bool,

    /// Ask the node to make the agent's mouse and keyboard input human-like (`human_input=1` on the CDP address).
    #[arg(long)]
    pub human_input: bool,
}

const INSTRUCTIONS: &str = "Browsers run on a mesh of nodes. Without a choice you get a \
throwaway browser on the node with the most room. To work in a saved profile (its logins and \
cookies persist, e.g. `work` for Outlook and OpsGenie) call browser_use with its name; \
browser_status lists the profiles you may open and the nodes. For a shared profile, \
browser_profile_tabs lists tabs and their owners; leave other clients' tabs alone. \
Switching closes the current browser. \
For the user's own running browsers, browser_shared_nodes lists temporary extension registrations \
and browser_shared_command acts on their explicitly shared tabs; browser_use selects managed browsers. \
Never type a password, token or TOTP code yourself: use browser_fill_secret when it is listed. \
For a passkey sign-in call browser_passkey first; for a CAPTCHA, an approval on another device or \
anything else only a person may do, call browser_ask_human when it is listed. \
Label your session with browser_set_topic when you start and when the task changes. \
Prefer browser_screenshot_censored for screenshots; use browser_take_screenshot only when the \
censored one gets in the way of the task, as it may show credentials.";

/// A node and profile the browser runs on.
#[derive(Clone)]
struct Target {
    node: String,
    /// The node's HTTP(S) base address.
    url: String,
    ws_base: String,
    profile: Option<String>,
    session_id: Option<String>,
}

impl Target {
    fn describe(&self) -> String {
        match &self.profile {
            Some(p) => format!("profile {p} on {}", self.node),
            None => format!("a throwaway browser on {}", self.node),
        }
    }
}

/// The CDP endpoint of a target, with the auth token when there is one.
fn cdp_endpoint(target: &Target, token: Option<&str>, human_input: bool) -> String {
    let mut query = Vec::new();
    if let Some(id) = &target.session_id {
        query.push(format!("session_id={}", url_component(id)));
    }
    if let Some(p) = &target.profile {
        query.push(format!(
            "profile={}",
            url::form_urlencoded::byte_serialize(p.as_bytes()).collect::<String>()
        ));
    }
    if let Some(t) = token {
        query.push(format!(
            "auth={}",
            url::form_urlencoded::byte_serialize(t.as_bytes()).collect::<String>()
        ));
    }
    if human_input {
        query.push("human_input=1".to_string());
    }
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    format!("{}/chromium{query}", target.ws_base)
}

fn url_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// A running Playwright MCP.
struct Child {
    stdin: Mutex<ChildStdin>,
    proc: Mutex<Proc>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next: AtomicU64,
    tools: std::sync::Mutex<Vec<Value>>,
    target: Target,
    used: AtomicBool,
}

fn profile_warning(ov: &MeshOverview, target: &Target, min_clients: usize) -> Option<String> {
    let profile = target.profile.as_ref()?;
    let session = ov.sessions.iter().find(|s| {
        s.node_name == target.node
            && s.session.profile.as_ref() == Some(profile)
            && s.session.clients.len() >= min_clients
    })?;
    Some(session_warning(&session.session))
}

fn session_warning(session: &crate::state::SessionSummary) -> String {
    let people: Vec<_> = session
        .clients
        .iter()
        .map(|client| client.tab_label())
        .collect();
    let who = if people.is_empty() {
        String::new()
    } else {
        format!(" Connected clients: {}.", people.join(", "))
    };
    format!("Warning: this profile already has a running session.{who} You share its browser and tabs; avoid disturbing other clients' tabs.")
}

impl Child {
    async fn write(&self, msg: &Value) -> bool {
        let mut line = msg.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
    }

    async fn request(&self, method: &str, params: Value, wait: Duration) -> Rpc {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let sent = self
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        if !sent {
            self.pending.lock().await.remove(&id);
            return Err((-32000, "The browser tools stopped".into()));
        }
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(resp)) => match resp.get("error") {
                Some(e) => Err((
                    e["code"].as_i64().unwrap_or(-32000),
                    e["message"].as_str().unwrap_or("error").to_string(),
                )),
                None => Ok(resp.get("result").cloned().unwrap_or(Value::Null)),
            },
            _ => {
                self.pending.lock().await.remove(&id);
                Err((-32000, "The browser tools stopped or took too long".into()))
            }
        }
    }

    async fn stop(&self) {
        let _ = self.proc.lock().await.kill().await;
    }
}

struct State {
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
    allow: Vec<String>,
    allow_secrets: Vec<String>,
    allow_cards: Vec<String>,
    allow_passkeys: Vec<String>,
    ask_human_cmd: Option<PathBuf>,
    allow_exports: Vec<String>,
    /// Files browser_export_auth wrote, removed when they expire or on exit.
    exports: std::sync::Mutex<Vec<PathBuf>>,
    rbw_api: Vec<crate::rbw_api::Client>,
    /// Secrets filled in this session, masked in every tool result.
    filled: std::sync::Mutex<Vec<String>>,
    cmd: PathBuf,
    out_dir: PathBuf,
    /// Seconds to wait for a bot wall to clear (0: only report it).
    challenge_wait: u64,
    /// Ask the node for human-like input (`human_input=1` on the CDP address).
    human_input: bool,
    /// browser_share and browser_unshare are offered.
    allow_share: bool,
    /// browser_take_screenshot is answered by the censored implementation.
    censor_screenshots: bool,
    /// The agent's conversation id (from its environment), to label its connections.
    conversation: Option<String>,
    /// What the agent said it is doing (browser_set_topic).
    topic: std::sync::Mutex<Option<String>>,
    /// The topic reached a connected browser (else it is applied after the next tool call).
    topic_applied: AtomicBool,
    /// Public links made here: (share id, node name, node address).
    shares: std::sync::Mutex<Vec<(String, String, String)>>,
    version: Mutex<String>,
    /// What the agent's MCP client calls itself (`clientInfo.name` of its initialize).
    client_name: Mutex<String>,
    child: Mutex<Option<Arc<Child>>>,
    stdout: Mutex<tokio::io::Stdout>,
    /// The tool names the client was last given (None before its first tools/list),
    /// to tell it when that changes.
    listed: std::sync::Mutex<Option<Vec<String>>>,
    /// A background retry of the browser tools is running.
    retrying: AtomicBool,
}

pub async fn run(args: McpArgs) -> R<()> {
    let rbw_api = match (&args.rbw_api_url, &args.rbw_api_ca) {
        (Some(url), Some(ca))
            if !args.rbw_api_cert.is_empty()
                && args.rbw_api_cert.len() == args.rbw_api_key.len() =>
        {
            args.rbw_api_cert
                .iter()
                .zip(&args.rbw_api_key)
                .map(|(cert, key)| crate::rbw_api::Client::new(url, ca, cert, key))
                .collect::<Result<Vec<_>, _>>()?
        }
        (None, None) if args.rbw_api_cert.is_empty() && args.rbw_api_key.is_empty() => vec![],
        _ => {
            return Err(
                "RBW API URL, CA and matching certificate/key pairs must all be configured".into(),
            )
        }
    };
    if rbw_api.is_empty() && (!args.allow_secrets.is_empty() || !args.allow_cards.is_empty()) {
        return Err(
            "Secret and card fills require the RBW agent API URL, CA, client certificate and key"
                .into(),
        );
    }
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()?;
    let base = args.conn.url.trim_end_matches('/').to_string();
    let secret = crate::tui::find_secret(&http, &base, args.conn.secret_file.as_ref()).await?;
    let out_dir = args.output_dir.clone().unwrap_or_else(|| {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("playwright-mcp/browser")
    });
    let state = Arc::new(State {
        http,
        base,
        secret,
        allow: args.allow,
        allow_secrets: args.allow_secrets,
        allow_cards: args.allow_cards,
        allow_passkeys: args.allow_passkeys,
        ask_human_cmd: args.ask_human_cmd,
        allow_exports: args.allow_exports,
        exports: std::sync::Mutex::new(Vec::new()),
        rbw_api,
        filled: std::sync::Mutex::new(Vec::new()),
        cmd: args.playwright_cmd,
        out_dir,
        challenge_wait: args.challenge_wait,
        human_input: args.human_input,
        allow_share: args.allow_share,
        censor_screenshots: args.censor_screenshots,
        conversation: conversation_id(|k| std::env::var(k).ok()),
        topic: std::sync::Mutex::new(None),
        topic_applied: AtomicBool::new(false),
        shares: std::sync::Mutex::new(Vec::new()),
        version: Mutex::new("2025-06-18".into()),
        client_name: Mutex::new(String::new()),
        child: Mutex::new(None),
        stdout: Mutex::new(tokio::io::stdout()),
        listed: std::sync::Mutex::new(None),
        retrying: AtomicBool::new(false),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let st = state.clone();
        tokio::spawn(async move { st.handle(msg).await });
    }
    for file in state.exports.lock().unwrap().drain(..) {
        let _ = std::fs::remove_file(file);
    }
    if let Some(child) = state.child.lock().await.take() {
        child.stop().await;
    }
    Ok(())
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Where the browser tools' list is kept between runs, so they can be listed
/// before (or without) a browser: `$XDG_CACHE_HOME/browser-daemon/mcp-tools.json`.
fn tool_cache_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("browser-daemon/mcp-tools.json"))
}

fn load_tool_cache() -> Vec<Value> {
    tool_cache_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
        .unwrap_or_default()
}

fn save_tool_cache(tools: &[Value]) {
    let Some(path) = tool_cache_path() else {
        return;
    };
    if tools.is_empty() || load_tool_cache() == tools {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Written aside and renamed, so a parallel reader never sees half a file.
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(tools).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Whether an `--allow-*` list lets agents use `name`: named in it, or `*` is.
fn allows(list: &[String], name: &str) -> bool {
    list.iter().any(|a| a == "*" || a == name)
}

/// An `--allow-*` list for agents to read.
fn allowed(list: &[String]) -> String {
    if list.is_empty() {
        "none".to_string()
    } else if allows(list, "*") {
        "any".to_string()
    } else {
        list.join(", ")
    }
}

fn text_result(text: impl Into<String>, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": error })
}

#[cfg(test)]
fn own_tools(
    secrets: &[String],
    passkeys: &[String],
    ask_human: bool,
    exports: &[String],
) -> Vec<Value> {
    own_tools_with(secrets, passkeys, ask_human, exports, false, false)
}

fn own_tools_with(
    secrets: &[String],
    passkeys: &[String],
    ask_human: bool,
    exports: &[String],
    cards: bool,
    share: bool,
) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "browser_status",
            "description": "What this browser can use: the browser in use now, the profiles you may open (saved logins and cookies) and the nodes with their load.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "title": "Browser status", "readOnlyHint": true }
        }),
        json!({
            "name": "browser_profile_tabs",
            "description": "List the tabs of the currently selected saved profile, including their advisory owners. Popups inherit their opener's owner when known. Use this to identify other clients' tabs before interacting; unknown owners are null. The target IDs are browser IDs, not Playwright tab indexes.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "title": "Profile tabs and owners", "readOnlyHint": true }
        }),
        json!({
            "name": "browser_use",
            "description": "Switch to another browser: a saved profile (its logins persist, e.g. `work` for Outlook and OpsGenie), or a throwaway browser on a chosen node. Closes the current browser; pages open there are lost. Without arguments: a fresh throwaway browser on the node with the most room.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "profile": { "type": "string", "description": "Name of a profile from browser_status." },
                    "node": { "type": "string", "description": "Destination node. An idle saved profile is moved here automatically; active profiles are not interrupted." },
                    "label": { "type": "string", "description": "Restrict placement to nodes with this label, such as cloud (see the labels of the nodes in browser_status). Never falls back outside it; node, if supplied, must have it." },
                    "group": { "type": "string", "description": "Old name of `label`." },
                    "ephemeral": { "type": "boolean", "description": "Create a dedicated short-lived node for this browser." },
                    "ephemeral_ttl_seconds": { "type": "integer", "minimum": 60, "maximum": 86400, "description": "Maximum lifetime of the dedicated node in seconds." }
                },
                "additionalProperties": false
            },
            "annotations": { "title": "Use another browser", "readOnlyHint": false }
        }),
    ];
    tools.push(json!({
        "name": "browser_shared_nodes",
        "description": "List temporary browsers registered through the Browser Daemon Node extension across the mesh. Only tabs explicitly shared by their owner appear. These are the user's real signed-in tabs. Use browser_shared_command to view or control them; browser_use still selects managed browsers.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        "annotations": {"title": "Shared browsers", "readOnlyHint": true}
    }));
    tools.push(json!({
        "name": "browser_shared_command",
        "description": "View or control a tab from browser_shared_nodes. Screenshot/snapshot work with view permission; other actions need the owner's separate control grant. Navigation ends sharing, including reload. Actions: screenshot, snapshot, click {x,y} or {selector}, type {text,selector?}, key {key}, scroll {x,y}, navigate {url}, back, forward, reload, focus, close. Chromium also supports cdp {method,params} for allowed Runtime/DOM/Input/Accessibility methods. No browser-wide CDP. Firefox's optional native companion provides trusted input and evaluate {script,args?} (a script body executed in the content context); without it, input uses DOM actions. Do not type credentials here: arguments and results are logged. Access ends on local stop, navigation, expiry or disconnect.",
        "inputSchema": {"type": "object", "properties": {
            "daemon": {"type": "string", "description": "Hosting daemon from browser_shared_nodes; omit for the local daemon."},
            "browser": {"type": "string", "description": "Temporary browser UUID."},
            "tab": {"type": "integer", "minimum": 0},
            "action": {"type": "string", "enum": ["screenshot", "snapshot", "click", "type", "key", "scroll", "navigate", "back", "forward", "reload", "focus", "close", "cdp", "evaluate"]},
            "params": {"type": "object"},
            "censor": {"type": "array", "items": {"type": "string", "enum": ["passwords", "cards", "otp", "secrets", "inputs", "emails"]}, "description": "screenshot only: black out these kinds of content (the user's server may force the defaults)."}
        }, "required": ["browser", "tab", "action"], "additionalProperties": false},
        "annotations": {"title": "Shared browser action", "readOnlyHint": false}
    }));
    tools.push(json!({
        "name": "browser_network_har",
        "description": "Record the network traffic of this browser (all its tabs) and save it as a HAR 1.2 file, e.g. to see which requests a page makes or why one fails. `start` begins recording, `stop` writes the file and returns its path and the number of requests, `status` says whether it records, `cancel` discards it. Credentials (Authorization and Cookie headers, tokens in query parameters, secret fields of JSON bodies) are replaced with [redacted]; text response bodies are kept up to 256 KiB each, posted bodies are not. Switching browsers (browser_use) loses a running recording.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["start", "stop", "status", "cancel"], "description": "What to do (default status)."}
        }, "additionalProperties": false},
        "annotations": {"title": "Record network traffic (HAR)", "readOnlyHint": false}
    }));
    tools.push(json!({
        "name": "browser_flight_recorder",
        "description": "Keep a rolling video of the last seconds of this browser, so that something that just went wrong can be shown to the user afterwards. `start` begins buffering (`seconds`: how much to keep, default 60), `stop` stops it, `status` says whether it runs, `save` stores the buffered seconds as a recording the user can watch in the dashboard and returns its id.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["start", "stop", "status", "save"], "description": "What to do (default status)."},
            "seconds": {"type": "integer", "minimum": 1, "description": "start only: how many seconds to keep (default 60)."}
        }, "additionalProperties": false},
        "annotations": {"title": "Flight recorder"}
    }));
    tools.push(json!({
        "name": "browser_set_topic",
        "description": "Label this conversation's browser sessions on the dashboard with what you are doing, so the user can tell agents apart. Call it when you start and when the task changes; the label sticks to connected browsers and is applied to the first one if none is open yet.",
        "inputSchema": {"type": "object", "properties": {
            "topic": {"type": "string", "description": "A few words, e.g. \"Check the invoice on the supplier portal\". Empty clears the label."}
        }, "required": ["topic"], "additionalProperties": false},
        "annotations": {"title": "Label the session"}
    }));
    tools.push(json!({
        "name": "browser_screenshot_censored",
        "description": "Take a screenshot of the page with passwords, card numbers, one-time codes, filled-in inputs and the secrets filled in this session blacked out. Prefer it to browser_take_screenshot whenever the page may show something private. Nothing is returned if the mask cannot be built.",
        "inputSchema": {"type": "object", "properties": {
            "fullPage": {"type": "boolean", "description": "The whole scrollable page instead of the viewport (not with target)."},
            "target": {"type": "string", "description": "Only this element: an element reference from the snapshot or a CSS selector."},
            "element": {"type": "string", "description": "Human-readable description of the target."},
            "type": {"type": "string", "enum": ["png", "jpeg"], "description": "Image format (default png)."},
            "censor": {"type": "array", "items": {"type": "string", "enum": ["passwords", "cards", "otp", "secrets", "inputs", "emails"]}, "description": "What to black out (default: passwords, cards, otp, secrets, inputs; emails is opt-in)."}
        }, "additionalProperties": false},
        "annotations": {"title": "Censored screenshot"}
    }));
    tools.push(json!({
        "name": "browser_live_link",
        "description": "The address of the live view of your current page, where the user can watch and take over. Use it instead of guessing node addresses.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        "annotations": {"title": "Live view link"}
    }));
    tools.push(json!({
        "name": "browser_recording",
        "description": "Record a video of your own browser session, to show the user afterwards. `start` begins, `stop` ends and returns the recording id, `status` says whether one is running.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["start", "stop", "status"], "description": "What to do (default status)."}
        }, "additionalProperties": false},
        "annotations": {"title": "Record the session"}
    }));
    if !secrets.is_empty() {
        tools.push(json!({
            "name": "browser_fill_secret",
            "description": format!(
                "Type a field of a vault (rbw) entry into a page field, e.g. a password into a login form, without the value ever reaching you: it is read from the vault here, typed into the page and masked as {} in all later tool results. Use this for passwords, tokens and TOTP codes instead of browser_type or browser_fill_form, never type a secret yourself. The page must be on a site the entry is saved for. Entries you may use: {}.",
                secrets::MASK,
                if allows(secrets, "*") { "any entry of the vault".to_string() } else { secrets.join(", ") }
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entry": { "type": "string", "description": "Name of the vault entry (one of the allowed ones)." },
                    "user": { "type": "string", "description": "Username of the entry, when several entries have that name." },
                    "field": { "type": "string", "description": "password (default), username, totp (the current code) or the name of a custom field." },
                    "target": { "type": "string", "description": "The field to type into: an exact element reference from the page snapshot, or a unique element selector." },
                    "element": { "type": "string", "description": "Human-readable description of that field." },
                    "submit": { "type": "boolean", "description": "Press Enter afterwards." }
                },
                "required": ["entry", "target"],
                "additionalProperties": false
            },
            "annotations": { "title": "Fill a vault secret", "readOnlyHint": false }
        }));
    }
    if !passkeys.is_empty() {
        tools.push(json!({
            "name": "browser_passkey",
            "description": format!(
                "Sign in with a passkey from the vault: call this before clicking a site's passkey (security key, WebAuthn) sign-in. For the next minutes every passkey request of this browser on the site is signed from the vault and answered in the page, also on pages the sign-in moves on to; nothing secret reaches you. Then click the sign-in button as usual and take a snapshot to see the result. Sites you may sign in to: {}.",
                if allows(passkeys, "*") { "any site with a passkey in the vault".to_string() } else { passkeys.join(", ") }
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "site": { "type": "string", "description": "The site to sign in to (e.g. microsoft.com); default: every allowed site." },
                    "minutes": { "type": "integer", "minimum": 0, "maximum": 30, "description": "How long to stay ready (default 5; 0 stops)." }
                },
                "additionalProperties": false
            },
            "annotations": { "title": "Passkey sign-in", "readOnlyHint": false }
        }));
    }
    if !exports.is_empty() {
        let sites = if allows(exports, "*") {
            "any site".to_string()
        } else {
            exports.join(", ")
        };
        let where_ = json!({
            "browser": { "type": "string", "description": "A shared browser (UUID from browser_shared_nodes) instead of your own browser's current page." },
            "tab": { "type": "integer", "minimum": 0, "description": "The shared browser's tab (it must be shared with control)." },
            "daemon": { "type": "string", "description": "The daemon hosting the shared browser, from browser_shared_nodes." }
        });
        let mut export_props = json!({
            "format": { "type": "string", "enum": ["cookies", "header", "json"], "description": "header (default): a `Cookie:` header line for `curl -H @FILE`; cookies: a Netscape cookie jar (curl 8.22 ignores the cookies of jar files it reads, other tools take them); json: cookies and the storage_keys asked for." },
            "urls": { "type": "array", "items": { "type": "string" }, "description": "Addresses to take the cookies for (default: the page's own); header uses the first." },
            "storage_keys": { "type": "array", "items": { "type": "string" }, "description": "Local or session storage keys to include (json format). The answer lists the page's storage keys." },
            "storage_filter": { "type": "string", "description": "List only the storage keys containing this (case-insensitive), e.g. token." },
            "minutes": { "type": "integer", "minimum": 1, "maximum": 120, "description": "When the file is deleted (default 15)." }
        });
        let mut fetch_props = json!({
            "url": { "type": "string", "description": "The address to request." },
            "method": { "type": "string", "description": "HTTP method (default GET)." },
            "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "More request headers." },
            "body": { "type": "string", "description": "Request body." },
            "bearer_storage_key": { "type": "string", "description": "Send `Authorization: Bearer` with the token in this local or session storage key of the page (its value, or its secret/access_token field when it is JSON)." }
        });
        for props in [&mut export_props, &mut fetch_props] {
            props
                .as_object_mut()
                .unwrap()
                .extend(where_.as_object().unwrap().clone());
        }
        tools.push(json!({
            "name": "browser_export_auth",
            "description": format!(
                "Hand the login of a browser page (its cookies, chosen storage values) to your other tools without seeing it: written to a private file that is deleted after a while, e.g. for `curl -H @FILE`. The answer gives the path and the cookie and storage names, never values; the values are masked in all later results. Never print, cat or copy the file. Prefer browser_fetch when one request is enough. The page must be on one of these sites: {sites}."
            ),
            "inputSchema": { "type": "object", "properties": export_props, "additionalProperties": false },
            "annotations": { "title": "Export a page's login", "readOnlyHint": false }
        }));
        tools.push(json!({
            "name": "browser_fetch",
            "description": format!(
                "Make an HTTP request with the login of a browser page (its cookies for the address, optionally a bearer token from its storage) without seeing it, e.g. to call a site's API as the signed-in user. Returns status, headers and body (text, first 64 KiB), with the login's values masked. The address must be on one of these sites: {sites}."
            ),
            "inputSchema": { "type": "object", "properties": fetch_props, "required": ["url"], "additionalProperties": false },
            "annotations": { "title": "Request with a page's login", "readOnlyHint": false }
        }));
    }
    if ask_human {
        tools.push(json!({
            "name": "browser_ask_human",
            "description": "Ask the user to do something in your browser that only a person may do: solve a CAPTCHA or other bot check, approve a sign-in on another device, type a password you may not fill. Sends them a link to the live view of your current page, where they can click and type in this very session. Say exactly what to do in `message`. Do not solve or bypass bot checks yourself. Afterwards leave the page alone and wait (browser_wait_for), then take a snapshot to see what changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "What the user should do, e.g. \"Solve the CAPTCHA, then tap Next\"." },
                    "title": { "type": "string", "description": "Short title of the notification." }
                },
                "required": ["message"],
                "additionalProperties": false
            },
            "annotations": { "title": "Ask the user for help", "readOnlyHint": false }
        }));
    }
    if cards {
        tools.push(json!({
            "name": "browser_fill_card",
            "description": "Fill an authorized vault card field into an authorized payment iframe without revealing its value. Never submits the payment.",
            "inputSchema": {"type":"object", "properties": {
                "entry": {"type":"string", "description": "Name of the vault card entry."},
                "field": {"type":"string", "enum":["number", "exp_month", "exp_year", "exp_date", "cvv"], "description":"Use exp_date for a combined expiration field (MM/YY)."},
                "target": {"type":"string", "description": "The field to fill: an element reference from the page snapshot or a unique selector."},
                "element": {"type":"string", "description": "Human-readable description of that field."}
            }, "required":["entry", "field", "target"], "additionalProperties":false},
            "annotations": {"title":"Fill a card field"}
        }));
    }
    if share {
        tools.push(json!({
            "name": "browser_share",
            "description": "Make a PUBLIC link to your browser session or to a recording: anyone who has the link can see it, without signing in. View-only unless `control` is true. The link expires (default 1 hour, at most 7 days). Only use it when the user asked you to share something.",
            "inputSchema": {"type": "object", "properties": {
                "recording_id": {"type": "string", "description": "Share this recording instead of your session."},
                "control": {"type": "boolean", "description": "Session only: let the viewer click and type too (Chromium). Default false."},
                "expires": {"type": "string", "description": "How long the link works: 30m, 1h (default), 2d ... at most 7d."}
            }, "additionalProperties": false},
            "annotations": {"title": "Make a public link"}
        }));
        tools.push(json!({
            "name": "browser_unshare",
            "description": "End a public link made with browser_share, so it stops working at once.",
            "inputSchema": {"type": "object", "properties": {
                "share_id": {"type": "string", "description": "The share id browser_share returned."}
            }, "required": ["share_id"], "additionalProperties": false},
            "annotations": {"title": "End a public link"}
        }));
    }
    apply_hints(&mut tools);
    tools
}

/// Playwright code giving the page's CDP target id and address: no secret in
/// it. (Its sandbox has no `URL`, so the host is taken apart here.)
const PAGE_INFO: &str = "async (page) => { const s = await page.context().newCDPSession(page); \
const { targetInfo } = await s.send('Target.getTargetInfo'); await s.detach(); \
return 'BDPAGE:' + targetInfo.targetId + ':' + page.url() + ':BDEND'; }";

/// (target id, address, host) from the answer to PAGE_INFO.
fn parse_page_info(result: &Value) -> Option<(String, String, String)> {
    let text: String = result["content"]
        .as_array()?
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect();
    let start = text.find("BDPAGE:")? + "BDPAGE:".len();
    let rest = &text[start..];
    let (id, rest) = rest.split_once(':')?;
    let (address, _) = rest.split_once(":BDEND")?;
    let parsed = url::Url::parse(address).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    (!id.is_empty() && !host.is_empty()).then(|| (id.to_string(), address.to_string(), host))
}

/// A Netscape cookie jar (curl's `-b FILE`), HttpOnly cookies marked as curl does.
fn cookie_jar(cookies: &[Value]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let domain = c["domain"].as_str().unwrap_or_default();
        let line = format!(
            "{}{domain}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            if c["http_only"].as_bool() == Some(true) {
                "#HttpOnly_"
            } else {
                ""
            },
            if domain.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            },
            c["path"].as_str().unwrap_or("/"),
            if c["secure"].as_bool() == Some(true) {
                "TRUE"
            } else {
                "FALSE"
            },
            c["expires"].as_f64().map_or(0, |e| e as i64),
            c["name"].as_str().unwrap_or_default(),
            c["value"].as_str().unwrap_or_default(),
        );
        out.push_str(&line);
    }
    out
}

/// The `Cookie` header the browser would send to `target`.
fn cookie_header(cookies: &[Value], target: &str) -> Result<String, String> {
    let url = url::Url::parse(target).map_err(|_| format!("Not an address: {target}"))?;
    Ok(cookies
        .iter()
        .filter(|c| {
            crate::auth_export::sent_to(
                c["domain"].as_str().unwrap_or_default(),
                c["path"].as_str().unwrap_or("/"),
                c["secure"].as_bool().unwrap_or(false),
                &url,
            )
        })
        .map(|c| {
            format!(
                "{}={}",
                c["name"].as_str().unwrap_or_default(),
                c["value"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; "))
}

/// The token in a storage value: the value itself, or the token field of a JSON
/// entry (MSAL keeps `secret`, others `access_token`).
fn bearer_token(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            ["secret", "access_token", "accessToken", "token"]
                .iter()
                .find_map(|k| v[*k].as_str().map(str::to_string))
        })
        .unwrap_or_else(|| raw.trim().trim_matches('"').to_string())
}

/// The `sessionId` of the one target `page_id` in a /json/list answer.
fn unique_session(list: &Value, page_id: &str) -> Option<String> {
    let mut hits = list
        .as_array()?
        .iter()
        .filter(|t| t["id"].as_str() == Some(page_id));
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    first["sessionId"].as_str().map(str::to_string)
}

impl State {
    async fn send(&self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        let mut out = self.stdout.lock().await;
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.flush().await;
    }

    async fn handle(self: &Arc<Self>, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = msg.get("id").cloned() else {
            return; // notifications need no answer
        };
        if method.is_empty() {
            return; // an answer to something we never asked
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Rpc = match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18")
                    .to_string();
                *self.version.lock().await = version.clone();
                *self.client_name.lock().await = params["clientInfo"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "browser-daemon", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.clone().tools_list().await),
            "tools/call" => self.tools_call(params).await,
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        self.send(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
        .await;
    }

    async fn tools_list(self: Arc<Self>) -> Value {
        let browser = match self.child_or_default().await {
            Ok(child) => child.tools.lock().unwrap().clone(),
            Err(e) => {
                // The page tools are listed anyway, as the browser tools last listed
                // them: a call starts a browser. Meanwhile they are retried, and the
                // client is told once the list changes.
                eprintln!(
                    "browser-daemon mcp: browser tools not up yet, listing the cached ones: {e}"
                );
                self.clone().retry_browser_tools();
                load_tool_cache()
            }
        };
        let tools = self.all_tools(browser);
        *self.listed.lock().unwrap() = Some(tool_names(&tools));
        json!({ "tools": tools })
    }

    fn all_tools(&self, browser: Vec<Value>) -> Vec<Value> {
        let mut tools = own_tools_with(
            &self.allow_secrets,
            &self.allow_passkeys,
            self.ask_human_cmd.is_some(),
            &self.allow_exports,
            !self.allow_cards.is_empty(),
            self.allow_share,
        );
        tools.extend(browser);
        tools
    }

    /// Tells the client to list the tools again when they differ from what it was
    /// given (`listChanged` is promised in initialize).
    async fn notify_tools_changed(&self) {
        let browser = match self.child.lock().await.as_ref() {
            Some(child) => child.tools.lock().unwrap().clone(),
            None => load_tool_cache(),
        };
        let names = tool_names(&self.all_tools(browser));
        {
            let mut listed = self.listed.lock().unwrap();
            match listed.as_ref() {
                Some(old) if *old != names => *listed = Some(names),
                _ => return,
            }
        }
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }))
            .await;
    }

    /// Starts the browser tools again in the background, with growing pauses, until
    /// they are up; then the client hears that the tool list changed.
    fn retry_browser_tools(self: Arc<Self>) {
        if self.retrying.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            for secs in [5, 10, 20, 30, 60, 60, 120, 120, 300, 300] {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                match self.child_or_default().await {
                    Ok(_) => {
                        self.notify_tools_changed().await;
                        break;
                    }
                    Err(e) => eprintln!("browser-daemon mcp: browser tools still not up: {e}"),
                }
            }
            self.retrying.store(false, Ordering::SeqCst);
        });
    }

    /// Masks every secret filled so far in a message.
    fn mask(&self, text: &str) -> String {
        secrets::redact_str(text, &self.filled.lock().unwrap())
    }

    async fn tools_call(&self, params: Value) -> Rpc {
        let started = std::time::SystemTime::now() - Duration::from_secs(1);
        let mut result = self.tools_call_raw(params).await;
        // Whatever comes back, results and errors alike, goes through the mask.
        let filled = self.filled.lock().unwrap().clone();
        if !filled.is_empty() {
            // Playwright MCP links its snapshots as files, which show a field's
            // value even when the screen shows dots: mask them before the agent can read them.
            let dir = self.out_dir.clone();
            let secrets = filled.clone();
            let _ =
                tokio::task::spawn_blocking(move || secrets::redact_files(&dir, started, &secrets))
                    .await;
        }
        match &mut result {
            Ok(v) => secrets::redact(v, &filled),
            Err((_, message)) => *message = secrets::redact_str(message, &filled),
        }
        result
    }

    async fn tools_call_raw(&self, params: Value) -> Rpc {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        match name {
            "browser_shared_nodes" => Ok(match self.shared_nodes().await {
                Ok(value) => text_result(value.to_string(), false),
                Err(error) => text_result(error, true),
            }),
            "browser_shared_command" => Ok(match self.shared_command(&args).await {
                Ok(value) => {
                    if let Some(data) = value["result"]["data"]
                        .as_str()
                        .filter(|_| args["action"] == "screenshot")
                    {
                        let mime = if value["result"]["format"] == "png" {
                            "image/png"
                        } else {
                            "image/jpeg"
                        };
                        json!({"content": [{"type": "image", "mimeType": mime, "data": data}]})
                    } else {
                        text_result(value.to_string(), value["error"].is_string())
                    }
                }
                Err(error) => text_result(error, true),
            }),
            "browser_status" => Ok(match self.status().await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_profile_tabs" => Ok(match self.current_profile_tabs().await {
                Ok(tabs) => text_result(tabs.to_string(), false),
                Err(error) => text_result(error, true),
            }),
            "browser_use" => Ok(match self.use_browser(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_fill_secret" if !self.allow_secrets.is_empty() => {
                Ok(match self.fill_secret(&args, false).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            "browser_fill_card" if !self.allow_cards.is_empty() => {
                Ok(match self.fill_secret(&args, true).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            "browser_passkey" if !self.allow_passkeys.is_empty() => {
                Ok(match self.passkey(&args).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            "browser_export_auth" if !self.allow_exports.is_empty() => {
                Ok(match self.export_auth(&args).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            "browser_fetch" if !self.allow_exports.is_empty() => {
                Ok(match self.fetch(&args).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            "browser_network_har" => Ok(match self.network_har(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_flight_recorder" => Ok(match self.flight_recorder(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_screenshot_censored" => Ok(self.censored_answer(&args).await),
            "browser_take_screenshot" if self.censor_screenshots => {
                Ok(self.censored_answer(&args).await)
            }
            "browser_set_topic" => Ok(match self.set_topic(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_live_link" => Ok(match self.live_link().await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_recording" => Ok(match self.recording(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_share" if self.allow_share => Ok(match self.share(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_unshare" if self.allow_share => Ok(match self.unshare(&args).await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
            }),
            "browser_ask_human" if self.ask_human_cmd.is_some() => {
                Ok(match self.ask_human(&args).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            _ => {
                let is_browser_tabs = name == "browser_tabs";
                let may_wall = may_hit_wall(name);
                let child = match self.child_or_default().await {
                    Ok(child) => child,
                    Err(e) => return Ok(text_result(e, true)),
                };
                self.notify_tools_changed().await;
                let first =
                    child.target.profile.is_some() && !child.used.swap(true, Ordering::SeqCst);
                let mut result = match child
                    .request("tools/call", params, Duration::from_secs(600))
                    .await
                {
                    Ok(result) => result,
                    // Unknown tool and bad arguments are protocol errors; a browser that
                    // died or timed out is a tool failure the agent can act on.
                    Err((code, e)) if code == -32601 || code == -32602 => return Err((code, e)),
                    Err((_, e)) => {
                        return Ok(text_result(
                            format!("The browser tool did not finish: {e}. Try again, or switch browsers with browser_use."),
                            true,
                        ))
                    }
                };
                self.apply_pending_topic(&child).await;
                // Both agents may select before either starts the profile. Check
                // once more after connecting; this never blocks the operation.
                let after = if first {
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        self.shared_profile_warning(&child.target, 2),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten()
                } else {
                    None
                };
                if let Some(warning) = after {
                    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
                        content.push(json!({ "type": "text", "text": warning }));
                    }
                }
                if may_wall && !result["isError"].as_bool().unwrap_or(false) {
                    self.watch_challenge(&child, &mut result).await;
                }
                if is_browser_tabs && !result["isError"].as_bool().unwrap_or(false) {
                    if let Ok(Ok(tabs)) = tokio::time::timeout(
                        Duration::from_secs(2),
                        self.profile_tabs(&child.target),
                    )
                    .await
                    {
                        if let Some(content) =
                            result.get_mut("content").and_then(Value::as_array_mut)
                        {
                            content.push(json!({ "type": "text", "text": format!("Profile tabs and owners (browser IDs, not Playwright indexes): {tabs}") }));
                        }
                    }
                }
                Ok(result)
            }
        }
    }

    // --------------------------------------------------------------- secrets

    async fn fill_secret(&self, args: &Value, card: bool) -> Result<String, String> {
        let arg = |k: &str| args[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        let entry = arg("entry").ok_or("Name the vault entry (`entry`)")?;
        let target = arg("target").ok_or("Name the field to type into (`target`)")?;
        let field = arg("field").unwrap_or("password");
        if !card && !allows(&self.allow_secrets, entry) {
            return Err(format!(
                "The entry '{entry}' is not open to agents here. Allowed: {}.",
                allowed(&self.allow_secrets)
            ));
        }
        let child = self.child_or_default().await?;
        let call = |name: &str, arguments: Value| {
            child.request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                Duration::from_secs(60),
            )
        };
        let info = call("browser_run_code_unsafe", json!({ "code": PAGE_INFO }))
            .await
            .map_err(|(_, e)| format!("Could not find the page: {e}"))?;
        let (page_id, address, host) =
            parse_page_info(&info).ok_or("Could not find the page (no page open?)")?;

        let card_origins: Option<[String; 2]> = if card {
            self.allow_cards
                .iter()
                .find_map(|rule| {
                    let parts: Vec<_> = rule.split('|').collect();
                    if parts.len() == 3
                        && parts[0] == entry
                        && secrets::exact_https_origin(&address, parts[1])
                        && secrets::exact_https_origin(parts[2], parts[2])
                    {
                        Some([parts[1].to_string(), parts[2].to_string()])
                    } else {
                        None
                    }
                })
                .ok_or("Card and merchant are not authorized")?
                .into()
        } else {
            None
        };
        let secret =
            { crate::rbw_api::lookup(&self.rbw_api, entry, arg("user"), field, card).await? };
        // Masked from here on, whatever fails later.
        {
            let mut filled = self.filled.lock().unwrap();
            filled.push(secret.value.clone());
            if let (true, "exp_date", Some((month, year))) =
                (card, field, secret.value.split_once('/'))
            {
                filled.extend([month.to_string(), year.to_string()]);
            }
            if let Some(ref user) = secret.username {
                // HTTP Basic auth carries both, base64-encoded.
                filled.push(format!("{user}:{}", secret.value));
            }
        }
        if !card && !secrets::url_matches(&address, &secret.uris) {
            return Err(format!(
                "The entry '{entry}' is not saved for {host}, so it is not typed there."
            ));
        }

        // Focus the field the Playwright way (references are Playwright's), then type.
        let element = arg("element").map_or_else(|| format!("{field} field"), str::to_string);
        let click = call(
            "browser_click",
            json!({ "target": target, "element": element }),
        )
        .await
        .map_err(|(_, e)| format!("Could not focus the field: {e}"))?;
        if click["isError"].as_bool() == Some(true) {
            let mut click = click;
            secrets::redact(&mut click, &self.filled.lock().unwrap());
            let why: String = click["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect();
            return Err(format!("Could not focus the field: {}", why.trim()));
        }
        let submit = !card && args["submit"].as_bool().unwrap_or(false);
        let session_id = self.page_session(&child.target.url, &page_id).await;
        let mut req = self
            .http
            .post(format!(
                "{}/api/pages/{page_id}/insert-text",
                child.target.url.trim_end_matches('/')
            ))
            .json(&json!({
                "text": secret.value,
                "uris": secret.uris,
                "card_origins": card_origins,
                "enter": submit,
                "session_id": session_id,
            }));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("{}: {e}", child.target.node))?;
        if !res.status().is_success() {
            let status = res.status();
            let body: Value = res.json().await.unwrap_or_default();
            return Err(self.mask(&format!(
                "{} refused the input: {}",
                child.target.node,
                body["error"].as_str().unwrap_or(status.as_str())
            )));
        }
        Ok(format!(
            "Typed the {field} of '{entry}' into {element} on {host}{}. The value is not shown; take a browser_snapshot to see the page.",
            if submit { " and pressed Enter" } else { "" }
        ))
    }

    /// The page the agent is on: (child, target id, address, host).
    async fn current_page(&self) -> Result<(Arc<Child>, String, String, String), String> {
        let child = self.child_or_default().await?;
        let info = child
            .request(
                "tools/call",
                json!({ "name": "browser_run_code_unsafe", "arguments": { "code": PAGE_INFO } }),
                Duration::from_secs(60),
            )
            .await
            .map_err(|(_, e)| format!("Could not find the page: {e}"))?;
        let (page_id, address, host) =
            parse_page_info(&info).ok_or("Could not find the page (no page open?)")?;
        Ok((child, page_id, address, host))
    }

    // -------------------------------------------------------------- passkeys

    async fn passkey(&self, args: &Value) -> Result<String, String> {
        let site = args["site"]
            .as_str()
            .map(|s| s.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        let minutes = args["minutes"].as_u64().unwrap_or(5).min(30);
        let sites: Vec<String> = match site {
            Some(site) if crate::agent_passkeys::site_allowed(&self.allow_passkeys, &site) => {
                vec![site]
            }
            Some(site) => {
                return Err(format!(
                    "Passkey sign-ins on {site} are not open to agents here. Allowed: {}.",
                    allowed(&self.allow_passkeys)
                ))
            }
            None => self.allow_passkeys.clone(),
        };
        let (child, page_id, _, _) = self.current_page().await?;
        let session = self
            .page_session(&child.target.url, &page_id)
            .await
            .ok_or("Could not tell which session the page is in")?;
        let mut req = self
            .http
            .post(format!(
                "{}/api/sessions/{session}/agent-passkey",
                child.target.url.trim_end_matches('/')
            ))
            .json(&json!({
                "sites": if minutes == 0 { Vec::new() } else { sites.clone() },
                "seconds": minutes * 60,
            }));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("{}: {e}", child.target.node))?;
        if !res.status().is_success() {
            let status = res.status();
            let body: Value = res.json().await.unwrap_or_default();
            return Err(format!(
                "{} refused: {}",
                child.target.node,
                body["error"].as_str().unwrap_or(status.as_str())
            ));
        }
        if minutes == 0 {
            return Ok("Passkey sign-ins are no longer answered from the vault.".into());
        }
        Ok(format!(
            "For the next {minutes} minutes, passkey sign-ins of this browser on {} are signed from the vault (the user may have to unlock it on their phone first). Click the site's passkey sign-in now, then take a snapshot to see the result.",
            if sites.iter().any(|s| s == "*") { "any site".to_string() } else { sites.join(", ") }
        ))
    }

    // ------------------------------------------------------------ logins

    /// Masks values that leave with a login from here on: its cookies and the
    /// storage values used. Short values (`1`, `dark`) are no secrets and would
    /// garble every later answer, so only those of 8 characters and more.
    fn remember_login(&self, login: &Value, storage_keys: &[&str]) {
        let mut filled = self.filled.lock().unwrap();
        let mut add = |v: &Value| {
            if let Some(v) = v.as_str().filter(|v| v.len() >= 8) {
                if !filled.iter().any(|f| f == v) {
                    filled.push(v.to_string());
                }
            }
        };
        for c in login["cookies"].as_array().into_iter().flatten() {
            add(&c["value"]);
        }
        for key in storage_keys {
            for kind in ["local_storage", "session_storage"] {
                let v = &login[kind][*key];
                add(v);
                // A JSON value (an MSAL token entry) also gets its token masked.
                if let Some(inner) = v
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                {
                    for k in ["secret", "access_token", "accessToken", "token"] {
                        add(&inner[k]);
                    }
                }
            }
        }
    }

    /// The login of the page in question: the agent's current page, or a shared
    /// tab (`browser`, `tab`, `daemon`). Its address must be on an allowed site;
    /// so must every one of `urls`. Its cookies, and the values of `storage_keys`,
    /// are masked from here on; `storage` reads the storage at all.
    async fn page_login(
        &self,
        args: &Value,
        urls: &[String],
        storage: bool,
        storage_keys: &[&str],
    ) -> Result<Value, String> {
        for u in urls {
            let host = url::Url::parse(u)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                .ok_or_else(|| format!("Not an address: {u}"))?;
            if !crate::agent_passkeys::site_allowed(&self.allow_exports, &host) {
                return Err(format!(
                    "{host} is not open to agents here. Allowed: {}.",
                    allowed(&self.allow_exports)
                ));
            }
        }
        let body = json!({ "urls": urls, "storage": storage });
        let req = match (args["browser"].as_str(), args["tab"].as_u64()) {
            (Some(browser), Some(tab)) => {
                let browser =
                    uuid::Uuid::parse_str(browser).map_err(|_| "Expected a browser UUID")?;
                let prefix = match args["daemon"].as_str().filter(|s| !s.is_empty()) {
                    Some(daemon) => {
                        let overview = self.overview().await?;
                        if overview.local_node.name == daemon {
                            "/api".to_string()
                        } else if overview.peers.iter().any(|p| p.name == daemon) {
                            format!("/api/peers/{}", url_component(daemon))
                        } else {
                            return Err("Unknown hosting daemon".into());
                        }
                    }
                    None => "/api".to_string(),
                };
                self.http
                    .post(format!(
                        "{}{prefix}/extension/nodes/{browser}/tabs/{tab}/auth",
                        self.base
                    ))
                    .json(&body)
            }
            (None, None) => {
                let (child, page_id, _, _) = self.current_page().await?;
                let session = self.page_session(&child.target.url, &page_id).await;
                let mut body = body;
                body["session_id"] = json!(session);
                self.http
                    .post(format!(
                        "{}/api/pages/{}/auth",
                        child.target.url.trim_end_matches('/'),
                        url_component(&page_id)
                    ))
                    .json(&body)
            }
            _ => {
                return Err("Name both `browser` and `tab` for a shared browser, or neither".into())
            }
        };
        let req = match self.secret {
            Some(ref s) => req.header("X-Browser-Cluster-Secret", s),
            None => req,
        };
        let res = req
            .send()
            .await
            .map_err(|e| format!("Could not reach the node: {e}"))?;
        let status = res.status();
        let login: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "The node refused: {}",
                login["error"].as_str().unwrap_or(status.as_str())
            ));
        }
        self.remember_login(&login, storage_keys);
        let page = login["url"].as_str().unwrap_or_default();
        let host = url::Url::parse(page)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        if !crate::agent_passkeys::site_allowed(&self.allow_exports, &host) {
            return Err(format!(
                "The page is on {host}, which is not open to agents here. Allowed: {}.",
                allowed(&self.allow_exports)
            ));
        }
        Ok(login)
    }

    async fn export_auth(&self, args: &Value) -> Result<String, String> {
        let format = args["format"].as_str().unwrap_or("header");
        if !matches!(format, "cookies" | "header" | "json") {
            return Err("format is cookies, header or json".into());
        }
        let urls: Vec<String> = args["urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| u.as_str().map(str::to_string))
            .collect();
        let keys: Vec<String> = args["storage_keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
        let minutes = args["minutes"].as_u64().unwrap_or(15).clamp(1, 120);
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let login = self.page_login(args, &urls, true, &key_refs).await?;
        let cookies = login["cookies"].as_array().cloned().unwrap_or_default();
        let target = urls
            .first()
            .cloned()
            .unwrap_or_else(|| login["url"].as_str().unwrap_or_default().to_string());
        let (content, ext) = match format {
            "cookies" => (cookie_jar(&cookies), "txt"),
            "header" => (
                format!("Cookie: {}\n", cookie_header(&cookies, &target)?),
                "txt",
            ),
            _ => {
                let mut storage = serde_json::Map::new();
                for k in &keys {
                    let v = login["local_storage"]
                        .get(k)
                        .or_else(|| login["session_storage"].get(k));
                    storage.insert(
                        k.clone(),
                        v.cloned().ok_or_else(|| format!("No storage key {k}"))?,
                    );
                }
                (
                    serde_json::to_string_pretty(
                        &json!({ "url": login["url"], "cookies": cookies, "storage": storage }),
                    )
                    .unwrap_or_default(),
                    "json",
                )
            }
        };
        let path = self.write_private(&content, ext, minutes)?;
        let names: Vec<String> = cookies
            .iter()
            .map(|c| {
                format!(
                    "{} ({})",
                    c["name"].as_str().unwrap_or("?"),
                    c["domain"].as_str().unwrap_or("?")
                )
            })
            .collect();
        let mut storage_keys: Vec<&str> = ["local_storage", "session_storage"]
            .iter()
            .flat_map(|k| {
                login[*k]
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.keys().map(String::as_str))
            })
            .collect();
        storage_keys.sort_unstable();
        storage_keys.dedup();
        let all = storage_keys.len();
        if let Some(filter) = args["storage_filter"].as_str().filter(|f| !f.is_empty()) {
            let filter = filter.to_lowercase();
            storage_keys.retain(|k| k.to_lowercase().contains(&filter));
        }
        Ok(format!(
            "Wrote the login of {} to {} ({format}; deleted in {minutes} minutes). Never print, cat or copy it.\nCookies ({}): {}\nStorage keys of the page{}: {}\nValues are not shown and are masked in all later results.",
            login["url"].as_str().unwrap_or("the page"),
            path.display(),
            names.len(),
            if names.is_empty() { "none".to_string() } else { names.join(", ") },
            if storage_keys.len() < all { format!(" ({} of {all} match the filter)", storage_keys.len()) } else { String::new() },
            if storage_keys.is_empty() { "none".to_string() } else { storage_keys.join(", ") },
        ))
    }

    /// Writes `content` to a new file only this user can read, in the runtime
    /// directory (memory, not disk, where there is one), removed after `minutes`.
    fn write_private(&self, content: &str, ext: &str, minutes: u64) -> Result<PathBuf, String> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("browser-daemon-mcp");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .map_err(|e| format!("Could not create {}: {e}", base.display()))?;
        let path = base.join(format!("login-{}.{ext}", uuid::Uuid::new_v4().simple()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        self.exports.lock().unwrap().push(path.clone());
        let gone = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
            let _ = tokio::fs::remove_file(gone).await;
        });
        Ok(path)
    }

    async fn fetch(&self, args: &Value) -> Result<String, String> {
        let target = args["url"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("Name the address (`url`)")?;
        let parsed = url::Url::parse(target).map_err(|_| format!("Not an address: {target}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("Only http and https".into());
        }
        let method = reqwest::Method::from_bytes(
            args["method"]
                .as_str()
                .unwrap_or("GET")
                .to_ascii_uppercase()
                .as_bytes(),
        )
        .map_err(|_| "Bad method")?;
        let bearer_key = args["bearer_storage_key"]
            .as_str()
            .filter(|s| !s.is_empty());
        let login = self
            .page_login(
                args,
                &[target.to_string()],
                bearer_key.is_some(),
                &bearer_key.into_iter().collect::<Vec<_>>(),
            )
            .await?;
        let cookies = login["cookies"].as_array().cloned().unwrap_or_default();
        let mut req = self
            .http
            .request(method, parsed.clone())
            .timeout(Duration::from_secs(60));
        for (k, v) in args["headers"].as_object().into_iter().flatten() {
            if let Some(v) = v.as_str() {
                req = req.header(k.as_str(), v);
            }
        }
        let header = cookie_header(&cookies, target)?;
        if !header.is_empty() {
            req = req.header(reqwest::header::COOKIE, header);
        }
        if let Some(key) = bearer_key {
            let raw = login["local_storage"]
                .get(key)
                .or_else(|| login["session_storage"].get(key))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("The page has no storage key {key}"))?;
            req = req.bearer_auth(bearer_token(raw));
        }
        if let Some(body) = args["body"].as_str() {
            req = req.body(body.to_string());
        }
        let res = req
            .send()
            .await
            .map_err(|e| self.mask(&format!("Request failed: {e}")))?;
        let status = res.status();
        let final_url = res.url().to_string();
        let headers: Vec<String> = res
            .headers()
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "set-cookie" | "content-security-policy" | "report-to" | "nel"
                )
            })
            .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("(binary)")))
            .collect();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| format!("Reading the answer failed: {e}"))?;
        const LIMIT: usize = 64 * 1024;
        let body = match std::str::from_utf8(&bytes[..bytes.len().min(LIMIT)]) {
            Ok(text) => text.to_string(),
            Err(e) if e.valid_up_to() > 0 => {
                String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
            }
            Err(_) => format!("({} bytes of binary data)", bytes.len()),
        };
        Ok(self.mask(&format!(
            "HTTP {status} from {final_url}\n{}\n\n{body}{}",
            headers.join("\n"),
            if bytes.len() > LIMIT {
                format!("\n… ({} bytes in all)", bytes.len())
            } else {
                String::new()
            }
        )))
    }

    // ------------------------------------------------------------ ask a human

    async fn ask_human(&self, args: &Value) -> Result<String, String> {
        let cmd = self
            .ask_human_cmd
            .as_ref()
            .ok_or("No way to reach the user")?;
        let message = args["message"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("Say what the user should do (`message`)")?;
        let title = args["title"].as_str().map(str::trim).unwrap_or_default();
        let (child, page_id, _, host) = self.current_page().await?;
        let node_url = child.target.url.trim_end_matches('/');
        let link = handoff_url(node_url, &page_id);
        let agent = self.client_name.lock().await.clone();
        let run = Command::new(cmd)
            .arg(self.mask(message))
            .env("BD_HANDOFF_URL", &link)
            .env("BD_PAGE", &page_id)
            .env("BD_NODE", &child.target.node)
            .env("BD_NODE_URL", node_url)
            .env("BD_TITLE", self.mask(title))
            .env("BD_AGENT", agent)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .map_err(|_| "Asking the user took too long".to_string())?
            .map_err(|e| format!("Could not ask the user: {e}"))?;
        let said = |b: &[u8]| self.mask(String::from_utf8_lossy(b).trim());
        if !out.status.success() {
            let err = said(&out.stderr);
            return Err(format!(
                "Could not ask the user: {}",
                err.lines().last().unwrap_or("the command failed")
            ));
        }
        let note = said(&out.stdout);
        Ok(format!(
            "Asked the user to help on {host} ({link}){}. Leave the page alone until they are done: wait (browser_wait_for), then take a snapshot.",
            if note.is_empty() { String::new() } else { format!(": {note}") }
        ))
    }

    /// The session the node shows `page_id` in, so the node types only into
    /// that session's page. None when the node does not say or says it twice.
    async fn page_session(&self, base: &str, page_id: &str) -> Option<String> {
        let mut req = self
            .http
            .get(format!("{}/json/list", base.trim_end_matches('/')));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let list: Value = req.send().await.ok()?.json().await.ok()?;
        unique_session(&list, page_id)
    }

    // ------------------------------------------------------------------ mesh

    async fn extension_request(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut req = if let Some(body) = body {
            self.http.post(format!("{}{path}", self.base)).json(&body)
        } else {
            self.http.get(format!("{}{path}", self.base))
        };
        if let Some(ref secret) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", secret);
        }
        let response = req
            .send()
            .await
            .map_err(|_| "Could not reach the hosting daemon".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "Shared browser request: HTTP {}",
                response.status()
            ));
        }
        response
            .json()
            .await
            .map_err(|_| "Invalid shared browser response".to_string())
    }

    async fn shared_nodes(&self) -> Result<Value, String> {
        let overview = self.overview().await?;
        let mut daemons = vec![(overview.local_node.name.clone(), "/api".to_string())];
        daemons.extend(
            overview
                .peers
                .iter()
                .filter(|p| p.status == "online")
                .map(|p| {
                    (
                        p.name.clone(),
                        format!("/api/peers/{}", url_component(&p.name)),
                    )
                }),
        );
        let reports = futures_util::future::join_all(daemons.into_iter().map(
            |(daemon, prefix)| async move {
                match self
                    .extension_request(&format!("{prefix}/extension/nodes"), None)
                    .await
                {
                    Ok(value) => json!({"daemon": daemon, "nodes": value["nodes"]}),
                    Err(error) => json!({"daemon": daemon, "error": error}),
                }
            },
        ))
        .await;
        Ok(json!({"daemons": reports}))
    }

    async fn shared_command(&self, args: &Value) -> Result<Value, String> {
        let browser = args["browser"]
            .as_str()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .ok_or("Expected a browser UUID")?;
        let tab = args["tab"]
            .as_u64()
            .filter(|n| *n <= u32::MAX as u64)
            .ok_or("Expected a tab ID")?;
        let action = args["action"].as_str().ok_or("Expected an action")?;
        let prefix = match args["daemon"].as_str().filter(|s| !s.is_empty()) {
            Some(daemon) => {
                let overview = self.overview().await?;
                if overview.local_node.name == daemon {
                    "/api".to_string()
                } else if overview.peers.iter().any(|p| p.name == daemon) {
                    format!("/api/peers/{}", url_component(daemon))
                } else {
                    return Err("Unknown hosting daemon".into());
                }
            }
            None => "/api".to_string(),
        };
        let mut params = args.get("params").cloned().unwrap_or(json!({}));
        let mut censored = false;
        if action == "screenshot" {
            let given = args
                .get("censor")
                .filter(|c| !c.is_null())
                .or(params.get("censor"));
            let filled = self.filled.lock().unwrap().clone();
            if let Some((cats, secrets)) = shared_censor(given, self.censor_screenshots, &filled)? {
                params["censor"] = json!(cats);
                if let Some(secrets) = secrets {
                    params["secrets"] = json!(secrets);
                }
                censored = true;
            }
        }
        let reply = self
            .extension_request(
                &format!("{prefix}/extension/nodes/{browser}/command"),
                Some(json!({ "tab": tab, "action": action, "params": params })),
            )
            .await?;
        if censored
            && reply["error"].is_null()
            && reply["censored"] != true
            && reply["result"]["censored"] != true
        {
            return Err("The browser extension did not censor the screenshot (it is too old): the image was discarded. Ask the user to update the Browser Daemon Node extension.".into());
        }
        Ok(reply)
    }

    async fn overview(&self) -> Result<MeshOverview, String> {
        let mut req = self.http.get(format!("{}/api/mesh/overview", self.base));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("{}: {e}", self.base))?;
        if !res.status().is_success() {
            return Err(format!("{}: HTTP {}", self.base, res.status()));
        }
        res.json()
            .await
            .map_err(|e| format!("Unexpected answer: {e}"))
    }

    /// Read the hosting node directly: the mesh overview can still have its
    /// cached, pre-connection snapshot when two clients attach concurrently.
    async fn shared_profile_warning(
        &self,
        target: &Target,
        min_clients: usize,
    ) -> Result<Option<String>, reqwest::Error> {
        let Some(profile) = target.profile.as_ref() else {
            return Ok(None);
        };
        let mut req = self
            .http
            .get(format!("{}/api/sessions", target.url.trim_end_matches('/')));
        if let Some(secret) = &self.secret {
            req = req.header("X-Browser-Cluster-Secret", secret);
        }
        let sessions = req
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<crate::state::SessionSummary>>()
            .await?;
        Ok(sessions
            .iter()
            .find(|session| {
                session.profile.as_ref() == Some(profile) && session.clients.len() >= min_clients
            })
            .map(session_warning))
    }

    async fn current_profile_tabs(&self) -> Result<Value, String> {
        let target = self
            .child
            .lock()
            .await
            .as_ref()
            .map(|child| child.target.clone())
            .ok_or("Select a saved profile with browser_use first")?;
        self.profile_tabs(&target).await
    }

    async fn profile_tabs(&self, target: &Target) -> Result<Value, String> {
        let profile = target
            .profile
            .as_ref()
            .ok_or("Select a saved profile with browser_use first")?;
        if !allows(&self.allow, profile) {
            return Err("This profile is not open to agents here".into());
        }
        let mut req = self
            .http
            .get(format!("{}/json/list", target.url.trim_end_matches('/')));
        if let Some(secret) = &self.secret {
            req = req.header("X-Browser-Cluster-Secret", secret);
        }
        let tabs = req
            .send()
            .await
            .map_err(|e| format!("Could not list profile tabs: {e}"))?
            .error_for_status()
            .map_err(|e| format!("Could not list profile tabs: {e}"))?
            .json::<Vec<crate::json_api::DevToolsTarget>>()
            .await
            .map_err(|e| format!("Could not read profile tabs: {e}"))?;
        let tabs: Vec<_> = tabs.into_iter().filter(|tab| tab.target_type == "page" && tab.profile.as_ref() == Some(profile))
            .map(|tab| json!({ "id": tab.id, "session_id": tab.session_id, "title": tab.title, "url": tab.url, "owner": tab.owner }))
            .collect();
        Ok(json!({ "profile": profile, "node": target.node, "tabs": tabs }))
    }

    /// (name, base URL, active sessions, capacity) of the nodes that take sessions.
    fn nodes(ov: &MeshOverview) -> Vec<(String, String, usize, usize)> {
        let mut nodes = vec![(
            ov.local_node.name.clone(),
            ov.local_node.url.clone(),
            ov.local_node.active_sessions,
            ov.local_node.max_sessions,
        )];
        nodes.extend(ov.peers.iter().filter(|p| p.status == "online").map(|p| {
            (
                p.name.clone(),
                p.url.clone(),
                p.active_sessions,
                p.max_sessions,
            )
        }));
        nodes
    }

    /// The nodes whose profiles the overview lists fewer of than they count: a
    /// peer that was too slow to answer `/api/profiles` shows up as online with none.
    fn incomplete_nodes(ov: &MeshOverview) -> Vec<String> {
        ov.peers
            .iter()
            .filter(|p| p.status == "online")
            .filter(|p| {
                ov.profiles.iter().filter(|m| m.node_name == p.name).count() < p.profiles_count
            })
            .map(|p| p.name.clone())
            .collect()
    }

    fn ws_base(url: &str) -> String {
        url.trim_end_matches('/').replacen("http", "ws", 1)
    }

    /// The node with the most room (or the one named).
    fn pick_node(ov: &MeshOverview, wanted: Option<&str>) -> Result<(String, String), String> {
        let nodes = Self::nodes(ov);
        if let Some(name) = wanted {
            return nodes
                .iter()
                .find(|n| n.0 == name)
                .map(|n| (n.0.clone(), n.1.clone()))
                .ok_or_else(|| {
                    format!(
                        "No node '{name}' online. Nodes: {}",
                        nodes
                            .iter()
                            .map(|n| n.0.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                });
        }
        nodes
            .iter()
            .filter(|n| n.2 < n.3)
            .min_by(|a, b| {
                let load = |n: &(String, String, usize, usize)| n.2 as f64 / n.3.max(1) as f64;
                load(a).total_cmp(&load(b)).then_with(|| a.0.cmp(&b.0))
            })
            .map(|n| (n.0.clone(), n.1.clone()))
            .ok_or_else(|| "Every node is at its session limit".to_string())
    }

    async fn default_target(&self) -> Result<Target, String> {
        let ov = self.overview().await?;
        let (node, url) = Self::pick_node(&ov, None)?;
        Ok(Target {
            node,
            ws_base: Self::ws_base(&url),
            url,
            profile: None,
            session_id: None,
        })
    }

    async fn status(&self) -> Result<String, String> {
        let ov = self.overview().await?;
        let current = self
            .child
            .lock()
            .await
            .as_ref()
            .map(|c| c.target.describe());
        let nodes: Vec<Value> = Self::nodes(&ov)
            .iter()
            .map(|n| {
                let labels = if n.0 == ov.local_node.name {
                    &ov.local_node.groups
                } else {
                    &ov.peers
                        .iter()
                        .find(|p| p.name == n.0)
                        .expect("listed node")
                        .groups
                };
                json!({ "node": n.0, "labels": labels, "sessions": format!("{}/{}", n.2, n.3) })
            })
            .collect();
        let profiles: Vec<Value> = ov
            .profiles
            .iter()
            .filter(|p| allows(&self.allow, &p.profile.name))
            .map(|p| {
                json!({
                    "profile": p.profile.name,
                    "node": p.node_name,
                    "in_use": p.profile.is_active,
                    "opens": p.profile.tabs,
                    "labels": p.profile.labels,
                })
            })
            .collect();
        let hidden = ov.profiles.len() - profiles.len();
        let mut status = json!({
            "current": current,
            "profiles_you_may_open": profiles,
            "other_profiles": format!("{hidden} more exist; the user has not allowed agents to open them"),
            "nodes": nodes,
        });
        let incomplete = Self::incomplete_nodes(&ov);
        if !incomplete.is_empty() {
            status["warning"] = json!(format!(
                "{} did not list all its profiles in time (busy?), so the list above may be short: call browser_status again",
                incomplete.join(", ")
            ));
        }
        Ok(serde_json::to_string_pretty(&status).unwrap_or_default())
    }

    async fn use_browser(&self, args: &Value) -> Result<String, String> {
        let ephemeral = args["ephemeral"].as_bool().unwrap_or(false);
        let profile = args["profile"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let node = args["node"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let group = args["label"]
            .as_str()
            .or_else(|| args["group"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if ephemeral && (profile.is_some() || node.is_some() || group.is_some()) {
            return Err(
                "An ephemeral browser cannot use a saved profile, node or label selector".into(),
            );
        }
        if let Some(name) = profile.filter(|name| !allows(&self.allow, name)) {
            return Err(format!(
                "Profile '{name}' is not open to agents here. Allowed: {}.",
                allowed(&self.allow)
            ));
        }
        let mut ov = self.overview().await?;
        let target = if ephemeral {
            let mut request = self
                .http
                .post(format!("{}/api/ephemeral/sessions", self.base))
                .timeout(Duration::from_secs(1800))
                .json(&json!({"persistent": false, "ephemeral_ttl_seconds": args["ephemeral_ttl_seconds"]}));
            if let Some(secret) = &self.secret {
                request = request.header("X-Browser-Cluster-Secret", secret);
            }
            let response = request
                .send()
                .await
                .map_err(|_| "Ephemeral provisioning request failed".to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "Ephemeral provisioning failed (HTTP {})",
                    response.status()
                ));
            }
            let body: Value = response
                .json()
                .await
                .map_err(|_| "Invalid ephemeral session response".to_string())?;
            let url = body["node_url"]
                .as_str()
                .ok_or("Missing ephemeral node URL")?
                .to_string();
            let id = body["session"]["id"]
                .as_str()
                .ok_or("Missing ephemeral session ID")?
                .to_string();
            Target {
                node: "ephemeral".into(),
                ws_base: Self::ws_base(&url),
                url,
                profile: None,
                session_id: Some(id),
            }
        } else if group.is_some() || (profile.is_some() && node.is_some()) {
            let mut request = self
                .http
                .post(format!("{}/api/mesh/placement", self.base))
                .timeout(Duration::from_secs(180))
                .json(&json!({"profile":profile,"node":node,"group":group}));
            if let Some(secret) = &self.secret {
                request = request.header("X-Browser-Cluster-Secret", secret);
            }
            let response = request
                .send()
                .await
                .map_err(|_| "Browser placement request failed".to_string())?;
            if !response.status().is_success() {
                return Err(format!("Browser placement refused (HTTP {}): check node/label capacity, active profile sessions and duplicate copies", response.status()));
            }
            let placed: crate::profile_transfer::Placement = response
                .json()
                .await
                .map_err(|_| "Invalid browser placement response".to_string())?;
            Target {
                node: placed.node,
                ws_base: Self::ws_base(&placed.node_url),
                url: placed.node_url,
                profile: placed.profile,
                session_id: None,
            }
        } else {
            match profile {
                Some(name) => {
                    if !allows(&self.allow, name) {
                        return Err(format!(
                            "Profile '{name}' is not open to agents here. Allowed: {}.",
                            allowed(&self.allow)
                        ));
                    }
                    let found = |ov: &MeshOverview| {
                        ov.profiles
                            .iter()
                            .filter(|p| {
                                p.profile.name == name && node.is_none_or(|n| p.node_name == n)
                            })
                            .count()
                    };
                    // The overview is shared and polls peers with a short timeout: a
                    // busy node can be missing its profiles for one round.
                    for _ in 0..2 {
                        if found(&ov) > 0 || Self::incomplete_nodes(&ov).is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2500)).await;
                        ov = self.overview().await?;
                    }
                    let hits: Vec<_> = ov
                        .profiles
                        .iter()
                        .filter(|p| p.profile.name == name && node.is_none_or(|n| p.node_name == n))
                        .collect();
                    let [p] = hits.as_slice() else {
                        return Err(if hits.is_empty() {
                            let incomplete = Self::incomplete_nodes(&ov);
                            format!(
                                "No profile '{name}'{}{}",
                                node.map(|n| format!(" on {n}")).unwrap_or_default(),
                                if incomplete.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                    ". {} did not list all its profiles in time (busy?): try again",
                                    incomplete.join(", ")
                                )
                                }
                            )
                        } else {
                            format!(
                                "Profile '{name}' exists on several nodes: name one with `node`"
                            )
                        });
                    };
                    Target {
                        node: p.node_name.clone(),
                        url: p.node_url.clone(),
                        ws_base: Self::ws_base(&p.node_url),
                        profile: Some(name.to_string()),
                        session_id: None,
                    }
                }
                None => {
                    let (node, url) = Self::pick_node(&ov, node)?;
                    Target {
                        node,
                        ws_base: Self::ws_base(&url),
                        url,
                        profile: None,
                        session_id: None,
                    }
                }
            }
        };
        let warning = match tokio::time::timeout(
            Duration::from_secs(2),
            self.shared_profile_warning(&target, 0),
        )
        .await
        {
            Ok(Ok(warning)) => warning,
            _ => profile_warning(&ov, &target, 0),
        }
        .map(|warning| format!(" {warning}"))
        .unwrap_or_default();
        let child = self.spawn_child(&target).await?;
        let old = self.child.lock().await.replace(child);
        if let Some(old) = old {
            old.stop().await;
        }
        self.notify_tools_changed().await;
        let start = if ephemeral {
            "The dedicated node and session are ready"
        } else {
            "It starts when you first use a browser tool"
        };
        Ok(format!(
            "Now using {}. {start}; the previous browser was closed.{}",
            target.describe(),
            warning
        ))
    }

    // ----------------------------------------------------------------- child

    async fn child_or_default(&self) -> Result<Arc<Child>, String> {
        if let Some(c) = self.child.lock().await.as_ref() {
            return Ok(c.clone());
        }
        let target = self.default_target().await?;
        let child = self.spawn_child(&target).await?;
        let mut slot = self.child.lock().await;
        // Another request may have been quicker.
        if let Some(c) = slot.as_ref() {
            child.stop().await;
            return Ok(c.clone());
        }
        *slot = Some(child.clone());
        Ok(child)
    }

    /// A token for `target`'s node, from the node (it is valid on the whole mesh).
    async fn auth_token(&self, target: &Target) -> Option<String> {
        let secret = self.secret.as_ref()?;
        let res = self
            .http
            .get(format!(
                "{}/api/auth/token",
                target.url.trim_end_matches('/')
            ))
            .header("X-Browser-Cluster-Secret", secret)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.ok()?;
                body["token"].as_str().map(str::to_string)
            }
            Ok(r) => {
                eprintln!(
                    "browser-daemon mcp: {} gave no auth token (HTTP {})",
                    target.node,
                    r.status()
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "browser-daemon mcp: {} gave no auth token: {e}",
                    target.node
                );
                None
            }
        }
    }

    async fn spawn_child(&self, target: &Target) -> Result<Arc<Child>, String> {
        // A fresh token for every child (they do not outlive it): Playwright cannot
        // send a header on a CDP connection, so the token goes in the address. The
        // cluster secret never does.
        let token = self.auth_token(target).await;
        // A new connection: its label has to be applied again.
        self.topic_applied.store(false, Ordering::SeqCst);
        let endpoint = cdp_endpoint(target, token.as_deref(), self.human_input);
        let out = self.out_dir.join(format!(
            "{}-{}",
            target.node,
            target.profile.as_deref().unwrap_or("throwaway")
        ));
        let client = self.client_name.lock().await.clone();
        let mut proc = Command::new(&self.cmd)
            .arg(&endpoint)
            .arg(&out)
            // Who is on the other end, for the wrapper's label when its environment doesn't say.
            .env("BROWSER_MCP_CLIENT", client)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Could not start {}: {e}", self.cmd.display()))?;
        let stdin = proc.stdin.take().ok_or("no stdin")?;
        let stdout = proc.stdout.take().ok_or("no stdout")?;
        let child = Arc::new(Child {
            stdin: Mutex::new(stdin),
            proc: Mutex::new(proc),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            tools: std::sync::Mutex::new(Vec::new()),
            target: target.clone(),
            used: AtomicBool::new(false),
        });
        tokio::spawn(read_child(child.clone(), stdout));

        let version = self.version.lock().await.clone();
        let setup = async {
            child
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": version,
                        "capabilities": {},
                        "clientInfo": { "name": "browser-daemon", "version": env!("CARGO_PKG_VERSION") },
                    }),
                    Duration::from_secs(30),
                )
                .await?;
            child
                .write(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await;
            let mut tools = Vec::new();
            let mut cursor: Option<String> = None;
            let mut cursors = std::collections::HashSet::new();
            loop {
                let params = cursor
                    .as_ref()
                    .map_or(json!({}), |c| json!({ "cursor": c }));
                let page = child
                    .request("tools/list", params, Duration::from_secs(30))
                    .await?;
                tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
                cursor = page["nextCursor"].as_str().map(str::to_string);
                match &cursor {
                    None => break,
                    Some(c) => advance_tool_cursor(&mut cursors, c)?,
                }
            }
            save_tool_cache(&tools);
            *child.tools.lock().unwrap() = tools;
            Ok::<(), (i64, String)>(())
        };
        if let Err((_, e)) = setup.await {
            child.stop().await;
            return Err(format!("The browser tools did not start: {e}"));
        }
        Ok(child)
    }
}

// ------------------------------------------------- page scripts and parsing

/// The text a `browser_run_code_unsafe` answer carries between `BDxxx:` and
/// `:BDEND`, as JSON. Playwright MCP prints a string result as a JSON string
/// (escaped) and echoes the script after it, so the first marker is the result's.
fn parse_marked(result: &Value, tag: &str) -> Option<Value> {
    let text: String = result["content"]
        .as_array()?
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect();
    let start = text.find(&format!("{tag}:"))? + tag.len() + 1;
    let body = &text[start..];
    let body = &body[..body.find(":BDEND")?];
    serde_json::from_str::<String>(&format!("\"{body}\""))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .or_else(|| serde_json::from_str(body).ok())
}

fn answer_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|c| c.iter().filter_map(|c| c["text"].as_str()).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------ bot walls

/// Playwright code that reports a few facts about the page; what they mean is
/// decided in `classify_wall`. It runs in the page and fails quietly.
const WALL_JS: &str = "async (page) => { try { return 'BDWALL:' + JSON.stringify(await page.evaluate(() => { \
const d = document, q = (s) => !!d.querySelector(s); \
const h = d.documentElement ? d.documentElement.outerHTML.slice(0, 400000).toLowerCase() : ''; \
const text = ((d.body && d.body.innerText) || '').trim(); \
const m = []; const add = (k, c) => { if (c) m.push(k); }; \
add('cf', q('#challenge-form, #cf-challenge-running, #challenge-running, #challenge-stage, .cf-browser-verification')); \
add('cf-weak', h.includes('cf-chl') || q('iframe[src*=\"challenges.cloudflare.com\"]')); \
add('dd', q('iframe[src*=\"captcha-delivery.com\"]')); \
add('dd-weak', h.includes('captcha-delivery.com')); \
add('px', q('#px-captcha')); \
add('imperva', q('iframe[src*=\"_incapsula_resource\" i]') || h.includes('incapsula incident id')); \
add('akamai', h.includes('errors.edgesuite.net')); \
add('awswaf', q('#awswaf-captcha, awswaf-captcha, #aws-waf-captcha')); \
add('awswaf-weak', h.includes('token.awswaf.com') || h.includes('awswaf')); \
add('recaptcha', q('iframe[src*=\"recaptcha\"], .g-recaptcha')); \
add('hcaptcha', q('iframe[src*=\"hcaptcha.com\"], .h-captcha')); \
return { t: d.title || '', n: text.length, i: d.querySelectorAll('input:not([type=hidden])').length, m, x: text.slice(0, 300).toLowerCase() }; })) + ':BDEND'; } catch (e) { return 'BDWALL:null:BDEND'; } }";

/// What `WALL_JS` saw.
#[derive(Debug, Default, PartialEq)]
struct WallFacts {
    title: String,
    /// Characters of visible text.
    body_len: usize,
    /// Visible inputs (a captcha's own fields are not inputs).
    inputs: usize,
    marks: Vec<String>,
    /// The first characters of the text, lowercase.
    text: String,
}

fn parse_wall_facts(result: &Value) -> Option<WallFacts> {
    let v = parse_marked(result, "BDWALL")?;
    Some(WallFacts {
        title: v["t"].as_str()?.to_string(),
        body_len: v["n"].as_u64()? as usize,
        inputs: v["i"].as_u64().unwrap_or(0) as usize,
        marks: v["m"]
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
        text: v["x"].as_str().unwrap_or_default().to_string(),
    })
}

/// The vendor of the bot wall the page shows, if it is one. Conservative: a
/// captcha or a challenge script inside a page with content (a login form) is
/// no wall; a strong marker, or a weak one on a page with almost no text, is.
fn classify_wall(f: &WallFacts) -> Option<&'static str> {
    let has = |m: &str| f.marks.iter().any(|x| x == m);
    let title = f.title.trim().to_lowercase();
    let short = f.body_len < 600;
    if has("cf")
        || title.starts_with("just a moment")
        || (title.starts_with("attention required")
            && (title.contains("cloudflare") || f.text.contains("cloudflare")))
        || (has("cf-weak") && short)
    {
        return Some("Cloudflare");
    }
    if has("dd") || (has("dd-weak") && short) {
        return Some("DataDome");
    }
    if has("px") {
        return Some("PerimeterX (HUMAN)");
    }
    if has("imperva") {
        return Some("Imperva (Incapsula)");
    }
    if has("akamai") && (title.contains("access denied") || f.text.contains("reference #")) {
        return Some("Akamai");
    }
    if has("awswaf") || (has("awswaf-weak") && short) {
        return Some("AWS WAF");
    }
    if short && f.inputs == 0 {
        if has("recaptcha") {
            return Some("reCAPTCHA");
        }
        if has("hcaptcha") {
            return Some("hCaptcha");
        }
    }
    const BLOCKED: [&str; 7] = [
        "access denied",
        "403 forbidden",
        "forbidden",
        "you have been blocked",
        "request blocked",
        "pardon our interruption",
        "sorry, you have been blocked",
    ];
    if short
        && BLOCKED
            .iter()
            .any(|b| title.starts_with(b) || f.text.starts_with(b))
    {
        return Some("an unidentified bot filter or block page");
    }
    None
}

/// The note appended to a navigation's answer. `cleared`: the wall was there and went away.
fn challenge_note(vendor: &str, cleared: bool, ask_human: bool) -> String {
    if cleared {
        return format!(
            "Note: a {vendor} bot check showed up and cleared by itself. Take a snapshot to see the page."
        );
    }
    format!(
        "Note: this page is a bot wall ({vendor}) that did not clear by itself. Do not try to solve or bypass it yourself{}",
        if ask_human {
            ": call browser_ask_human so the user can do it in the live view, then wait and take a snapshot."
        } else {
            "; tell the user a person has to do this."
        }
    )
}

/// Tools after which the page is checked for a bot wall.
fn may_hit_wall(name: &str) -> bool {
    matches!(
        name,
        "browser_navigate" | "browser_navigate_back" | "browser_click" | "browser_press_key"
    )
}

// -------------------------------------------------------------- HAR export

/// Playwright code that collects the traffic of the page's context. Playwright
/// MCP runs every script on the same `BrowserContext` object, so the collected
/// entries live on it (`__bdHar`) from one call to the next (until the browser
/// is switched). Headers are kept raw here and redacted by the MCP server.
const HAR_JS: &str = "async (page) => { \
const ACT = '__ACT__', MAXE = 2000, MAXB = 262144, MAXTOTAL = 16 * 1024 * 1024; \
const TEXT = /^(text\\/|application\\/(json|xml|javascript|x-www-form-urlencoded|graphql)|[^;\\s]*\\+(json|xml))/i; \
const ctx = page.context(); \
const out = (o) => 'BDHAR:' + JSON.stringify(o) + ':BDEND'; \
let h = ctx.__bdHar; \
const off = () => { if (h) { for (const [ev, fn] of h.ls) { try { ctx.off(ev, fn); } catch (e) {} } h.ls = []; h.on = false; } }; \
if (ACT === 'status') return out({ active: !!(h && h.on), entries: h ? h.entries.length : 0, dropped: h ? h.dropped : 0, since: h ? h.since : null }); \
if (ACT === 'cancel') { off(); ctx.__bdHar = undefined; return out({ cancelled: !!h }); } \
if (ACT === 'stop') { \
if (!h) return out({ none: true }); \
off(); \
await Promise.race([Promise.allSettled([...h.pend]), page.waitForTimeout(10000)]); \
ctx.__bdHar = undefined; \
return out({ since: h.since, dropped: h.dropped, entries: h.entries }); } \
if (h && h.on) return out({ already: true, entries: h.entries.length, since: h.since }); \
h = { on: true, since: new Date().toISOString(), entries: [], map: new Map(), pend: new Set(), ls: [], dropped: 0, bytes: 0 }; \
ctx.__bdHar = h; \
const track = (p) => { h.pend.add(p); p.then(() => h.pend.delete(p)); }; \
const hdrs = async (o) => { try { return await o.headersArray(); } catch (e) { return Object.entries(o.headers()).map(([name, value]) => ({ name, value: String(value) })); } }; \
const on = (ev, fn) => { ctx.on(ev, fn); h.ls.push([ev, fn]); }; \
on('request', (r) => { \
if (!h.on) return; \
if (h.entries.length >= MAXE) { h.dropped++; return; } \
const pd = r.postDataBuffer(); \
const e = { url: r.url(), method: r.method(), type: r.resourceType(), start: new Date().toISOString(), t0: Date.now(), post: pd ? pd.length : 0, status: 0 }; \
h.map.set(r, e); h.entries.push(e); \
track(hdrs(r).then((x) => { e.reqH = x; })); }); \
on('response', (res) => { const e = h.map.get(res.request()); if (e) { e.status = res.status(); e.statusText = res.statusText(); } }); \
on('requestfailed', (r) => { const e = h.map.get(r); if (e) { e.error = (r.failure() || {}).errorText || 'failed'; e.t1 = Date.now(); } }); \
on('requestfinished', (r) => track((async () => { \
const e = h.map.get(r); if (!e) return; \
e.t1 = Date.now(); \
try { e.timing = r.timing(); } catch (x) {} \
try { e.sizes = await r.sizes(); } catch (x) {} \
let res = null; try { res = await r.response(); } catch (x) {} \
if (!res) return; \
e.resH = await hdrs(res); \
const ct = ((e.resH.find((x) => x.name.toLowerCase() === 'content-type') || {}).value || ''); \
e.mime = ct.split(';')[0].trim().toLowerCase(); \
if (TEXT.test(ct) && e.status >= 200 && e.status < 300 && e.status !== 204 && h.bytes < MAXTOTAL) { \
try { const b = await res.body(); e.bodySize = b.length; e.trunc = b.length > MAXB; e.body = b.subarray(0, MAXB).toString('utf8'); h.bytes += e.body.length; } catch (x) {} } })())); \
return out({ started: true, since: h.since }); }";

const REDACTED: &str = "[redacted]";

/// A header whose value must not be written to a file.
fn secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !n.starts_with(':')
        && [
            "cookie", "token", "secret", "auth", "api-key", "apikey", "api_key", "csrf", "xsrf",
            "session",
        ]
        .iter()
        .any(|s| n.contains(s))
}

/// A query parameter whose value must not be written to a file.
fn secret_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "token",
        "code",
        "pass",
        "pwd",
        "key",
        "secret",
        "sig",
        "auth",
        "session",
        "jwt",
        "otp",
        "credential",
    ]
    .iter()
    .any(|s| n.contains(s))
}

/// A field of a JSON body whose value must not be written to a file (narrower
/// than `secret_param`: `code` and `key` are common in harmless answers).
fn secret_field(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "pass",
        "auth",
        "apikey",
        "api_key",
        "api-key",
        "session",
        "jwt",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|s| n.contains(s))
}

/// The address with the values of secret-looking query parameters masked, and
/// its query as HAR `queryString`.
fn mask_url(address: &str) -> (String, Vec<Value>) {
    let Ok(mut parsed) = url::Url::parse(address) else {
        return (address.to_string(), Vec::new());
    };
    parsed.set_fragment(None);
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| {
            let v = if secret_param(&k) {
                REDACTED.to_string()
            } else {
                v.into_owned()
            };
            (k.into_owned(), v)
        })
        .collect();
    if pairs.is_empty() {
        return (parsed.to_string(), Vec::new());
    }
    parsed
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let query = pairs
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    (parsed.to_string(), query)
}

/// HAR headers with secret values replaced, and the cookies of `Cookie` and
/// `Set-Cookie` as HAR cookies without values.
fn har_headers(raw: &Value) -> (Vec<Value>, Vec<Value>) {
    let mut headers = Vec::new();
    let mut cookies = Vec::new();
    for h in raw.as_array().into_iter().flatten() {
        let name = h["name"].as_str().unwrap_or_default();
        let value = h["value"].as_str().unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "cookie" => cookies.extend(value.split(';').filter_map(|c| cookie_name(c, false))),
            "set-cookie" => cookies.extend(value.lines().filter_map(|c| cookie_name(c, true))),
            _ => {}
        }
        // HTTP/2 pseudo-headers repeat the address (with its query) and add nothing.
        if name.starts_with(':') {
            continue;
        }
        let value = if secret_header(name) { REDACTED } else { value };
        headers.push(json!({ "name": name, "value": value }));
    }
    (headers, cookies)
}

fn cookie_name(pair: &str, set: bool) -> Option<Value> {
    let pair = if set { pair.split(';').next()? } else { pair };
    let name = pair.split('=').next()?.trim();
    (!name.is_empty()).then(|| json!({ "name": name, "value": REDACTED }))
}

fn mask_json(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                if secret_field(k) && !v.is_object() && !v.is_array() {
                    *v = Value::String(REDACTED.into());
                } else {
                    mask_json(v);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(mask_json),
        _ => {}
    }
}

/// A response body for the file: JSON gets its secret fields masked; JSON that
/// cannot be parsed (cut off at the size cap) is left out, as it cannot be masked.
fn har_body(text: &str, mime: &str) -> Option<String> {
    if !mime.contains("json") {
        return Some(text.to_string());
    }
    let mut v: Value = serde_json::from_str(text).ok()?;
    mask_json(&mut v);
    Some(v.to_string())
}

/// Milliseconds between two of a request's timing marks, -1 when not known.
fn span(timing: &Value, from: &str, to: &str) -> f64 {
    match (timing[from].as_f64(), timing[to].as_f64()) {
        (Some(a), Some(b)) if a >= 0.0 && b >= a => b - a,
        _ => -1.0,
    }
}

/// A HAR 1.2 log from what `HAR_JS` collected, credentials redacted. The second
/// number: entries with a response body.
fn build_har(raw: &Value) -> (Value, usize) {
    let mut bodies = 0;
    let entries: Vec<Value> = raw["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let (url, query) = mask_url(e["url"].as_str().unwrap_or_default());
            let (req_headers, req_cookies) = har_headers(&e["reqH"]);
            let (res_headers, res_cookies) = har_headers(&e["resH"]);
            let timing = &e["timing"];
            let elapsed = e["t1"].as_f64().zip(e["t0"].as_f64()).map_or(-1.0, |(b, a)| (b - a).max(0.0));
            let total = timing["responseEnd"]
                .as_f64()
                .filter(|t| *t >= 0.0)
                .unwrap_or(elapsed);
            let content_type = e["mime"].as_str().unwrap_or_default();
            let mut content = json!({
                "size": e["bodySize"].as_i64().unwrap_or(-1),
                "mimeType": content_type,
            });
            if let Some(text) = e["body"].as_str() {
                match har_body(text, content_type) {
                    Some(text) => {
                        bodies += 1;
                        content["text"] = Value::String(text);
                        if e["trunc"].as_bool() == Some(true) {
                            content["comment"] = "cut off at 256 KiB".into();
                        }
                    }
                    None => content["comment"] = "JSON body left out: it could not be parsed to mask secrets".into(),
                }
            }
            let redirect = e["resH"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("location")))
                .and_then(|h| h["value"].as_str())
                .map(|l| mask_url(l).0)
                .unwrap_or_default();
            let mut request = json!({
                "method": e["method"],
                "url": url,
                "httpVersion": "unknown",
                "cookies": req_cookies,
                "headers": req_headers,
                "queryString": query,
                "headersSize": e["sizes"]["requestHeadersSize"].as_i64().unwrap_or(-1),
                "bodySize": e["sizes"]["requestBodySize"].as_i64().unwrap_or_else(|| e["post"].as_i64().unwrap_or(0)),
            });
            if e["post"].as_i64().unwrap_or(0) > 0 {
                let mime = e["reqH"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("content-type")))
                    .and_then(|h| h["value"].as_str())
                    .unwrap_or_default();
                // The posted text is not kept: it is where passwords go.
                request["postData"] = json!({ "mimeType": mime, "text": "", "comment": "body not recorded" });
            }
            let mut entry = json!({
                "startedDateTime": e["start"],
                "time": total,
                "request": request,
                "response": {
                    "status": e["status"].as_i64().unwrap_or(0),
                    "statusText": e["statusText"].as_str().unwrap_or_default(),
                    "httpVersion": "unknown",
                    "cookies": res_cookies,
                    "headers": res_headers,
                    "content": content,
                    "redirectURL": redirect,
                    "headersSize": e["sizes"]["responseHeadersSize"].as_i64().unwrap_or(-1),
                    "bodySize": e["sizes"]["responseBodySize"].as_i64().unwrap_or(-1),
                },
                "cache": {},
                "timings": {
                    "blocked": -1,
                    "dns": span(timing, "domainLookupStart", "domainLookupEnd"),
                    "connect": span(timing, "connectStart", "connectEnd"),
                    "ssl": span(timing, "secureConnectionStart", "connectEnd"),
                    "send": 0,
                    "wait": span(timing, "requestStart", "responseStart"),
                    "receive": span(timing, "responseStart", "responseEnd"),
                },
                "_resourceType": e["type"],
            });
            if let Some(error) = e["error"].as_str() {
                entry["_error"] = error.into();
            }
            entry
        })
        .collect();
    (
        json!({ "log": {
            "version": "1.2",
            "creator": { "name": "browser-daemon", "version": env!("CARGO_PKG_VERSION") },
            "pages": [],
            "entries": entries,
        }}),
        bodies,
    )
}

/// `20261111T093005Z` for a time in seconds since the epoch.
fn utc_stamp(secs: u64) -> String {
    let (days, rest) = ((secs / 86400) as i64, secs % 86400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

impl State {
    // ------------------------------------------------------- bot walls

    /// Whether the page shows a bot wall: Some(vendor) if it does, None if it
    /// does not or the check failed (it never errors: a check that cannot run says nothing).
    async fn wall_vendor(&self, child: &Child) -> Option<&'static str> {
        let result = child
            .request(
                "tools/call",
                json!({ "name": "browser_run_code_unsafe", "arguments": { "code": WALL_JS } }),
                Duration::from_secs(5),
            )
            .await
            .ok()?;
        if result["isError"].as_bool() == Some(true) {
            return None;
        }
        classify_wall(&parse_wall_facts(&result)?)
    }

    /// After a navigation: waits up to `--challenge-wait` seconds for a bot wall
    /// to clear and says what is left. Never blocks longer than that wait.
    async fn watch_challenge(&self, child: &Child, result: &mut Value) {
        let Some(vendor) = self.wall_vendor(child).await else {
            return;
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.challenge_wait);
        let mut cleared = false;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // A check that cannot run also says None: ask once more after a moment.
            if self.wall_vendor(child).await.is_none() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if self.wall_vendor(child).await.is_none() {
                    cleared = true;
                    break;
                }
            }
        }
        let note = challenge_note(vendor, cleared, self.ask_human_cmd.is_some());
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            content.push(json!({ "type": "text", "text": note }));
        }
    }

    // -------------------------------------------------------------- HAR

    async fn network_har(&self, args: &Value) -> Result<String, String> {
        let action = args["action"].as_str().unwrap_or("status");
        if !["start", "stop", "status", "cancel"].contains(&action) {
            return Err("`action` is start, stop, status or cancel".into());
        }
        let child = self.child_or_default().await?;
        let code = HAR_JS.replace("__ACT__", action);
        let result = child
            .request(
                "tools/call",
                json!({ "name": "browser_run_code_unsafe", "arguments": { "code": code } }),
                Duration::from_secs(60),
            )
            .await
            .map_err(|(_, e)| format!("Could not reach the browser: {e}"))?;
        let Some(data) = parse_marked(&result, "BDHAR") else {
            let text = answer_text(&result);
            return Err(format!(
                "The browser did not answer as expected (is a page open?): {}",
                text.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("no answer")
            ));
        };
        let count = |d: &Value| d["entries"].as_u64().unwrap_or(0);
        match action {
            "start" if data["already"] == true => Ok(format!(
                "Already recording since {} ({} requests so far).",
                data["since"].as_str().unwrap_or("?"),
                count(&data)
            )),
            "start" => Ok("Recording the network traffic of this browser (all its tabs). Call browser_network_har with action stop to write the HAR file. Switching browsers (browser_use) loses the recording.".into()),
            "cancel" => Ok("Recording discarded.".into()),
            "status" if data["active"] == true => Ok(format!(
                "Recording since {}: {} requests{}.",
                data["since"].as_str().unwrap_or("?"),
                count(&data),
                match data["dropped"].as_u64().unwrap_or(0) {
                    0 => String::new(),
                    n => format!(" ({n} more not kept: entry cap)"),
                }
            )),
            "status" => Ok("Not recording.".into()),
            _ => {
                if data["none"] == true {
                    return Err("Not recording: nothing to stop (start first).".into());
                }
                let (mut har, bodies) = build_har(&data);
                let entries = har["log"]["entries"].as_array().map_or(0, Vec::len);
                let filled = self.filled.lock().unwrap().clone();
                secrets::redact(&mut har, &filled);
                let path = self.write_har(&har.to_string())?;
                Ok(format!(
                    "Wrote {entries} requests ({bodies} with a text body) to {}{}. Credentials in headers, cookies and secret-looking query parameters or JSON fields are replaced with {REDACTED}; posted bodies are not recorded.",
                    path.display(),
                    match data["dropped"].as_u64().unwrap_or(0) {
                        0 => String::new(),
                        n => format!(" ({n} more requests were not kept: entry cap)"),
                    }
                ))
            }
        }
    }

    /// Writes a HAR file under the output directory, readable by this user only.
    fn write_har(&self, content: &str) -> Result<PathBuf, String> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let dir = self.out_dir.join("har");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!(
            "{}-{}.har",
            utc_stamp(secs),
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        Ok(path)
    }

    // -------------------------------------------------- flight recorder

    async fn flight_recorder(&self, args: &Value) -> Result<String, String> {
        let action = args["action"].as_str().unwrap_or("status");
        if !["start", "stop", "status", "save"].contains(&action) {
            return Err("`action` is start, stop, status or save".into());
        }
        let (child, page_id, _, _) = self.current_page().await?;
        let session = self
            .page_session(&child.target.url, &page_id)
            .await
            .ok_or("Could not tell which session the page is in")?;
        let node_url = child.target.url.trim_end_matches('/');
        let mut body = json!({ "action": action });
        if action == "start" {
            if let Some(seconds) = args["seconds"].as_u64() {
                body["seconds"] = seconds.into();
            }
        }
        let mut req = self
            .http
            .post(format!("{node_url}/api/sessions/{session}/flight-recorder"))
            .json(&body);
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("{}: {e}", child.target.node))?;
        let status = res.status();
        let answer: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{} refused: {}",
                child.target.node,
                answer["error"].as_str().unwrap_or(status.as_str())
            ));
        }
        let active = answer["active"].as_bool().unwrap_or(false);
        let window = answer["seconds"].as_u64().unwrap_or(0);
        let buffered = answer["buffered_secs"].as_f64().unwrap_or(0.0);
        Ok(match (action, answer["recording_id"].as_str()) {
            ("save", Some(id)) => format!(
                "Saved the last {buffered:.0} s of the browser as recording {id}. The user can watch it in the dashboard of {} ({node_url}/#recordings/{}).",
                child.target.node,
                url_component(id)
            ),
            ("save", None) => "The node did not return a recording id: nothing was saved (is the flight recorder running? start it first).".into(),
            _ if active => format!(
                "The flight recorder is running: it keeps the last {window} s of this browser ({buffered:.0} s buffered). Call it with action save to keep them as a recording."
            ),
            _ => "The flight recorder is not running.".into(),
        })
    }
}

// ------------------------------------------- conversation, links, sharing

/// The agent's conversation id, as its harness puts it in the environment.
fn conversation_id(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    [
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
    ]
    .iter()
    .find_map(|k| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// The live view of a page on its node.
fn handoff_url(node_url: &str, page_id: &str) -> String {
    format!(
        "{}/handoff/?page={}",
        node_url.trim_end_matches('/'),
        url_component(page_id)
    )
}

/// The dashboard's player for a recording.
fn recording_url(node_url: &str, id: &str) -> String {
    format!(
        "{}/#recordings/{}",
        node_url.trim_end_matches('/'),
        url_component(id)
    )
}

const SHARE_DEFAULT_SECS: u64 = 3600;
const SHARE_MAX_SECS: u64 = 7 * 86400;

/// How long a public link lives, in seconds: `30m`, `1h`, `2d`, `90s` or a
/// number of seconds. One hour by default, at most 7 days, never unlimited.
fn parse_expiry(text: Option<&str>) -> Result<u64, String> {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(SHARE_DEFAULT_SECS);
    };
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`expires` is a duration like 30m, 1h or 2d, not '{text}'"))?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => {
            return Err(format!(
                "`expires` is a duration like 30m, 1h or 2d, not '{text}'"
            ))
        }
    };
    let secs = n.saturating_mul(factor);
    if secs < 60 {
        return Err("`expires` must be at least one minute".into());
    }
    if secs > SHARE_MAX_SECS {
        return Err(
            "Public links last at most 7 days (7d); there are no links without an end".into(),
        );
    }
    Ok(secs)
}

/// The id of the recording in progress for a session, if there is one.
fn active_recording(ov: &MeshOverview, session: &str) -> Option<(String, String)> {
    ov.recordings
        .iter()
        .find(|r| {
            r.recording.session_id.to_string() == session && r.recording.status == "recording"
        })
        .map(|r| (r.recording.id.to_string(), r.node_url.clone()))
}

/// Adds the behaviour hints MCP clients use to decide what needs confirming:
/// (name, read only, destructive, idempotent, open world). Tools not listed keep what they have.
fn apply_hints(tools: &mut [Value]) {
    const HINTS: [(&str, bool, bool, bool, bool); 19] = [
        ("browser_status", true, false, true, false),
        ("browser_profile_tabs", true, false, true, false),
        ("browser_use", false, true, false, false),
        ("browser_shared_nodes", true, false, true, false),
        ("browser_shared_command", false, true, false, true),
        ("browser_network_har", false, false, false, false),
        ("browser_flight_recorder", false, false, false, false),
        ("browser_set_topic", false, false, true, false),
        ("browser_live_link", true, false, true, false),
        ("browser_screenshot_censored", true, false, true, false),
        ("browser_recording", false, false, false, false),
        ("browser_share", false, false, false, true),
        ("browser_unshare", false, true, true, true),
        ("browser_fill_secret", false, false, false, true),
        ("browser_fill_card", false, false, false, true),
        ("browser_passkey", false, false, true, true),
        ("browser_export_auth", false, false, false, false),
        ("browser_fetch", false, true, false, true),
        ("browser_ask_human", false, false, false, true),
    ];
    for tool in tools {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        if let Some((_, ro, destructive, idempotent, open)) = HINTS.iter().find(|h| h.0 == name) {
            let a = &mut tool["annotations"];
            a["readOnlyHint"] = (*ro).into();
            // The spec reads these two only for tools that change something.
            if !ro {
                a["destructiveHint"] = (*destructive).into();
                a["idempotentHint"] = (*idempotent).into();
            }
            a["openWorldHint"] = (*open).into();
        }
    }
}

impl State {
    /// The agent's page: (child, page id, session id on its node).
    async fn current_session(&self) -> Result<(Arc<Child>, String, String), String> {
        let (child, page_id, _, _) = self.current_page().await?;
        let session = self
            .page_session(&child.target.url, &page_id)
            .await
            .ok_or("Could not tell which session the page is in")?;
        Ok((child, page_id, session))
    }

    /// One request to a node's API with the cluster secret; the answer, or what the node refused with.
    async fn node_call(
        &self,
        method: reqwest::Method,
        node: &str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut req = self.http.request(method, url);
        if let Some(body) = body {
            req = req.json(&body);
        }
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        let res = req.send().await.map_err(|e| format!("{node}: {e}"))?;
        let status = res.status();
        let answer: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{node} refused: {}",
                answer["error"].as_str().unwrap_or(status.as_str())
            ));
        }
        Ok(answer)
    }

    // ------------------------------------------------------------ topic

    /// Labels the agent's connections on these nodes; how many were labelled.
    async fn post_topic(&self, urls: &[String], conversation: &str, topic: &str) -> usize {
        let mut set = tokio::task::JoinSet::new();
        for url in urls {
            let req = self
                .http
                .post(format!("{}/api/agent-topic", url.trim_end_matches('/')))
                .timeout(Duration::from_secs(3))
                .json(&json!({ "conversation": conversation, "topic": topic }));
            set.spawn(async move {
                let res = req.send().await.ok()?;
                res.json::<Value>().await.ok()?["clients"].as_u64()
            });
        }
        let mut labelled = 0;
        while let Some(n) = set.join_next().await {
            labelled += n.ok().flatten().unwrap_or(0) as usize;
        }
        labelled
    }

    async fn set_topic(&self, args: &Value) -> Result<String, String> {
        let topic = args["topic"]
            .as_str()
            .ok_or("Say what you are doing (`topic`)")?
            .trim()
            .to_string();
        let conversation = self.conversation.as_deref().ok_or(
            "Your conversation id is not in this server's environment (CLAUDE_CODE_SESSION_ID, CODEX_THREAD_ID or CODEX_SESSION_ID), so the dashboard cannot tell which connections are yours.",
        )?;
        *self.topic.lock().unwrap() = Some(topic.clone());
        self.topic_applied.store(false, Ordering::SeqCst);
        let urls: Vec<String> = match self.overview().await {
            Ok(ov) => Self::nodes(&ov).into_iter().map(|n| n.1).collect(),
            Err(_) => self
                .child
                .lock()
                .await
                .as_ref()
                .map(|c| c.target.url.clone())
                .into_iter()
                .collect(),
        };
        let labelled = self.post_topic(&urls, conversation, &topic).await;
        if labelled > 0 {
            self.topic_applied.store(true, Ordering::SeqCst);
            Ok(format!(
                "Labelled {labelled} browser connection(s) of this conversation: {topic}."
            ))
        } else {
            Ok("Noted. No browser of this conversation is connected yet; the label is applied when it connects.".into())
        }
    }

    /// Applies a topic that was set before the browser connected, once it has.
    async fn apply_pending_topic(&self, child: &Child) {
        if self.topic_applied.load(Ordering::SeqCst) {
            return;
        }
        let (Some(conversation), Some(topic)) = (
            self.conversation.clone(),
            self.topic.lock().unwrap().clone(),
        ) else {
            return;
        };
        let urls = [child.target.url.clone()];
        let labelled = tokio::time::timeout(
            Duration::from_secs(3),
            self.post_topic(&urls, &conversation, &topic),
        )
        .await
        .unwrap_or(0);
        if labelled > 0 {
            self.topic_applied.store(true, Ordering::SeqCst);
        }
    }

    // ------------------------------------------- live link, recording

    async fn live_link(&self) -> Result<String, String> {
        let (child, page_id, _, host) = self.current_page().await?;
        Ok(format!(
            "Live view of your current page ({host}): {}. Whoever opens it can watch and, if they are signed in to the dashboard, also click and type.",
            handoff_url(&child.target.url, &page_id)
        ))
    }

    async fn recording(&self, args: &Value) -> Result<String, String> {
        let action = args["action"].as_str().unwrap_or("status");
        if !["start", "stop", "status"].contains(&action) {
            return Err("`action` is start, stop or status".into());
        }
        let (child, _, session) = self.current_session().await?;
        let node = child.target.node.clone();
        let base = child.target.url.trim_end_matches('/').to_string();
        let running = || async {
            self.overview()
                .await
                .ok()
                .and_then(|ov| active_recording(&ov, &session))
        };
        match action {
            "start" => {
                let answer = self
                    .node_call(
                        reqwest::Method::POST,
                        &node,
                        format!("{base}/api/sessions/{session}/record"),
                        None,
                    )
                    .await?;
                let id = match answer["recording_id"].as_str() {
                    Some(id) => Some(id.to_string()),
                    None => running().await.map(|r| r.0),
                };
                let id = id.map(|id| format!(" Recording {id}.")).unwrap_or_default();
                Ok(if answer["status"] == "already_recording" {
                    format!("Your session is already being recorded.{id}")
                } else {
                    format!("Recording your session.{id} Stop it with action stop.")
                })
            }
            "stop" => {
                let answer = self
                    .node_call(
                        reqwest::Method::POST,
                        &node,
                        format!("{base}/api/sessions/{session}/stop-record"),
                        None,
                    )
                    .await?;
                let id = answer["recording_id"].as_str().unwrap_or_default();
                Ok(format!(
                    "Stopped recording {id}. The user can watch it at {}.",
                    recording_url(&base, id)
                ))
            }
            _ => Ok(match running().await {
                Some((id, url)) => format!(
                    "Your session is being recorded: recording {id} ({}).",
                    recording_url(&url, &id)
                ),
                None => "Your session is not being recorded right now.".into(),
            }),
        }
    }

    // ---------------------------------------------------------- sharing

    async fn share(&self, args: &Value) -> Result<String, String> {
        let secs = parse_expiry(args["expires"].as_str())?;
        let control = args["control"].as_bool().unwrap_or(false);
        let (kind, id, node, base) = match args["recording_id"].as_str().map(str::trim) {
            Some(id) if !id.is_empty() => {
                let ov = self.overview().await?;
                let r = ov
                    .recordings
                    .iter()
                    .find(|r| r.recording.id.to_string() == id)
                    .ok_or_else(|| format!("No recording '{id}' (see the dashboard for ids)"))?;
                (
                    "recording",
                    id.to_string(),
                    r.node_name.clone(),
                    r.node_url.clone(),
                )
            }
            _ => {
                let (child, _, session) = self.current_session().await?;
                (
                    "session",
                    session,
                    child.target.node.clone(),
                    child.target.url.clone(),
                )
            }
        };
        let body = json!({
            "kind": kind,
            "id": id,
            "expires_in_secs": secs,
            "control": kind == "session" && control,
        });
        let answer = self
            .node_call(
                reqwest::Method::POST,
                &node,
                format!("{}/api/shares", base.trim_end_matches('/')),
                Some(body),
            )
            .await?;
        let url = answer["url"].as_str().ok_or("The node returned no link")?;
        let share_id = answer["id"].as_str().unwrap_or_default();
        self.shares
            .lock()
            .unwrap()
            .push((share_id.to_string(), node.clone(), base));
        Ok(format!(
            "PUBLIC link, anyone who has it can {} until it expires in {}: {url}\nShare id {share_id} (browser_unshare ends it). Give it only to the person the user wants it shared with.",
            if kind == "session" && control { "watch and control your browser session" } else if kind == "session" { "watch your browser session (view only)" } else { "watch the recording" },
            describe_secs(secs),
        ))
    }

    async fn unshare(&self, args: &Value) -> Result<String, String> {
        let id = args["share_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("Give the `share_id` browser_share returned")?;
        let (node, base) = self
            .shares
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.0 == id)
            .map(|s| (s.1.clone(), s.2.clone()))
            .ok_or("Only links made with browser_share in this server can be ended here")?;
        self.node_call(
            reqwest::Method::DELETE,
            &node,
            format!("{}/api/shares/{id}", base.trim_end_matches('/')),
            None,
        )
        .await?;
        self.shares.lock().unwrap().retain(|s| s.0 != id);
        Ok("The link no longer works.".into())
    }
}

fn describe_secs(secs: u64) -> String {
    match secs {
        s if s % 86400 == 0 => format!("{} day(s)", s / 86400),
        s if s % 3600 == 0 => format!("{} hour(s)", s / 3600),
        s => format!("{} minute(s)", s.div_ceil(60)),
    }
}

// ------------------------------------------------- censored screenshots

/// What a censored screenshot blacks out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Censor {
    Passwords,
    Cards,
    Otp,
    Secrets,
    Inputs,
    Emails,
}

impl Censor {
    const DEFAULT: [Censor; 5] = [
        Censor::Passwords,
        Censor::Cards,
        Censor::Otp,
        Censor::Secrets,
        Censor::Inputs,
    ];

    fn name(self) -> &'static str {
        match self {
            Censor::Passwords => "passwords",
            Censor::Cards => "cards",
            Censor::Otp => "otp",
            Censor::Secrets => "secrets",
            Censor::Inputs => "inputs",
            Censor::Emails => "emails",
        }
    }

    fn parse(name: &str) -> Option<Censor> {
        Some(match name {
            "passwords" => Censor::Passwords,
            "cards" => Censor::Cards,
            "otp" => Censor::Otp,
            "secrets" => Censor::Secrets,
            "inputs" => Censor::Inputs,
            "emails" => Censor::Emails,
            _ => return None,
        })
    }
}

#[derive(Debug, PartialEq)]
struct CensorArgs {
    cats: Vec<Censor>,
    full_page: bool,
    target: Option<String>,
    png: bool,
}

/// The arguments of browser_screenshot_censored, or of Playwright MCP's
/// browser_take_screenshot when `--censor-screenshots` answers that one.
fn parse_censor_args(args: &Value) -> Result<CensorArgs, String> {
    let cats = match args.get("censor") {
        None | Some(Value::Null) => Censor::DEFAULT.to_vec(),
        Some(Value::Array(names)) => {
            let mut cats = Vec::new();
            for name in names {
                let name = name.as_str().unwrap_or_default();
                let cat = Censor::parse(name).ok_or_else(|| {
                    format!("Unknown `censor` category '{name}': use passwords, cards, otp, secrets, inputs or emails")
                })?;
                if !cats.contains(&cat) {
                    cats.push(cat);
                }
            }
            if cats.is_empty() {
                return Err("`censor` is empty: name at least one category, or leave it out for the defaults".into());
            }
            cats
        }
        Some(_) => return Err("`censor` is a list of category names".into()),
    };
    let target = ["target", "ref"]
        .iter()
        .find_map(|k| args[*k].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(CensorArgs {
        cats,
        full_page: args["fullPage"].as_bool().unwrap_or(false) && target.is_none(),
        target,
        png: args["type"].as_str() != Some("jpeg"),
    })
}

/// Text-like inputs, textareas and editable elements.
const TEXT_INPUTS: &str = "input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=button]):not([type=submit]):not([type=reset]):not([type=image]):not([type=file]):not([type=range]):not([type=color]), textarea, [contenteditable=''], [contenteditable=true]";

fn name_hints(hints: &[&str]) -> Vec<String> {
    hints
        .iter()
        .flat_map(|h| {
            [
                format!("input[name*=\"{h}\" i]"),
                format!("input[id*=\"{h}\" i]"),
            ]
        })
        .collect()
}

/// CSS selectors of the elements a category blacks out always (the inputs
/// category only those with a value, the secrets and emails by their text).
fn censor_selectors(cats: &[Censor]) -> Vec<String> {
    let mut sels = Vec::new();
    for cat in cats {
        match cat {
            Censor::Passwords => sels.push("input[type=password]".to_string()),
            Censor::Cards => {
                sels.push("[autocomplete^=cc-]".to_string());
                sels.extend(name_hints(&[
                    "card",
                    "ccnum",
                    "cc-num",
                    "cvc",
                    "cvv",
                    "expir",
                    "expdate",
                    "exp-date",
                    "securitycode",
                    "security-code",
                ]));
            }
            Censor::Otp => {
                sels.push("[autocomplete=one-time-code]".to_string());
                sels.extend(name_hints(&["otp", "totp", "2fa", "mfa", "one-time"]));
                sels.push("input[name=code i]".to_string());
                sels.push("input[id=code i]".to_string());
            }
            Censor::Secrets | Censor::Inputs | Censor::Emails => {}
        }
    }
    sels
}

/// What `SHOT_JS` is told: no secret in it, as Playwright MCP echoes scripts.
fn censor_plan(a: &CensorArgs, ids: &[u64]) -> Value {
    json!({
        "sels": censor_selectors(&a.cats),
        "filled": a.cats.contains(&Censor::Inputs).then_some(TEXT_INPUTS),
        "emails": a.cats.contains(&Censor::Emails),
        "frames": a.cats.contains(&Censor::Cards),
        "ids": ids,
        "full": a.full_page,
        "target": a.target,
        "type": if a.png { "png" } else { "jpeg" },
    })
}

/// Tags every element that has text of its own, or a value, with a number and
/// returns the texts, so that the secrets can be looked for here and not in
/// a script (which Playwright MCP echoes).
const TEXTS_JS: &str = "async (page) => { const out = []; let next = 0; \
for (const f of page.frames()) { if (f.isDetached() || out.length > 20000) continue; \
const rows = await f.evaluate((start) => { const rows = []; let id = start; \
document.querySelectorAll('*').forEach((el) => { \
if (el.tagName === 'SCRIPT' || el.tagName === 'STYLE') return; \
let t = ''; for (const c of el.childNodes) if (c.nodeType === 3) t += c.nodeValue; \
if (typeof el.value === 'string' && el.value) t += ' ' + el.value; \
t = t.trim(); if (t.length < 4) return; \
el.setAttribute('data-bd-i', String(id)); rows.push([id, t.slice(0, 2000)]); id++; }); \
return rows; }, next); \
for (const r of rows) out.push(r); if (rows.length) next = rows[rows.length - 1][0] + 1; } \
return 'BDTXT:' + JSON.stringify(out) + ':BDEND'; }";

/// Marks what is to be blacked out, takes the screenshot with those elements
/// masked and removes the marks again. Any failure throws: no screenshot
/// without the mask.
const SHOT_JS: &str = "async (page) => { const P = __PLAN__; \
const MARK = (p) => { let n = 0; \
const mk = (el) => { if (el && el.setAttribute) { el.setAttribute('data-bd-censor', '1'); n++; } }; \
if (p.sels.length) document.querySelectorAll(p.sels.join(',')).forEach(mk); \
if (p.filled) document.querySelectorAll(p.filled).forEach((el) => { const v = typeof el.value === 'string' ? el.value : el.textContent; if (v && v.trim()) mk(el); }); \
if (p.ids.length) { const s = new Set(p.ids.map(String)); document.querySelectorAll('[data-bd-i]').forEach((el) => { if (s.has(el.getAttribute('data-bd-i'))) mk(el); }); } \
if (p.emails) { const re = /[A-Z0-9._%+-]+@[A-Z0-9.-]+\\.[A-Z]{2,}/i; \
document.querySelectorAll('input,textarea').forEach((el) => { if (re.test(el.value || '')) mk(el); }); \
const w = document.createTreeWalker(document.body || document, NodeFilter.SHOW_TEXT); \
for (let t = w.nextNode(); t; t = w.nextNode()) { if (re.test(t.nodeValue) && t.parentElement) mk(t.parentElement); } } \
if (p.frames) document.querySelectorAll('iframe,frame').forEach((f) => { let blocked = false; try { blocked = !f.contentDocument; } catch (e) { blocked = true; } if (blocked) mk(f); }); \
return n; }; \
const frames = page.frames().filter((f) => !f.isDetached()); \
try { let masked = 0; \
for (const f of frames) masked += await f.evaluate(MARK, P); \
const opts = { mask: frames.map((f) => f.locator('[data-bd-censor]')), maskColor: '#000000', type: P.type }; \
if (P.type === 'jpeg') opts.quality = 80; \
let buf; \
if (P.target) { const loc = page.locator(/^(f\\d+)?e\\d+$/.test(P.target) ? 'aria-ref=' + P.target : P.target); buf = await loc.screenshot(opts); } \
else { opts.fullPage = P.full; buf = await page.screenshot(opts); } \
return 'BDSHOT:' + JSON.stringify({ mime: P.type === 'png' ? 'image/png' : 'image/jpeg', data: buf.toString('base64'), masked }) + ':BDEND'; \
} finally { for (const f of frames) { if (f.isDetached()) continue; try { await f.evaluate(() => document.querySelectorAll('[data-bd-censor],[data-bd-i]').forEach((e) => { e.removeAttribute('data-bd-censor'); e.removeAttribute('data-bd-i'); })); } catch (e) {} } } }";

/// The numbers of the elements (from `TEXTS_JS`) whose text holds a secret.
/// What a shared tab's screenshot is asked to hide: the categories (`given`,
/// plus all defaults when `force`) and, with `secrets` among them, the values
/// filled in this session. None: no censoring asked for.
/// Categories and the secrets to hide, for a shared tab's screenshot.
type SharedCensor = (Vec<&'static str>, Option<Vec<String>>);

fn shared_censor(
    given: Option<&Value>,
    force: bool,
    filled: &[String],
) -> Result<Option<SharedCensor>, String> {
    let mut cats = match given {
        None => Vec::new(),
        Some(Value::Array(a)) if a.is_empty() => Vec::new(),
        Some(v) => {
            parse_censor_args(&json!({ "censor": v }))
                .map_err(|e| e.replace("`censor`", "`censor` of a screenshot"))?
                .cats
        }
    };
    if cats.is_empty() && !force {
        return Ok(None);
    }
    if force {
        for c in Censor::DEFAULT {
            if !cats.contains(&c) {
                cats.push(c);
            }
        }
    }
    let secrets = cats.contains(&Censor::Secrets).then(|| {
        filled
            .iter()
            .filter(|s| s.len() >= 4)
            .cloned()
            .collect::<Vec<_>>()
    });
    Ok(Some((cats.iter().map(|c| c.name()).collect(), secrets)))
}

fn secret_element_ids(rows: &Value, filled: &[String]) -> Vec<u64> {
    let secrets: Vec<String> = filled.iter().filter(|s| s.len() >= 4).cloned().collect();
    if secrets.is_empty() {
        return Vec::new();
    }
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let text = row[1].as_str()?;
            (secrets::redact_str(text, &secrets) != text).then(|| row[0].as_u64())?
        })
        .collect()
}

impl State {
    /// A screenshot of the agent's page with passwords, card and one-time-code
    /// fields, filled inputs and the secrets of this session blacked out. Fails
    /// closed: if the mask cannot be built there is no screenshot.
    async fn censored_answer(&self, args: &Value) -> Value {
        match self.screenshot_censored(args).await {
            Ok(result) => result,
            Err(e) => text_result(format!("No screenshot taken: {e}"), true),
        }
    }

    async fn screenshot_censored(&self, args: &Value) -> Result<Value, String> {
        let a = parse_censor_args(args)?;
        let child = self.child_or_default().await?;
        let run = |code: String| {
            let child = child.clone();
            async move {
                let result = child
                    .request(
                        "tools/call",
                        json!({ "name": "browser_run_code_unsafe", "arguments": { "code": code } }),
                        Duration::from_secs(90),
                    )
                    .await
                    .map_err(|(_, e)| format!("The browser did not answer: {e}"))?;
                if result["isError"].as_bool() == Some(true) {
                    let text = answer_text(&result);
                    let line = text
                        .lines()
                        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                        .unwrap_or("the script failed");
                    return Err(format!("Could not build the mask: {line}"));
                }
                Ok(result)
            }
        };
        let filled = self.filled.lock().unwrap().clone();
        let mut ids = Vec::new();
        if a.cats.contains(&Censor::Secrets) && filled.iter().any(|s| s.len() >= 4) {
            let result = run(TEXTS_JS.to_string()).await?;
            let rows = parse_marked(&result, "BDTXT")
                .ok_or("Could not read the page's text to find secrets in it")?;
            ids = secret_element_ids(&rows, &filled);
        }
        let code = SHOT_JS.replace("__PLAN__", &censor_plan(&a, &ids).to_string());
        let result = run(code).await?;
        let shot = parse_marked(&result, "BDSHOT").ok_or("The browser returned no screenshot")?;
        let (Some(data), Some(mime)) = (shot["data"].as_str(), shot["mime"].as_str()) else {
            return Err("The browser returned no screenshot".into());
        };
        let names: Vec<&str> = a.cats.iter().map(|c| c.name()).collect();
        Ok(json!({ "content": [
            { "type": "image", "data": data, "mimeType": mime },
            { "type": "text", "text": format!(
                "Censored screenshot: {} region(s) blacked out ({}). Shadow DOM and canvas content are not covered.",
                shot["masked"].as_u64().unwrap_or(0),
                names.join(", ")
            ) },
        ], "isError": false }))
    }
}

// A child must advance its cursor and finish listing in a bounded number of pages.
fn advance_tool_cursor(
    seen: &mut std::collections::HashSet<String>,
    cursor: &str,
) -> Result<(), (i64, String)> {
    if seen.len() >= 100 || !seen.insert(cursor.to_owned()) {
        return Err((-32603, "Invalid or excessive tool pagination".into()));
    }
    Ok(())
}

/// Answers of a child go to whoever asked; its own requests get the plainest reply.
async fn read_child(child: Arc<Child>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        match (msg.get("method").and_then(Value::as_str), id) {
            // An answer.
            (None, Some(id)) => {
                if let Some(i) = id.as_u64() {
                    if let Some(tx) = child.pending.lock().await.remove(&i) {
                        let _ = tx.send(msg);
                    }
                }
            }
            // A request of the child: no roots, nothing else.
            (Some(method), Some(id)) => {
                let reply = match method {
                    "roots/list" => {
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": [] } })
                    }
                    "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                    _ => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
                    }
                };
                child.write(&reply).await;
            }
            _ => {}
        }
    }
    // The child ended: whoever still waits gets an error.
    let waiting: Vec<_> = child.pending.lock().await.drain().collect();
    for (_, tx) in waiting {
        let _ =
            tx.send(json!({ "error": { "code": -32000, "message": "The browser tools stopped" } }));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_pagination_rejects_repeated_and_excessive_cursors() {
        let mut seen = std::collections::HashSet::new();
        assert!(super::advance_tool_cursor(&mut seen, "first").is_ok());
        assert!(super::advance_tool_cursor(&mut seen, "first").is_err());
        for i in 1..100 {
            assert!(super::advance_tool_cursor(&mut seen, &i.to_string()).is_ok());
        }
        assert!(super::advance_tool_cursor(&mut seen, "overflow").is_err());
    }
    use super::*;

    #[test]
    fn a_page_session_is_only_named_when_unique() {
        let list = json!([
            { "id": "a", "sessionId": "s1" },
            { "id": "b", "sessionId": "s1" },
            { "id": "b", "sessionId": "s2" },
        ]);
        assert_eq!(unique_session(&list, "a").as_deref(), Some("s1"));
        assert_eq!(unique_session(&list, "b"), None);
        assert_eq!(unique_session(&list, "c"), None);
    }

    fn overview(nodes: &[(&str, usize, usize, &str)]) -> MeshOverview {
        let node = |n: &(&str, usize, usize, &str)| {
            json!({
                "name": n.0, "url": format!("https://browser.{}.test", n.0), "version": "0",
                "uptime_seconds": 1, "active_sessions": n.1, "max_sessions": n.2,
                "profiles_count": 0, "recordings_count": 0, "storage_used_bytes": 0, "max_storage_bytes": 0,
                "status": n.3, "latency_ms": null
            })
        };
        serde_json::from_value(json!({
            "local_node": node(&nodes[0]),
            "peers": nodes[1..].iter().map(node).collect::<Vec<_>>(),
            "sessions": [], "profiles": [], "targets": [], "recordings": [],
            "total_sessions": 0, "total_capacity": 0, "total_profiles": 0, "total_recordings": 0,
            "total_nodes": nodes.len(), "online_nodes": nodes.len(),
            "total_storage_bytes": 0, "max_storage_bytes": 0
        }))
        .unwrap()
    }

    #[test]
    fn profile_warning_matches_node_profile_and_connected_clients() {
        let mut ov = overview(&[("a", 0, 5, "online")]);
        let target = Target {
            node: "a".into(),
            url: String::new(),
            ws_base: String::new(),
            profile: Some("work".into()),
            session_id: None,
        };
        assert!(profile_warning(&ov, &target, 0).is_none());
        ov.sessions.push(serde_json::from_value(json!({
            "id": "00000000-0000-0000-0000-000000000001", "port": 1, "ws_path": "",
            "profile": "work", "persistent": true, "uptime_seconds": 0,
            "client_count": 1, "tabs_count": 1, "is_recording": false,
            "node_name": "a", "node_url": "", "is_local": true,
            "clients": [{"kind": "Playwright", "agent": true, "from": "a", "since": 0, "agent_name": "AgentA"}]
        })).unwrap());
        assert!(profile_warning(&ov, &target, 0).unwrap().contains("AgentA"));
        assert!(profile_warning(&ov, &target, 2).is_none());
        let client = ov.sessions[0].session.clients[0].clone();
        ov.sessions[0].session.clients.push(client);
        assert!(profile_warning(&ov, &target, 2).is_some());
        let other = Target {
            node: "b".into(),
            ..target
        };
        assert!(profile_warning(&ov, &other, 0).is_none());
    }

    #[test]
    fn the_node_with_the_most_room_is_picked() {
        let ov = overview(&[
            ("fnuc", 4, 5, "online"),
            ("a", 1, 10, "online"),
            ("b", 5, 10, "online"),
        ]);
        assert_eq!(State::pick_node(&ov, None).unwrap().0, "a");
        assert_eq!(State::pick_node(&ov, Some("b")).unwrap().0, "b");
        assert!(State::pick_node(&ov, Some("nope")).is_err());
    }

    #[test]
    fn a_node_that_lists_fewer_profiles_than_it_counts_is_incomplete() {
        let mut ov = overview(&[("fnuc", 0, 5, "online"), ("a", 0, 10, "online")]);
        assert!(State::incomplete_nodes(&ov).is_empty());
        ov.peers[0].profiles_count = 2;
        assert_eq!(State::incomplete_nodes(&ov), vec!["a".to_string()]);
        ov.peers[0].status = "offline".to_string();
        assert!(State::incomplete_nodes(&ov).is_empty());
    }

    #[test]
    fn full_and_offline_nodes_are_skipped() {
        let ov = overview(&[
            ("fnuc", 5, 5, "online"),
            ("a", 0, 10, "offline"),
            ("b", 9, 10, "online"),
        ]);
        assert_eq!(State::pick_node(&ov, None).unwrap().0, "b");
        let full = overview(&[("fnuc", 5, 5, "online")]);
        assert!(State::pick_node(&full, None).is_err());
    }

    #[test]
    fn the_page_info_is_found_in_the_answer() {
        let answer = json!({ "content": [{ "type": "text", "text": "### Result\n\"BDPAGE:E831D6:https://auth.frigate.video/login?redirect_uri=x:BDEND\"\n### Ran Playwright code" }] });
        assert_eq!(
            parse_page_info(&answer),
            Some((
                "E831D6".to_string(),
                "https://auth.frigate.video/login?redirect_uri=x".to_string(),
                "auth.frigate.video".to_string()
            ))
        );
        assert_eq!(
            parse_page_info(&json!({ "content": [{ "type": "text", "text": "Error" }] })),
            None
        );
    }

    #[test]
    fn the_secret_tool_is_only_offered_with_allowed_entries() {
        let names = |t: Vec<Value>| {
            t.iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(
            !names(own_tools(&[], &[], false, &[])).contains(&"browser_fill_secret".to_string())
        );
        assert!(names(own_tools(&["Frigate Plus".into()], &[], false, &[]))
            .contains(&"browser_fill_secret".to_string()));
    }

    #[test]
    fn logins_become_jars_headers_and_tokens() {
        let cookies = vec![
            json!({ "name": "sid", "value": "s3cr3t", "domain": ".example.com", "path": "/", "expires": 1900000000.5, "http_only": true, "secure": true }),
            json!({ "name": "pref", "value": "dark", "domain": "app.example.com", "path": "/app", "expires": null, "http_only": false, "secure": false }),
        ];
        let jar = cookie_jar(&cookies);
        assert!(jar.contains("#HttpOnly_.example.com\tTRUE\t/\tTRUE\t1900000000\tsid\ts3cr3t\n"));
        assert!(jar.contains("app.example.com\tFALSE\t/app\tFALSE\t0\tpref\tdark\n"));
        assert_eq!(
            cookie_header(&cookies, "https://app.example.com/app/x").unwrap(),
            "sid=s3cr3t; pref=dark"
        );
        assert_eq!(
            cookie_header(&cookies, "https://api.example.com/").unwrap(),
            "sid=s3cr3t"
        );
        assert_eq!(
            cookie_header(&cookies, "http://app.example.com/app").unwrap(),
            "pref=dark"
        );
        assert_eq!(
            bearer_token(r#"{"credentialType":"AccessToken","secret":"eyJ.a.b"}"#),
            "eyJ.a.b"
        );
        assert_eq!(bearer_token("\"plain-token\""), "plain-token");
    }

    #[test]
    fn export_tools_are_only_offered_with_allowed_sites() {
        let names = |t: Vec<Value>| {
            t.iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(!names(own_tools(&[], &[], false, &[])).contains(&"browser_fetch".to_string()));
        let on = names(own_tools(&[], &[], false, &["microsoft.com".into()]));
        assert!(
            on.contains(&"browser_export_auth".to_string())
                && on.contains(&"browser_fetch".to_string())
        );
    }

    #[test]
    fn passkey_and_ask_human_tools_are_only_offered_when_configured() {
        let names = |t: Vec<Value>| {
            t.iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let none = names(own_tools(&[], &[], false, &[]));
        assert!(!none.contains(&"browser_passkey".to_string()));
        assert!(!none.contains(&"browser_ask_human".to_string()));
        let both = names(own_tools(&[], &["microsoft.com".into()], true, &[]));
        assert!(both.contains(&"browser_passkey".to_string()));
        assert!(both.contains(&"browser_ask_human".to_string()));
    }

    #[test]
    fn a_star_allows_everything() {
        let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(allows(&list(&["work"]), "work"));
        assert!(!allows(&list(&["work"]), "hrworks"));
        assert!(!allows(&[], "work"));
        assert!(allows(&list(&["*"]), "anything"));
        assert_eq!(allowed(&[]), "none");
        assert_eq!(allowed(&list(&["work", "*"])), "any");
        assert_eq!(allowed(&list(&["work", "google"])), "work, google");
    }

    #[test]
    fn web_addresses_become_websocket_ones() {
        assert_eq!(
            State::ws_base("https://browser.x.test/"),
            "wss://browser.x.test"
        );
        assert_eq!(
            State::ws_base("http://127.0.0.1:3001"),
            "ws://127.0.0.1:3001"
        );
    }

    #[test]
    fn the_endpoint_carries_the_token_never_the_secret() {
        let t = |profile: Option<&str>| Target {
            node: "n".into(),
            url: "https://b.test".into(),
            ws_base: "wss://b.test".into(),
            profile: profile.map(str::to_string),
            session_id: None,
        };
        assert_eq!(cdp_endpoint(&t(None), None, false), "wss://b.test/chromium");
        assert_eq!(
            cdp_endpoint(&t(None), Some("123.abc"), false),
            "wss://b.test/chromium?auth=123.abc"
        );
        assert_eq!(
            cdp_endpoint(&t(Some("my work")), Some("123.abc"), false),
            "wss://b.test/chromium?profile=my+work&auth=123.abc"
        );
        assert_eq!(
            cdp_endpoint(&t(Some("work")), None, false),
            "wss://b.test/chromium?profile=work"
        );
        let mut ephemeral = t(None);
        ephemeral.session_id = Some("session-1".into());
        assert_eq!(
            cdp_endpoint(&ephemeral, Some("123.abc"), false),
            "wss://b.test/chromium?session_id=session-1&auth=123.abc"
        );
        assert_eq!(
            cdp_endpoint(&t(Some("work")), Some("123.abc"), true),
            "wss://b.test/chromium?profile=work&auth=123.abc&human_input=1"
        );
        assert_eq!(
            cdp_endpoint(&t(None), None, true),
            "wss://b.test/chromium?human_input=1"
        );
    }

    fn facts(title: &str, body_len: usize, inputs: usize, marks: &[&str], text: &str) -> WallFacts {
        WallFacts {
            title: title.into(),
            body_len,
            inputs,
            marks: marks.iter().map(|m| m.to_string()).collect(),
            text: text.into(),
        }
    }

    #[test]
    fn bot_walls_are_told_from_pages_that_embed_a_captcha() {
        let wall = |f: WallFacts| classify_wall(&f);
        assert_eq!(
            wall(facts("Just a moment...", 120, 0, &[], "")),
            Some("Cloudflare")
        );
        assert_eq!(wall(facts("x", 5000, 0, &["cf"], "")), Some("Cloudflare"));
        assert_eq!(
            wall(facts("x", 80, 0, &["cf-weak"], "")),
            Some("Cloudflare")
        );
        // A login form with a Turnstile widget is no wall.
        assert_eq!(
            wall(facts("Sign in", 1500, 3, &["cf-weak"], "sign in")),
            None
        );
        assert_eq!(
            wall(facts("Sign in", 200, 2, &["recaptcha"], "sign in")),
            None
        );
        assert_eq!(
            wall(facts("", 30, 0, &["recaptcha"], "")),
            Some("reCAPTCHA")
        );
        assert_eq!(wall(facts("", 30, 0, &["hcaptcha"], "")), Some("hCaptcha"));
        assert_eq!(wall(facts("", 10, 0, &["dd"], "")), Some("DataDome"));
        assert_eq!(wall(facts("Shop", 9000, 1, &["dd-weak"], "")), None);
        assert_eq!(
            wall(facts("", 50, 0, &["px"], "")),
            Some("PerimeterX (HUMAN)")
        );
        assert_eq!(
            wall(facts("", 50, 0, &["imperva"], "")),
            Some("Imperva (Incapsula)")
        );
        assert_eq!(
            wall(facts("Access Denied", 200, 0, &["akamai"], "access denied")),
            Some("Akamai")
        );
        assert_eq!(wall(facts("", 50, 0, &["awswaf"], "")), Some("AWS WAF"));
        assert_eq!(
            wall(facts("403 Forbidden", 40, 0, &[], "403 forbidden")),
            Some("an unidentified bot filter or block page")
        );
        // A long article about access denied pages is no block page.
        assert_eq!(
            wall(facts("Access denied errors explained", 8000, 0, &[], "")),
            None
        );
        assert_eq!(wall(facts("Home", 3000, 1, &[], "welcome")), None);
    }

    #[test]
    fn wall_facts_are_read_from_the_page_answer() {
        let answer = |text: &str| json!({ "content": [{ "type": "text", "text": text }] });
        let escaped = r#"### Result
"BDWALL:{\"t\":\"Just a moment...\",\"n\":40,\"i\":0,\"m\":[\"cf-weak\"],\"x\":\"\"}:BDEND"
### Ran Playwright code"#;
        let f = parse_wall_facts(&answer(escaped)).unwrap();
        assert_eq!(f.title, "Just a moment...");
        assert_eq!(f.marks, vec!["cf-weak".to_string()]);
        assert_eq!(classify_wall(&f), Some("Cloudflare"));
        assert!(parse_wall_facts(&answer("\"BDWALL:null:BDEND\"")).is_none());
        assert!(parse_wall_facts(&answer("Error: boom")).is_none());
    }

    #[test]
    fn the_wall_note_names_the_vendor_and_the_human_option() {
        let with = challenge_note("Cloudflare", false, true);
        assert!(with.contains("Cloudflare") && with.contains("browser_ask_human"));
        let without = challenge_note("Cloudflare", false, false);
        assert!(!without.contains("browser_ask_human"));
        assert!(challenge_note("Cloudflare", true, true).contains("cleared by itself"));
        assert!(may_hit_wall("browser_navigate") && !may_hit_wall("browser_snapshot"));
    }

    #[test]
    fn har_secrets_are_masked() {
        assert!(
            secret_header("Authorization")
                && secret_header("X-Api-Key")
                && secret_header("Set-Cookie")
        );
        assert!(secret_header("x-amz-security-token") && secret_header("X-Client-Secret"));
        assert!(!secret_header("Content-Type") && !secret_header(":authority"));
        assert!(
            secret_param("access_token") && secret_param("code") && secret_param("X-Amz-Signature")
        );
        assert!(!secret_param("page") && !secret_param("q"));
        let (url, query) = mask_url("https://a.test/cb?code=abc123&state=ok&token=t#frag");
        assert_eq!(
            url,
            "https://a.test/cb?code=%5Bredacted%5D&state=ok&token=%5Bredacted%5D"
        );
        assert_eq!(query[0], json!({ "name": "code", "value": "[redacted]" }));
        assert_eq!(query[1], json!({ "name": "state", "value": "ok" }));
        assert_eq!(mask_url("https://a.test/x").1.len(), 0);
        let (headers, cookies) = har_headers(&json!([
            { "name": "Cookie", "value": "sid=abc; theme=dark" },
            { "name": "Accept", "value": "*/*" },
            { "name": ":path", "value": "/?token=x" },
            { "name": "Set-Cookie", "value": "a=1; Path=/\nb=2; HttpOnly" },
        ]));
        assert_eq!(headers[0]["value"], "[redacted]");
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[1]["value"], "*/*");
        let names: Vec<_> = cookies
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["sid", "theme", "a", "b"]);
        assert!(cookies.iter().all(|c| c["value"] == "[redacted]"));
        assert_eq!(
            har_body(
                r#"{"access_token":"x","code":404,"n":{"password":"p"}}"#,
                "application/json"
            )
            .unwrap(),
            r#"{"access_token":"[redacted]","code":404,"n":{"password":"[redacted]"}}"#
        );
        assert!(har_body(r#"{"access_token":"x"#, "application/json").is_none());
        assert_eq!(har_body("<p>hi</p>", "text/html").unwrap(), "<p>hi</p>");
    }

    #[test]
    fn a_har_is_built_from_the_collected_entries() {
        let raw = json!({ "entries": [{
            "url": "https://a.test/api?key=k1&x=1", "method": "GET", "type": "fetch",
            "start": "2026-11-11T09:30:05.000Z", "t0": 1000.0, "t1": 1100.0, "post": 0, "status": 200,
            "statusText": "OK", "mime": "application/json",
            "reqH": [{ "name": "Authorization", "value": "Bearer s3cr3t" }],
            "resH": [{ "name": "Content-Type", "value": "application/json" }, { "name": "Set-Cookie", "value": "sid=zzz" }],
            "timing": { "startTime": 1.0, "domainLookupStart": -1, "domainLookupEnd": -1, "connectStart": 5, "connectEnd": 9,
                "secureConnectionStart": 6, "requestStart": 10, "responseStart": 30, "responseEnd": 40 },
            "body": "{\"ok\":true,\"token\":\"t\"}", "bodySize": 24
        }, {
            "url": "https://b.test/", "method": "POST", "type": "document", "start": "x", "t0": 1.0, "t1": 3.0,
            "post": 10, "status": 0, "error": "net::ERR_FAILED",
            "reqH": [{ "name": "Content-Type", "value": "application/x-www-form-urlencoded" }]
        }]});
        let (har, bodies) = build_har(&raw);
        assert_eq!(bodies, 1);
        let entries = har["log"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let e = &entries[0];
        assert_eq!(
            e["request"]["url"],
            "https://a.test/api?key=%5Bredacted%5D&x=1"
        );
        assert_eq!(e["request"]["headers"][0]["value"], "[redacted]");
        assert_eq!(
            e["response"]["content"]["text"],
            r#"{"ok":true,"token":"[redacted]"}"#
        );
        assert_eq!(
            e["response"]["cookies"][0],
            json!({ "name": "sid", "value": "[redacted]" })
        );
        assert_eq!(e["time"], 40.0);
        assert_eq!(e["timings"]["wait"], 20.0);
        assert_eq!(e["timings"]["connect"], 4.0);
        assert_eq!(e["timings"]["dns"], -1.0);
        let failed = &entries[1];
        assert_eq!(failed["_error"], "net::ERR_FAILED");
        assert_eq!(failed["time"], 2.0);
        assert_eq!(failed["request"]["postData"]["text"], "");
        assert!(!har.to_string().contains("s3cr3t") && !har.to_string().contains("zzz"));
    }

    #[test]
    fn marked_answers_survive_the_escaping_of_the_result() {
        let answer = |text: &str| json!({ "content": [{ "type": "text", "text": text }] });
        // As JSON string, followed by the echoed script, which contains the marker again.
        let a = answer(
            "\"BDHAR:{\\\"a\\\":\\\"q\\\\n\\\"}:BDEND\"\n```js\n'BDHAR:' + x + ':BDEND'\n```",
        );
        assert_eq!(parse_marked(&a, "BDHAR").unwrap(), json!({ "a": "q\n" }));
        // Printed raw.
        assert_eq!(
            parse_marked(&answer("BDHAR:{\"b\":1}:BDEND"), "BDHAR").unwrap(),
            json!({ "b": 1 })
        );
        assert!(parse_marked(&answer("nothing"), "BDHAR").is_none());
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_794_389_405), "20261111T093005Z");
        assert_eq!(utc_stamp(951_782_400 + 86_399), "20000229T235959Z");
    }

    #[test]
    fn network_har_and_flight_recorder_tools_are_always_offered() {
        let names: Vec<String> = own_tools(&[], &[], false, &[])
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"browser_network_har".to_string()));
        assert!(names.contains(&"browser_flight_recorder".to_string()));
    }

    #[test]
    fn the_conversation_id_comes_from_the_agents_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
        };
        assert_eq!(conversation_id(env(&[])), None);
        assert_eq!(
            conversation_id(env(&[
                ("CODEX_SESSION_ID", "c3"),
                ("CODEX_THREAD_ID", "c2")
            ])),
            Some("c2".into())
        );
        assert_eq!(
            conversation_id(env(&[
                ("CODEX_THREAD_ID", "c2"),
                ("CLAUDE_CODE_SESSION_ID", " c1 ")
            ])),
            Some("c1".into())
        );
        assert_eq!(
            conversation_id(env(&[
                ("CLAUDE_CODE_SESSION_ID", " "),
                ("CODEX_THREAD_ID", "c2")
            ])),
            Some("c2".into())
        );
    }

    #[test]
    fn links_are_built_from_the_node_address() {
        assert_eq!(
            handoff_url("https://b.test/", "AB12"),
            "https://b.test/handoff/?page=AB12"
        );
        assert_eq!(
            recording_url("https://b.test", "1b2c"),
            "https://b.test/#recordings/1b2c"
        );
    }

    #[test]
    fn public_links_expire_and_never_last_forever() {
        assert_eq!(parse_expiry(None), Ok(3600));
        assert_eq!(parse_expiry(Some("")), Ok(3600));
        assert_eq!(parse_expiry(Some("30m")), Ok(1800));
        assert_eq!(parse_expiry(Some("2H")), Ok(7200));
        assert_eq!(parse_expiry(Some("7d")), Ok(7 * 86400));
        assert_eq!(parse_expiry(Some("90")), Ok(90));
        assert!(parse_expiry(Some("8d")).is_err());
        assert!(parse_expiry(Some("never")).is_err());
        assert!(parse_expiry(Some("0")).is_err());
        assert!(parse_expiry(Some("10s")).is_err());
        assert!(parse_expiry(Some("1w")).is_err());
        assert_eq!(describe_secs(3600), "1 hour(s)");
        assert_eq!(describe_secs(1800), "30 minute(s)");
    }

    #[test]
    fn sharing_tools_need_the_flag_and_every_tool_is_annotated() {
        let all = |share: bool| {
            own_tools_with(
                &["e".into()],
                &["x.test".into()],
                true,
                &["x.test".into()],
                true,
                share,
            )
        };
        let name = |t: &Value| t["name"].as_str().unwrap().to_string();
        let off: Vec<String> = all(false).iter().map(name).collect();
        assert!(!off.contains(&"browser_share".to_string()));
        assert!(!off.contains(&"browser_unshare".to_string()));
        let on = all(true);
        let names: Vec<String> = on.iter().map(name).collect();
        for want in [
            "browser_share",
            "browser_unshare",
            "browser_set_topic",
            "browser_live_link",
            "browser_recording",
            "browser_fill_card",
        ] {
            assert!(names.contains(&want.to_string()), "{want}");
        }
        for tool in &on {
            let n = name(tool);
            let a = &tool["annotations"];
            assert!(a["title"].is_string(), "{n} title");
            assert!(a["readOnlyHint"].is_boolean(), "{n} readOnlyHint");
            assert!(a["openWorldHint"].is_boolean(), "{n} openWorldHint");
            if a["readOnlyHint"] == false {
                assert!(a["destructiveHint"].is_boolean(), "{n} destructiveHint");
                assert!(a["idempotentHint"].is_boolean(), "{n} idempotentHint");
            }
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{n}");
            assert!(tool["description"].as_str().unwrap().len() > 20, "{n}");
        }
        let share = on.iter().find(|t| name(t) == "browser_share").unwrap();
        assert!(share["description"].as_str().unwrap().contains("PUBLIC"));
        assert_eq!(
            on.iter().find(|t| name(t) == "browser_live_link").unwrap()["annotations"]
                ["readOnlyHint"],
            true
        );
    }

    #[test]
    fn censor_arguments_default_to_everything_but_emails() {
        let a = parse_censor_args(&json!({})).unwrap();
        assert_eq!(a.cats, Censor::DEFAULT.to_vec());
        assert!(!a.cats.contains(&Censor::Emails));
        assert!(a.png && !a.full_page && a.target.is_none());
        let a = parse_censor_args(&json!({
            "censor": ["emails", "passwords", "emails"], "fullPage": true, "type": "jpeg"
        }))
        .unwrap();
        assert_eq!(a.cats, vec![Censor::Emails, Censor::Passwords]);
        assert!(a.full_page && !a.png);
        // An element screenshot is never full page; `ref` is the old name of `target`.
        let a = parse_censor_args(&json!({ "ref": " e12 ", "fullPage": true })).unwrap();
        assert_eq!(a.target.as_deref(), Some("e12"));
        assert!(!a.full_page);
        assert!(parse_censor_args(&json!({ "censor": [] })).is_err());
        assert!(parse_censor_args(&json!({ "censor": ["faces"] })).is_err());
        assert!(parse_censor_args(&json!({ "censor": "passwords" })).is_err());
    }

    #[test]
    fn censor_categories_become_selectors() {
        let pw = censor_selectors(&[Censor::Passwords]);
        assert_eq!(pw, vec!["input[type=password]".to_string()]);
        let cards = censor_selectors(&[Censor::Cards]);
        assert!(cards.contains(&"[autocomplete^=cc-]".to_string()));
        assert!(cards.contains(&"input[name*=\"cvc\" i]".to_string()));
        let otp = censor_selectors(&[Censor::Otp]);
        assert!(otp.contains(&"[autocomplete=one-time-code]".to_string()));
        assert!(otp.contains(&"input[id*=\"totp\" i]".to_string()));
        // Secrets, inputs and emails are found by their content, not by selector.
        assert!(censor_selectors(&[Censor::Secrets, Censor::Inputs, Censor::Emails]).is_empty());
        let a = parse_censor_args(&json!({ "censor": ["inputs", "emails", "cards"] })).unwrap();
        let plan = censor_plan(&a, &[3, 9]);
        assert_eq!(plan["emails"], true);
        assert_eq!(plan["frames"], true);
        assert_eq!(plan["ids"], json!([3, 9]));
        assert!(plan["filled"].as_str().unwrap().contains("textarea"));
        let plan = censor_plan(
            &parse_censor_args(&json!({ "censor": ["passwords"] })).unwrap(),
            &[],
        );
        assert!(plan["filled"].is_null() && plan["frames"] == false);
    }

    #[test]
    fn elements_holding_a_secret_are_found_by_their_text() {
        let rows = json!([
            [0, "Welcome"],
            [1, "the code is SuperSecret123 ok"],
            [2, "x"],
            [5, "c3VwZXJzZWNyZXQxMjM="]
        ]);
        let filled = vec!["SuperSecret123".to_string(), "ab".to_string()];
        assert_eq!(secret_element_ids(&rows, &filled), vec![1]);
        assert!(secret_element_ids(&rows, &["ab".to_string()]).is_empty());
        assert!(secret_element_ids(&rows, &[]).is_empty());
    }

    #[test]
    fn the_censored_screenshot_tool_is_always_offered() {
        let tools = own_tools(&[], &[], false, &[]);
        let t = tools
            .iter()
            .find(|t| t["name"] == "browser_screenshot_censored")
            .unwrap();
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert!(t["inputSchema"]["properties"]["censor"].is_object());
    }

    #[test]
    fn shared_screenshots_are_censored_when_asked_or_forced() {
        let filled = vec!["SuperSecret123".to_string(), "ab".to_string()];
        assert_eq!(shared_censor(None, false, &filled), Ok(None));
        let (cats, secrets) = shared_censor(None, true, &filled).unwrap().unwrap();
        assert_eq!(cats, ["passwords", "cards", "otp", "secrets", "inputs"]);
        assert_eq!(secrets, Some(vec!["SuperSecret123".to_string()]));
        // An empty list cannot turn it off when forced, nor asks for it when not.
        assert!(shared_censor(Some(&json!([])), true, &filled)
            .unwrap()
            .is_some());
        assert_eq!(shared_censor(Some(&json!([])), false, &filled), Ok(None));
        let (cats, secrets) = shared_censor(Some(&json!(["emails"])), false, &filled)
            .unwrap()
            .unwrap();
        assert_eq!(cats, ["emails"]);
        assert_eq!(secrets, None);
        let (cats, _) = shared_censor(Some(&json!(["emails"])), true, &filled)
            .unwrap()
            .unwrap();
        assert_eq!(cats.len(), 6);
        assert!(shared_censor(Some(&json!(["faces"])), true, &filled).is_err());
    }
}
