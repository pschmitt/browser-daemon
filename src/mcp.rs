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

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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
browser_status lists the profiles you may open and the nodes. Switching closes the current browser. \
Never type a password, token or TOTP code yourself: use browser_fill_secret when it is listed.";

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

/// A running Playwright MCP.
struct Child {
    stdin: Mutex<ChildStdin>,
    proc: Mutex<Proc>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next: AtomicU64,
    tools: std::sync::Mutex<Vec<Value>>,
    target: Target,
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

fn own_tools(secrets: &[String]) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "browser_status",
            "description": "What this browser can use: the browser in use now, the profiles you may open (saved logins and cookies) and the nodes with their load.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "title": "Browser status", "readOnlyHint": true }
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
        let mut tools = own_tools(&self.allow_secrets);
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
        let mut result = self.tools_call_raw(params).await;
        // Whatever comes back, results and errors alike, goes through the mask.
        let filled = self.filled.lock().unwrap().clone();
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
            "browser_status" => Ok(match self.status().await {
                Ok(text) => text_result(text, false),
                Err(e) => text_result(e, true),
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
            _ => {
                let child = self.child_or_default().await.map_err(|e| (-32000, e))?;
                child
                    .request("tools/call", params, Duration::from_secs(600))
                    .await
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
        Ok(serde_json::to_string_pretty(&json!({
            "current": current,
            "profiles_you_may_open": profiles,
            "other_profiles": format!("{hidden} more exist; the user has not allowed agents to open them"),
            "nodes": nodes,
        }))
        .unwrap_or_default())
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
        let ov = self.overview().await?;
        let target = match profile {
            Some(name) => {
                if !allows(&self.allow, name) {
                    return Err(format!(
                        "Profile '{name}' is not open to agents here. Allowed: {}.",
                        allowed(&self.allow)
                    ));
                }
                let hits: Vec<_> = ov
                    .profiles
                    .iter()
                    .filter(|p| p.profile.name == name && node.is_none_or(|n| p.node_name == n))
                    .collect();
                let [p] = hits.as_slice() else {
                    return Err(if hits.is_empty() {
                        format!(
                            "No profile '{name}'{}",
                            node.map(|n| format!(" on {n}")).unwrap_or_default()
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
        let child = self.spawn_child(&target).await?;
        let old = self.child.lock().await.replace(child);
        if let Some(old) = old {
            old.stop().await;
        }
        Ok(format!(
            "Now using {}. It starts when you first use a browser tool; the previous browser was closed.",
            target.describe()
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
            loop {
                let params = cursor
                    .as_ref()
                    .map_or(json!({}), |c| json!({ "cursor": c }));
                let page = child
                    .request("tools/list", params, Duration::from_secs(30))
                    .await?;
                tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
                cursor = page["nextCursor"].as_str().map(str::to_string);
                if cursor.is_none() {
                    break;
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
        assert!(!names(own_tools(&[])).contains(&"browser_fill_secret".to_string()));
        assert!(
            names(own_tools(&["Frigate Plus".into()])).contains(&"browser_fill_secret".to_string())
        );
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
