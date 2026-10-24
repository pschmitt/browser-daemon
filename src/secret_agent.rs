//! `browser-daemon secret-agent`: runs as the vault's user and serves the
//! live views' vault from rbw: the passkeys approved in them (see passkeys.rs,
//! `rbw passkey inject`/`assert`), the logins their vault button lists and the
//! one picked there (see autofill.rs), unlocking rbw from the phone if needed.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use crate::passkeys::{Outcome, Request};
use crate::secrets;

#[derive(clap::Args, Debug)]
pub struct SecretAgentArgs {
    /// Base URL of a node to serve (repeat for several), e.g. https://browser.fnuc.nb.brkn.lol
    #[arg(long = "node", required = true)]
    pub nodes: Vec<String>,
    /// File holding the mesh's cluster secret.
    #[arg(long)]
    pub cluster_secret_file: PathBuf,
    /// rbw binary.
    #[arg(long, default_value = "rbw")]
    pub rbw: PathBuf,
    /// Webhook asking for an rbw unlock on the phone (POST {"host", "agent"}).
    #[arg(long)]
    pub unlock_webhook: Option<String>,
    /// Host name the unlock webhook should unlock rbw on.
    #[arg(long, default_value = "fnuc")]
    pub unlock_host: String,
    /// Webhook told about every login filled into a site the entry is not saved
    /// for (POST {"message": "..."}). The fill goes ahead; this is the notice.
    #[arg(long)]
    pub notify_webhook: Option<String>,
    #[arg(skip)]
    seen: std::sync::Mutex<std::collections::VecDeque<uuid::Uuid>>,
    /// The rbw account of each entry a lookup listed (with several accounts,
    /// `rbw get` and `rbw code` need to be told).
    #[arg(skip)]
    accounts: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

/// How long one rbw call may take.
const RBW_WAIT: Duration = Duration::from_secs(60);
/// Longest a request may take, the wait for a phone unlock included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(420);
/// Request ids remembered, so a node cannot make the agent serve one twice.
const SEEN_IDS: usize = 512;

/// Whether the request's fields are plain values the vault tools can take:
/// nothing a node sends reaches rbw as an option or a path.
fn valid_request(req: &Request) -> Result<(), String> {
    let id_like = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    let host_like = |s: &str| {
        !s.is_empty()
            && s.len() <= 253
            && !s.starts_with(['-', '.'])
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '[' | ']'))
    };
    if !id_like(&req.page_id) {
        return Err("Bad page id".into());
    }
    if !host_like(&req.rp_id) {
        return Err("Bad host".into());
    }
    if req.entry.as_deref().is_some_and(|e| !id_like(e)) {
        return Err("Bad entry id".into());
    }
    if req
        .challenge
        .as_deref()
        .is_some_and(|c| !id_like(c) || c.len() > 1024)
    {
        return Err("Bad challenge".into());
    }
    if req.allow.iter().any(|a| !id_like(a)) || req.allow.len() > 64 {
        return Err("Bad credential id".into());
    }
    let web = |o: &str| url::Url::parse(o).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    if req.origin.as_deref().is_some_and(|o| !web(o)) {
        return Err("Bad origin".into());
    }
    // The page's address goes to rbw as a value of `--url`, never as an option.
    if req
        .url
        .as_deref()
        .is_some_and(|u| !web(u) || u.len() > 2048)
    {
        return Err("Bad page address".into());
    }
    if req
        .fields
        .iter()
        .any(|f| !matches!(f.as_str(), "username" | "password" | "totp"))
    {
        return Err("Bad field".into());
    }
    Ok(())
}

impl SecretAgentArgs {
    /// Notes a request id; false when it was seen before.
    fn remember(&self, id: uuid::Uuid) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.contains(&id) {
            return false;
        }
        if seen.len() >= SEEN_IDS {
            seen.pop_front();
        }
        seen.push_back(id);
        true
    }
}

pub async fn run(args: SecretAgentArgs) -> Result<(), Box<dyn std::error::Error>> {
    let secret = tokio::fs::read_to_string(&args.cluster_secret_file)
        .await?
        .trim()
        .to_string();
    let args = std::sync::Arc::new(args);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut tasks = Vec::new();
    for node in &args.nodes {
        let (node, secret, client, args) = (
            node.trim_end_matches('/').to_string(),
            secret.clone(),
            client.clone(),
            args.clone(),
        );
        tasks.push(tokio::spawn(async move {
            poll(node, secret, client, args).await
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

async fn poll(
    node: String,
    secret: String,
    client: reqwest::Client,
    args: std::sync::Arc<SecretAgentArgs>,
) {
    info!(node = %node, "Serving the vault");
    loop {
        let res = client
            .get(format!("{node}/api/secret-agent/next"))
            .header("X-Browser-Cluster-Secret", &secret)
            .send()
            .await;
        let req = match res {
            Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => continue,
            Ok(r) if r.status().is_success() => match r.json::<Request>().await {
                Ok(req) => req,
                Err(e) => {
                    warn!(node = %node, error = %e, "Bad request from node");
                    continue;
                }
            },
            Ok(r) => {
                warn!(node = %node, status = %r.status(), "Node refused the poll");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Err(e) => {
                warn!(node = %node, error = %e.without_url(), "Node unreachable");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        info!(node = %node, rp_id = %req.rp_id, page = %req.page_id, kind = %req.kind, "Passkey requested");
        let (node, secret, client, args) =
            (node.clone(), secret.clone(), client.clone(), args.clone());
        let replay = !args.remember(req.id);
        tokio::spawn(async move {
            let work = async {
                if replay {
                    return Err("This request was served already".to_string());
                }
                valid_request(&req)?;
                match req.kind.as_str() {
                    "find" => find(&req, &args).await.map(Some),
                    "logins" => logins(&node, &secret, &req, &client, &args).await.map(Some),
                    "fill" => fill(&node, &secret, &req, &client, &args).await.map(Some),
                    "copy" => copy(&node, &secret, &req, &client, &args).await.map(Some),
                    "assert" => assert(&node, &secret, &req, &client, &args).await.map(Some),
                    "inject" => serve(&node, &secret, &req, &client, &args)
                        .await
                        .map(|()| None),
                    other => Err(format!("Unknown request kind '{other}'")),
                }
            };
            let result = tokio::time::timeout(REQUEST_TIMEOUT, work)
                .await
                .unwrap_or_else(|_| Err("The request took too long".to_string()));
            let outcome = match result {
                Ok(info) => Outcome {
                    ok: true,
                    error: None,
                    info,
                },
                Err(e) => {
                    warn!(rp_id = %req.rp_id, error = %e, "Passkey not served");
                    Outcome {
                        ok: false,
                        error: Some(e),
                        info: None,
                    }
                }
            };
            let _ = client
                .post(format!("{node}/api/secret-agent/{}", req.id))
                .header("X-Browser-Cluster-Secret", &secret)
                .json(&outcome)
                .send()
                .await;
        });
    }
}

/// Which vault entry would serve `rp_id` (name and user only), or that the vault
/// is locked. Never asks for an unlock: that only happens once the user approves.
async fn find(req: &Request, args: &SecretAgentArgs) -> Result<serde_json::Value, String> {
    if !unlocked(&args.rbw).await {
        return Ok(serde_json::json!({ "locked": true }));
    }
    let out = tokio::time::timeout(
        RBW_WAIT,
        Command::new(&args.rbw)
            .args(["passkey", "find", "--rp-id", &req.rp_id])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "rbw did not answer in time".to_string())?
    .map_err(|e| format!("Could not run rbw: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("rbw failed")
            .trim()
            .to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("Unexpected rbw output: {e}"))
}

async fn unlocked(rbw: &PathBuf) -> bool {
    tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(rbw)
            .arg("unlocked")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|s| s.is_ok_and(|s| s.success()))
}

/// Ready once `rbw passkey inject` has the passkey in the page; rbw keeps
/// running in the background until the assertion and the vault sync are done.
/// rbw unlocked, asking the phone first (and telling the view so) if needed.
async fn ensure_unlocked(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<(), String> {
    if !unlocked(&args.rbw).await {
        let hook = args.unlock_webhook.as_ref().ok_or("The vault is locked")?;
        client
            .post(hook)
            .timeout(Duration::from_secs(15))
            .json(&serde_json::json!({ "host": args.unlock_host, "agent": format!("{} for {}", if matches!(req.kind.as_str(), "logins" | "fill" | "copy") { "Login" } else { "Passkey" }, req.rp_id) }))
            .send()
            .await
            .map_err(|e| format!("Could not ask for an unlock: {}", e.without_url()))?;
        let _ = client
            .post(format!("{node}/api/secret-agent/{}/note", req.id))
            .header("X-Browser-Cluster-Secret", secret)
            .json(&serde_json::json!({ "note": "rbw is locked: confirm the unlock on your phone" }))
            .send()
            .await;
        let mut ok = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if unlocked(&args.rbw).await {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err("The vault was not unlocked in time".into());
        }
    }
    Ok(())
}

/// Sign the page's request (rbw passkey assert); the view hands the assertion to
/// the page, which answers navigator.credentials.get() with it. Any browser.
async fn assert(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<serde_json::Value, String> {
    let origin = req.origin.as_deref().ok_or("No page origin")?;
    let challenge = req.challenge.as_deref().ok_or("No challenge")?;
    ensure_unlocked(node, secret, req, client, args).await?;
    let mut cmd = Command::new(&args.rbw);
    cmd.args([
        "passkey",
        "assert",
        "--rp-id",
        &req.rp_id,
        "--origin",
        origin,
        "--challenge",
        challenge,
    ]);
    for id in &req.allow {
        cmd.args(["--allow", id]);
    }
    cmd.args(req.entry.as_deref());
    let out = tokio::time::timeout(
        RBW_WAIT,
        cmd.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "rbw did not answer in time".to_string())?
    .map_err(|e| format!("Could not run rbw: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("rbw failed")
            .trim()
            .to_string());
    }
    info!(rp_id = %req.rp_id, "Passkey assertion signed");
    serde_json::from_slice(&out.stdout).map_err(|e| format!("Unexpected rbw output: {e}"))
}

async fn serve(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<(), String> {
    ensure_unlocked(node, secret, req, client, args).await?;
    // A picked entry (its vault id) narrows the search; otherwise rbw finds the one
    // entry holding a passkey for the site.
    let entry: Vec<&str> = req.entry.as_deref().into_iter().collect();
    let mut child = Command::new(&args.rbw)
        .args([
            "passkey",
            "inject",
            "--rp-id",
            &req.rp_id,
            "--cdp-url",
            node,
            "--page-id",
            &req.page_id,
            // The page only goes on once the passkey is in place, so its assertion
            // follows within seconds; a stuck attempt must not lock the entry long.
            "--timeout",
            "30",
        ])
        .args(&entry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("Could not run rbw: {e}"))?;
    let mut out = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let mut err = BufReader::new(child.stderr.take().ok_or("no stderr")?).lines();
    let rp = req.rp_id.clone();
    // rbw's errors name the problem (no passkey, ambiguous entry), never the key.
    let mut last_err = String::new();
    let mut err_open = true;
    loop {
        tokio::select! {
            line = out.next_line() => match line {
                Ok(Some(l)) if l.starts_with("Passkey ready") => {
                    info!(rp_id = %rp, "Passkey ready in the page");
                    // The rest (assertion, counter sync) finishes on its own.
                    tokio::spawn(async move {
                        while let Ok(Some(l)) = out.next_line().await {
                            info!(rp_id = %rp, "rbw: {l}");
                        }
                        let _ = child.wait().await;
                    });
                    return Ok(());
                }
                Ok(Some(_)) => {}
                _ => break,
            },
            line = err.next_line(), if err_open => match line {
                Ok(Some(l)) if !l.trim().is_empty() => last_err = l.trim().to_string(),
                Ok(Some(_)) => {}
                _ => err_open = false,
            },
        }
    }
    while let Ok(Some(l)) = err.next_line().await {
        if !l.trim().is_empty() {
            last_err = l.trim().to_string();
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    Err(if last_err.is_empty() {
        format!("rbw ended before the passkey was ready ({status})")
    } else {
        last_err
    })
}

/// At most this many logins are listed.
const MAX_RESULTS: usize = 50;

/// rbw's answer for the page's site (`rbw search --url`, rbw's URI matching,
/// as its browser extension offers logins): the logins saved for it or, with
/// a query, those matching it, the site's first. Metadata only, no secrets;
/// one batched decrypt in rbw-agent.
async fn rbw_logins(
    args: &SecretAgentArgs,
    req: &Request,
    query: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    // A node from before page addresses only names the host.
    let url = req
        .url
        .clone()
        .unwrap_or_else(|| format!("https://{}/", req.rp_id));
    let mut cmd = vec!["search", "--url", url.as_str(), "--json"];
    if let Some(query) = query {
        cmd.extend(["--", query]);
    }
    let found: Vec<serde_json::Value> = match secrets::rbw(&args.rbw, &cmd).await {
        Ok(out) => serde_json::from_str(&out).map_err(|_| "rbw: unexpected output".to_string())?,
        Err(e) if e.contains("no entries found") => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut accounts = args.accounts.lock().unwrap();
    if accounts.len() > 10_000 {
        accounts.clear();
    }
    for e in &found {
        if let (Some(id), Some(account)) = (e["id"].as_str(), e["account"].as_str()) {
            accounts.insert(id.to_string(), account.to_string());
        }
    }
    Ok(found)
}

/// `rbw` arguments for the entry, with its account when a lookup named one.
fn for_entry<'a>(args: &'a SecretAgentArgs, cmd: &[&'a str], entry: &'a str) -> Vec<String> {
    let mut out: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
    if let Some(account) = args.accounts.lock().unwrap().get(entry) {
        out.extend(["--account".to_string(), account.clone()]);
    }
    out.push(entry.to_string());
    out
}

/// The login entries for the page's site (rp_id is its host), as rbw picks
/// them for its browser extension: name, user and whether there's a TOTP, no
/// secrets. A locked vault is reported, or unlocked first when the user asked.
async fn logins(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<serde_json::Value, String> {
    if req.unlock {
        ensure_unlocked(node, secret, req, client, args).await?;
    } else if !unlocked(&args.rbw).await {
        return Ok(serde_json::json!({ "locked": true }));
    }
    let found = rbw_logins(args, req, req.query.as_deref()).await?;
    Ok(found
        .into_iter()
        // Logins with something to fill (not only a passkey).
        .filter(|e| e["has_password"] == true || e["user"].as_str().is_some_and(|u| !u.is_empty()))
        .take(MAX_RESULTS)
        .map(|e| {
            serde_json::json!({
                "id": e["id"],
                "name": e["name"],
                "user": e["user"],
                "folder": e["folder"],
                "password": e["has_password"],
                "totp": e["has_totp"],
                "site": e["site"],
                // For its favicon in the view.
                "uri": e["uris"].as_array().into_iter().flatten().filter_map(|u| u.as_str()).find(|u| u.starts_with("https://") || u.starts_with("http://")),
            })
        })
        .collect())
}

/// Fills the picked entry into the page: read here, checked against the page's
/// site, then sent to the node, which puts it into the login form.
async fn fill(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<serde_json::Value, String> {
    let entry = req.entry.as_deref().ok_or("No entry")?;
    if uuid::Uuid::parse_str(entry).is_err() {
        return Err("Not a vault entry id".into());
    }
    ensure_unlocked(node, secret, req, client, args).await?;
    // Saved for the page's site? rbw decides, as for its browser extension
    // (the lookup also notes the entry's account).
    let saved = rbw_logins(args, req, None)
        .await?
        .iter()
        .any(|e| e["id"] == entry && e["site"] == true);
    let get = for_entry(args, &["get", "--raw"], entry);
    let get: Vec<&str> = get.iter().map(String::as_str).collect();
    let raw: serde_json::Value = serde_json::from_str(&secrets::rbw(&args.rbw, &get).await?)
        .map_err(|_| "rbw: unexpected output".to_string())?;
    let mut uris = secrets::uris(&raw);
    let mut any_site = false;
    if !saved {
        if !req.any_site {
            return Err(format!("This entry is not saved for {}", req.rp_id));
        }
        // Confirmed in the live view: only into the page's own site, no other frames
        // (the node narrows the URIs). The user hears about it.
        warn!(host = %req.rp_id, "Filling an entry not saved for the site, as confirmed");
        uris = Vec::new();
        any_site = true;
        notify_any_site(client, args, node, &raw, req).await;
    }
    let want = |f: &str| req.fields.iter().any(|x| x == f);
    let field = |f: &str| {
        want(f)
            .then(|| secrets::pick_field(&raw, f))
            .flatten()
            .filter(|v| !v.is_empty())
    };
    let totp = if want("totp") && raw["data"]["totp"].as_str().is_some_and(|t| !t.is_empty()) {
        let code = for_entry(args, &["code"], entry);
        let code: Vec<&str> = code.iter().map(String::as_str).collect();
        Some(secrets::rbw(&args.rbw, &code).await?.trim().to_string())
    } else {
        None
    };
    // Custom fields go along with a login fill (not a one-time-code-only one): text
    // ones as they are, linked ones as the login field they stand for. Hidden and
    // boolean ones never.
    let custom: Vec<serde_json::Value> = if want("username") || want("password") {
        custom_fields(&raw)
    } else {
        Vec::new()
    };
    let body = serde_json::json!({
        "request": req.id,
        "host": req.rp_id,
        "uris": uris,
        "any_site": any_site,
        "username": field("username"),
        "password": field("password"),
        "totp": totp,
        "custom": custom,
    });
    if body["username"].is_null()
        && body["password"].is_null()
        && body["totp"].is_null()
        && custom.is_empty()
    {
        return Err("The entry has nothing to fill".into());
    }
    let res = client
        .post(format!("{node}/api/pages/{}/autofill", req.page_id))
        .header("X-Browser-Cluster-Secret", secret)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Could not reach the node: {}", e.without_url()))?;
    let ok = res.status().is_success();
    let answer: serde_json::Value = res.json().await.unwrap_or_default();
    if !ok {
        return Err(answer["error"]
            .as_str()
            .unwrap_or("The node could not fill the page")
            .to_string());
    }
    info!(host = %req.rp_id, fields = %answer["filled"], "Login filled");
    Ok(answer)
}

/// Copies a value of the picked entry for the clipboard of the device viewing
/// (the live view writes it there): the first of the request's fields the
/// entry has, a TOTP as its current code. Unlike a fill it needs no site: the
/// user asked for the value itself. Only the field's name is logged.
async fn copy(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &SecretAgentArgs,
) -> Result<serde_json::Value, String> {
    let entry = req.entry.as_deref().ok_or("No entry")?;
    if uuid::Uuid::parse_str(entry).is_err() {
        return Err("Not a vault entry id".into());
    }
    ensure_unlocked(node, secret, req, client, args).await?;
    let get = for_entry(args, &["get", "--raw"], entry);
    let get: Vec<&str> = get.iter().map(String::as_str).collect();
    let raw: serde_json::Value = serde_json::from_str(&secrets::rbw(&args.rbw, &get).await?)
        .map_err(|_| "rbw: unexpected output".to_string())?;
    for field in &req.fields {
        let value = match field.as_str() {
            "totp" if raw["data"]["totp"].as_str().is_some_and(|t| !t.is_empty()) => {
                let code = for_entry(args, &["code"], entry);
                let code: Vec<&str> = code.iter().map(String::as_str).collect();
                Some(secrets::rbw(&args.rbw, &code).await?.trim().to_string())
            }
            "totp" => None,
            f => secrets::pick_field(&raw, f),
        };
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            info!(host = %req.rp_id, field = %field, "Vault value copied");
            return Ok(serde_json::json!({ "field": field, "value": value }));
        }
    }
    Err("The entry has nothing to copy".into())
}

/// The custom fields of an entry (`rbw get --raw`) worth filling, as name and
/// value. A linked field (rbw 2.22.13 and later say which, as `linked_id`) takes
/// the login's username (100) or password (101); other links are skipped.
fn custom_fields(raw: &serde_json::Value) -> Vec<serde_json::Value> {
    raw["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let name = f["name"].as_str().filter(|n| !n.is_empty())?;
            let value = match f["type"].as_str() {
                None | Some("text") => f["value"].as_str(),
                Some("linked") => match f["linked_id"].as_u64() {
                    Some(100) => raw["data"]["username"].as_str(),
                    Some(101) => raw["data"]["password"].as_str(),
                    _ => None,
                },
                _ => None,
            }
            .filter(|v| !v.is_empty())?;
            Some(serde_json::json!({"name": name, "value": value}))
        })
        .collect()
}

/// Tells the user (webhook) that an entry went into a site it is not saved for.
/// The message names the entry, the site and the fields, never a value.
async fn notify_any_site(
    client: &reqwest::Client,
    args: &SecretAgentArgs,
    node: &str,
    raw: &serde_json::Value,
    req: &Request,
) {
    let Some(hook) = args.notify_webhook.as_deref() else {
        return;
    };
    let message = any_site_message(
        raw["name"].as_str().unwrap_or("?"),
        &req.rp_id,
        &req.fields,
        node,
    );
    let sent = client
        .post(hook)
        .timeout(Duration::from_secs(15))
        .json(&serde_json::json!({ "message": message }))
        .send()
        .await;
    match sent {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => warn!(status = %r.status(), "Notification webhook refused the message"),
        Err(e) => warn!(error = %e.without_url(), "Could not reach the notification webhook"),
    }
}

fn any_site_message(entry: &str, host: &str, fields: &[String], node: &str) -> String {
    format!(
        "Vault entry '{entry}' ({}) was filled into {host}, a site it is not saved for (live view on {node}).",
        fields.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passkeys::Request;

    fn req() -> Request {
        serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000000",
            "page_id": "7C4E8F21A9B0",
            "rp_id": "login.example.com",
            "kind": "fill",
            "entry": "00000000-0000-0000-0000-000000000001",
            "fields": ["username", "password"],
        }))
        .unwrap()
    }

    #[test]
    fn custom_fields_are_text_and_linked_logins() {
        let raw = serde_json::json!({
            "data": { "username": "me", "password": "pw" },
            "fields": [
                { "name": "a", "value": "1", "type": "text" },
                { "name": "b", "value": "2" },
                { "name": "h", "value": "3", "type": "hidden" },
                { "name": "c", "value": "4", "type": "boolean" },
                { "name": "u", "value": null, "type": "linked", "linked_id": 100 },
                { "name": "p", "value": null, "type": "linked", "linked_id": 101 },
                { "name": "x", "value": null, "type": "linked", "linked_id": 305 },
                { "name": "old", "value": null, "type": "linked" },
                { "name": "", "value": "5", "type": "text" },
                { "name": "e", "value": "", "type": "text" }
            ]
        });
        let names: Vec<(String, String)> = custom_fields(&raw)
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().into(),
                    f["value"].as_str().unwrap().into(),
                )
            })
            .collect();
        let want = [("a", "1"), ("b", "2"), ("u", "me"), ("p", "pw")];
        assert_eq!(names, want.map(|(n, v)| (n.to_string(), v.to_string())));
    }

    #[test]
    fn requests_from_a_node_are_plain_values() {
        assert!(valid_request(&req()).is_ok());
        let mut bad = req();
        bad.rp_id = "--cdp-url".into();
        assert!(valid_request(&bad).is_err());
        let mut bad = req();
        bad.page_id = "x --flag".into();
        assert!(valid_request(&bad).is_err());
        let mut bad = req();
        bad.entry = Some("-x".into());
        assert!(valid_request(&bad).is_ok(), "ids may contain a dash");
        bad.entry = Some("a b".into());
        assert!(valid_request(&bad).is_err());
        let mut bad = req();
        bad.fields = vec!["notes".into()];
        assert!(valid_request(&bad).is_err());
        let mut bad = req();
        bad.origin = Some("file:///etc/passwd".into());
        assert!(valid_request(&bad).is_err());
        // The page's address only ever is a web address (it becomes rbw's --url).
        let mut ok = req();
        ok.url = Some("http://10.5.1.1:8123/lovelace/0".into());
        assert!(valid_request(&ok).is_ok());
        for url in ["--help", "javascript:alert(1)", "file:///etc/passwd"] {
            let mut bad = req();
            bad.url = Some(url.into());
            assert!(valid_request(&bad).is_err(), "{url}");
        }
    }

    #[test]
    fn a_request_id_is_served_once() {
        let args = SecretAgentArgs {
            nodes: vec![],
            cluster_secret_file: PathBuf::new(),
            rbw: PathBuf::from("rbw"),
            unlock_webhook: None,
            unlock_host: "x".into(),
            notify_webhook: None,
            seen: Default::default(),
            accounts: Default::default(),
        };
        let id = uuid::Uuid::new_v4();
        assert!(args.remember(id));
        assert!(!args.remember(id));
        assert!(args.remember(uuid::Uuid::new_v4()));
    }

    #[test]
    fn the_notice_names_entry_site_and_fields_only() {
        let m = any_site_message(
            "Bank",
            "evil.test",
            &["username".into(), "password".into()],
            "rofl-13",
        );
        assert_eq!(
            m,
            "Vault entry 'Bank' (username, password) was filled into evil.test, a site it is not saved for (live view on rofl-13)."
        );
    }
}
