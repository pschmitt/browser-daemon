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

    /// A site (relying party, with its subdomains) agents may sign in to with a vault passkey through browser_passkey (repeat for several, `*` for all).
    #[arg(long = "allow-passkey")]
    pub allow_passkeys: Vec<String>,

    /// Program browser_ask_human runs to ask the user for help, called as `PROGRAM MESSAGE` with BD_HANDOFF_URL (the live view of the agent's page), BD_PAGE, BD_NODE, BD_NODE_URL, BD_TITLE and BD_AGENT (the MCP client's name) in its environment.
    #[arg(long)]
    pub ask_human_cmd: Option<PathBuf>,

    /// A site (with its subdomains) whose login agents may export with browser_export_auth or use with browser_fetch (repeat for several, `*` for all).
    #[arg(long = "allow-export")]
    pub allow_exports: Vec<String>,

    /// rbw binary, for browser_fill_secret.
    #[arg(long, default_value = "rbw")]
    pub rbw: PathBuf,

    /// Program that runs Playwright MCP on a CDP endpoint, called as `PROGRAM ENDPOINT OUTPUT_DIR`.
    #[arg(long, default_value = "browser-mcp")]
    pub playwright_cmd: PathBuf,

    /// Where Playwright MCP keeps its output (default: ~/.cache/playwright-mcp/browser).
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
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
anything else only a person may do, call browser_ask_human when it is listed.";

/// A node and profile the browser runs on.
#[derive(Clone)]
struct Target {
    node: String,
    /// The node's HTTP(S) base address.
    url: String,
    ws_base: String,
    profile: Option<String>,
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
fn cdp_endpoint(target: &Target, token: Option<&str>) -> String {
    let mut query = Vec::new();
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
    allow_passkeys: Vec<String>,
    ask_human_cmd: Option<PathBuf>,
    allow_exports: Vec<String>,
    /// Files browser_export_auth wrote, removed when they expire or on exit.
    exports: std::sync::Mutex<Vec<PathBuf>>,
    rbw: PathBuf,
    /// Secrets filled in this session, masked in every tool result.
    filled: std::sync::Mutex<Vec<String>>,
    cmd: PathBuf,
    out_dir: PathBuf,
    version: Mutex<String>,
    /// What the agent's MCP client calls itself (`clientInfo.name` of its initialize).
    client_name: Mutex<String>,
    child: Mutex<Option<Arc<Child>>>,
    stdout: Mutex<tokio::io::Stdout>,
}

pub async fn run(args: McpArgs) -> R<()> {
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
        allow_passkeys: args.allow_passkeys,
        ask_human_cmd: args.ask_human_cmd,
        allow_exports: args.allow_exports,
        exports: std::sync::Mutex::new(Vec::new()),
        rbw: args.rbw,
        filled: std::sync::Mutex::new(Vec::new()),
        cmd: args.playwright_cmd,
        out_dir,
        version: Mutex::new("2025-06-18".into()),
        client_name: Mutex::new(String::new()),
        child: Mutex::new(None),
        stdout: Mutex::new(tokio::io::stdout()),
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

fn own_tools(
    secrets: &[String],
    passkeys: &[String],
    ask_human: bool,
    exports: &[String],
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
                    "node": { "type": "string", "description": "Node to run a throwaway browser on (a profile lives on one node)." }
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
            "params": {"type": "object"}
        }, "required": ["browser", "tab", "action"], "additionalProperties": false},
        "annotations": {"title": "Shared browser action", "readOnlyHint": false}
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
            "tools/list" => Ok(self.tools_list().await),
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

    async fn tools_list(&self) -> Value {
        let mut tools = own_tools(
            &self.allow_secrets,
            &self.allow_passkeys,
            self.ask_human_cmd.is_some(),
            &self.allow_exports,
        );
        match self.child_or_default().await {
            Ok(child) => tools.extend(child.tools.lock().unwrap().iter().cloned()),
            Err(e) => eprintln!("browser-daemon mcp: no browser tools yet: {e}"),
        }
        json!({ "tools": tools })
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
                Ok(match self.fill_secret(&args).await {
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
            "browser_ask_human" if self.ask_human_cmd.is_some() => {
                Ok(match self.ask_human(&args).await {
                    Ok(text) => text_result(text, false),
                    Err(e) => text_result(e, true),
                })
            }
            _ => {
                let is_browser_tabs = name == "browser_tabs";
                let child = self.child_or_default().await.map_err(|e| (-32000, e))?;
                let first =
                    child.target.profile.is_some() && !child.used.swap(true, Ordering::SeqCst);
                let mut result = child
                    .request("tools/call", params, Duration::from_secs(600))
                    .await?;
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

    async fn fill_secret(&self, args: &Value) -> Result<String, String> {
        let arg = |k: &str| args[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        let entry = arg("entry").ok_or("Name the vault entry (`entry`)")?;
        let target = arg("target").ok_or("Name the field to type into (`target`)")?;
        let field = arg("field").unwrap_or("password");
        if !allows(&self.allow_secrets, entry) {
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

        let secret = secrets::lookup(&self.rbw, entry, arg("user"), field)
            .await
            .map_err(|e| self.mask(&e))?;
        // Masked from here on, whatever fails later.
        {
            let mut filled = self.filled.lock().unwrap();
            filled.push(secret.value.clone());
            if let Some(ref user) = secret.username {
                // HTTP Basic auth carries both, base64-encoded.
                filled.push(format!("{user}:{}", secret.value));
            }
        }
        if !secrets::url_matches(&address, &secret.uris) {
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
        let submit = args["submit"].as_bool().unwrap_or(false);
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
        let link = format!("{node_url}/handoff/?page={}", url_component(&page_id));
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
        self.extension_request(&format!("{prefix}/extension/nodes/{browser}/command"), Some(json!({
            "tab": tab, "action": action, "params": args.get("params").cloned().unwrap_or(json!({})),
        }))).await
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
            .map(|n| json!({ "node": n.0, "sessions": format!("{}/{}", n.2, n.3) }))
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
        let profile = args["profile"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let node = args["node"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut ov = self.overview().await?;
        let target = match profile {
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
                        .filter(|p| p.profile.name == name && node.is_none_or(|n| p.node_name == n))
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
                        format!("Profile '{name}' exists on several nodes: name one with `node`")
                    });
                };
                Target {
                    node: p.node_name.clone(),
                    url: p.node_url.clone(),
                    ws_base: Self::ws_base(&p.node_url),
                    profile: Some(name.to_string()),
                }
            }
            None => {
                let (node, url) = Self::pick_node(&ov, node)?;
                Target {
                    node,
                    ws_base: Self::ws_base(&url),
                    url,
                    profile: None,
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
        Ok(format!(
            "Now using {}. It starts when you first use a browser tool; the previous browser was closed.{}",
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
        let endpoint = cdp_endpoint(target, token.as_deref());
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
        };
        assert_eq!(cdp_endpoint(&t(None), None), "wss://b.test/chromium");
        assert_eq!(
            cdp_endpoint(&t(None), Some("123.abc")),
            "wss://b.test/chromium?auth=123.abc"
        );
        assert_eq!(
            cdp_endpoint(&t(Some("my work")), Some("123.abc")),
            "wss://b.test/chromium?profile=my+work&auth=123.abc"
        );
        assert_eq!(
            cdp_endpoint(&t(Some("work")), None),
            "wss://b.test/chromium?profile=work"
        );
    }
}
