//! `browser-daemon passkey-helper`: runs as the vault's user and serves the
//! passkeys approved in live views (see passkeys.rs) with `rbw passkey inject`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use crate::passkeys::{Outcome, Request};

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
            .get(format!("{node}/api/passkey-helper/next"))
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
        info!(node = %node, rp_id = %req.rp_id, page = %req.page_id, "Passkey requested");
        let (node, secret, client, args) =
            (node.clone(), secret.clone(), client.clone(), args.clone());
        tokio::spawn(async move {
            let outcome = match serve(&node, &req, &client, &args).await {
                Ok(()) => Outcome {
                    ok: true,
                    error: None,
                },
                Err(e) => {
                    warn!(rp_id = %req.rp_id, error = %e, "Passkey not served");
                    Outcome {
                        ok: false,
                        error: Some(e),
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
async fn serve(
    node: &str,
    req: &Request,
    client: &reqwest::Client,
    args: &HelperArgs,
) -> Result<(), String> {
    if !unlocked(&args.rbw).await {
        let hook = args.unlock_webhook.as_ref().ok_or("The vault is locked")?;
        client
            .post(hook)
            .json(&serde_json::json!({ "host": args.unlock_host, "agent": format!("Passkey for {}", req.rp_id) }))
            .send()
            .await
            .map_err(|e| format!("Could not ask for an unlock: {e}"))?;
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
        ])
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
