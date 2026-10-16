//! `browser-daemon passkey-helper`: runs as the vault's user and serves the
//! passkeys approved in live views (see passkeys.rs) with `rbw passkey inject`,
//! and the logins picked from their vault button (see autofill.rs).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use crate::passkeys::{Outcome, Request};
use crate::{secrets, vault_index};

#[derive(clap::Args, Debug)]
pub struct HelperArgs {
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
}

pub async fn run(args: HelperArgs) -> Result<(), Box<dyn std::error::Error>> {
    let secret = tokio::fs::read_to_string(&args.cluster_secret_file)
        .await?
        .trim()
        .to_string();
    let args = std::sync::Arc::new(args);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    // Keeps the vault index (vault_index.rs) current while rbw is unlocked, so
    // lookups rarely wait for rbw.
    let rbw = args.rbw.clone();
    tokio::spawn(async move {
        loop {
            if unlocked(&rbw).await {
                let _ = vault_index::entries(&rbw);
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
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
    args: std::sync::Arc<HelperArgs>,
) {
    info!(node = %node, "Serving passkeys");
    loop {
        let res = client
            .get(format!(
                "{node}/api/passkey-helper/next?finds=true&autofill=true"
            ))
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
                warn!(node = %node, error = %e, "Node unreachable");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        info!(node = %node, rp_id = %req.rp_id, page = %req.page_id, kind = %req.kind, "Passkey requested");
        let (node, secret, client, args) =
            (node.clone(), secret.clone(), client.clone(), args.clone());
        tokio::spawn(async move {
            let result = match req.kind.as_str() {
                "find" => find(&req, &args).await.map(Some),
                "logins" => logins(&node, &secret, &req, &client, &args).await.map(Some),
                "fill" => fill(&node, &secret, &req, &client, &args).await.map(Some),
                "assert" => assert(&node, &secret, &req, &client, &args).await.map(Some),
                _ => serve(&node, &secret, &req, &client, &args)
                    .await
                    .map(|()| None),
            };
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
                .post(format!("{node}/api/passkey-helper/{}", req.id))
                .header("X-Browser-Cluster-Secret", &secret)
                .json(&outcome)
                .send()
                .await;
        });
    }
}

/// Which vault entry would serve `rp_id` (name and user only), or that the vault
/// is locked. Never asks for an unlock: that only happens once the user approves.
async fn find(req: &Request, args: &HelperArgs) -> Result<serde_json::Value, String> {
    if !unlocked(&args.rbw).await {
        return Ok(serde_json::json!({ "locked": true }));
    }
    let out = Command::new(&args.rbw)
        .args(["passkey", "find", "--rp-id", &req.rp_id])
        .stdin(Stdio::null())
        .output()
        .await
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
    Command::new(rbw)
        .arg("unlocked")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Ready once `rbw passkey inject` has the passkey in the page; rbw keeps
/// running in the background until the assertion and the vault sync are done.
/// rbw unlocked, asking the phone first (and telling the view so) if needed.
async fn ensure_unlocked(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &HelperArgs,
) -> Result<(), String> {
    if !unlocked(&args.rbw).await {
        let hook = args.unlock_webhook.as_ref().ok_or("The vault is locked")?;
        client
            .post(hook)
            .json(&serde_json::json!({ "host": args.unlock_host, "agent": format!("{} for {}", if matches!(req.kind.as_str(), "logins" | "fill") { "Login" } else { "Passkey" }, req.rp_id) }))
            .send()
            .await
            .map_err(|e| format!("Could not ask for an unlock: {e}"))?;
        let _ = client
            .post(format!("{node}/api/passkey-helper/{}/note", req.id))
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
    args: &HelperArgs,
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
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .await
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
    args: &HelperArgs,
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

/// At most this many vault entries are checked for a site (without the index).
const MAX_CANDIDATES: usize = 30;
/// At most this many logins are listed.
const MAX_RESULTS: usize = 50;

/// The login entries for the page's site (rp_id is its host), as Bitwarden's
/// autofill would pick them: name, user and whether there's a TOTP, no secrets.
/// A locked vault is reported, or unlocked first when the user asked.
async fn logins(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &HelperArgs,
) -> Result<serde_json::Value, String> {
    if req.unlock {
        ensure_unlocked(node, secret, req, client, args).await?;
    } else if !unlocked(&args.rbw).await {
        return Ok(serde_json::json!({ "locked": true }));
    }
    let host = req.rp_id.as_str();
    if let Some(all) = vault_index::entries(&args.rbw) {
        let mut out: Vec<(bool, &vault_index::Entry)> = all
            .iter()
            .map(|e| (secrets::site_matches(host, &e.uris), e))
            .filter(|(site, e)| match req.query.as_deref() {
                Some(q) => e.matches(q),
                None => *site,
            })
            .collect();
        out.sort_by_key(|(site, e)| (!site, e.name.to_lowercase()));
        return Ok(out
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(site, e)| {
                serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "user": e.user,
                    "folder": e.folder,
                    "password": e.password,
                    "totp": e.totp,
                    "site": site,
                })
            })
            .collect());
    }
    // Every URI of a matching entry names the site's base domain, so a search for it
    // narrows the vault cheaply (`rbw list --raw` decrypts all of it); the URIs and
    // their match types then decide. A search of the user's lists every login it
    // finds, the site's first.
    let term = req.query.as_deref().unwrap_or(secrets::base_domain(host));
    let found = secrets::rbw(&args.rbw, &["search", "--fields", "id", "--", term])
        .await
        .unwrap_or_default();
    let ids: Vec<&str> = found
        .lines()
        .filter_map(|l| l.split('\t').next())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .take(MAX_CANDIDATES)
        .collect();
    let entries = futures_util::future::join_all(
        ids.iter()
            .map(|id| async move { secrets::rbw(&args.rbw, &["get", "--raw", id]).await }),
    )
    .await;
    let mut out: Vec<serde_json::Value> = ids
        .iter()
        .zip(entries)
        .filter_map(|(id, raw)| {
            let raw: serde_json::Value = serde_json::from_str(&raw.ok()?).ok()?;
            let data = &raw["data"];
            let has = |f: &str| data[f].as_str().is_some_and(|v| !v.is_empty());
            let site = secrets::site_matches(host, &secrets::uris(&raw));
            ((site || req.query.is_some()) && (has("username") || has("password"))).then(|| {
                serde_json::json!({
                    "id": id,
                    "name": raw["name"],
                    "user": data["username"],
                    "folder": raw["folder"],
                    "password": has("password"),
                    "totp": has("totp"),
                    "site": site,
                })
            })
        })
        .collect();
    // The site's entries first, then by name.
    out.sort_by_key(|e| {
        (
            e["site"] != true,
            e["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    });
    Ok(serde_json::Value::Array(out))
}

/// Fills the picked entry into the page: read here, checked against the page's
/// site, then sent to the node, which puts it into the login form.
async fn fill(
    node: &str,
    secret: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &HelperArgs,
) -> Result<serde_json::Value, String> {
    let entry = req.entry.as_deref().ok_or("No entry")?;
    if uuid::Uuid::parse_str(entry).is_err() {
        return Err("Not a vault entry id".into());
    }
    ensure_unlocked(node, secret, req, client, args).await?;
    let raw: serde_json::Value =
        serde_json::from_str(&secrets::rbw(&args.rbw, &["get", "--raw", entry]).await?)
            .map_err(|_| "rbw: unexpected output".to_string())?;
    let mut uris = secrets::uris(&raw);
    if !secrets::site_matches(&req.rp_id, &uris) {
        if !req.any_site {
            return Err(format!("This entry is not saved for {}", req.rp_id));
        }
        // Confirmed in the live view: only into the page's own host, no other frames.
        info!(host = %req.rp_id, "Filling an entry not saved for the site, as confirmed");
        uris = vec![secrets::Uri {
            uri: format!("https://{}", req.rp_id),
            match_type: Some(1),
        }];
    }
    let want = |f: &str| req.fields.iter().any(|x| x == f);
    let field = |f: &str| {
        want(f)
            .then(|| secrets::pick_field(&raw, f))
            .flatten()
            .filter(|v| !v.is_empty())
    };
    let totp = if want("totp") && raw["data"]["totp"].as_str().is_some_and(|t| !t.is_empty()) {
        Some(
            secrets::rbw(&args.rbw, &["code", entry])
                .await?
                .trim()
                .to_string(),
        )
    } else {
        None
    };
    let body = serde_json::json!({
        "host": req.rp_id,
        "uris": uris,
        "username": field("username"),
        "password": field("password"),
        "totp": totp,
    });
    if body["username"].is_null() && body["password"].is_null() && body["totp"].is_null() {
        return Err("The entry has nothing to fill".into());
    }
    let res = client
        .post(format!("{node}/api/pages/{}/autofill", req.page_id))
        .header("X-Browser-Cluster-Secret", secret)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Could not reach the node: {e}"))?;
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
