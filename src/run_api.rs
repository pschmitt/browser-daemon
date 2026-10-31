//! `POST /api/run`: run a Playwright script or a list of steps in a session of
//! this node and answer with the result (see docs/run.md). Playwright runs here,
//! in a Node process per request (playwright-runner.mjs) under Node's permission
//! model, so clients need nothing but HTTP.

use super::*;
use tokio::io::AsyncReadExt;

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// The runner, written into each run's own directory.
static RUNNER_JS: &str = include_str!("../playwright-runner.mjs");

/// Runs at once on a node; more wait for a slot.
const MAX_CONCURRENT_RUNS: usize = 4;
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 600;
const DEFAULT_ACTION_TIMEOUT_SECS: u64 = 15;
const MAX_STEPS: usize = 500;
/// The runner's answer (screenshots included) is cut off beyond this.
const MAX_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
/// Set on a run forwarded to the profile's node, which must not forward it again.
const FORWARDED_HEADER: &str = "x-browser-daemon-run-forwarded";

/// Node and Playwright for runs (`--run-node`, `--playwright-core`).
pub(crate) struct Runner {
    node: PathBuf,
    playwright_core: PathBuf,
    /// Node 25+ also restricts the network under `--permission`.
    allow_net: bool,
    /// Where the daemon itself listens, for the runner's CDP connection.
    endpoint_base: String,
    slots: tokio::sync::Semaphore,
}

impl Runner {
    /// Checks that Node runs and has a permission model. Runs stay off without one:
    /// the scripts would have the daemon's own rights on the host.
    pub(crate) async fn new(node: PathBuf, playwright_core: PathBuf, bind: SocketAddr) -> R<Self> {
        if !playwright_core.join("index.mjs").is_file() {
            return Err(format!(
                "{} is not a playwright-core package (no index.mjs)",
                playwright_core.display()
            )
            .into());
        }
        let probe = |flags: &'static [&'static str]| {
            let node = node.clone();
            async move {
                tokio::process::Command::new(&node)
                    .args(flags)
                    .args(["-e", "0"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| s.success())
            }
        };
        if !probe(&["--permission"]).await {
            return Err(format!(
                "{} does not run with --permission (Node 22 or newer is needed)",
                node.display()
            )
            .into());
        }
        let allow_net = probe(&["--permission", "--allow-net"]).await;
        Ok(Self {
            node,
            playwright_core,
            allow_net,
            endpoint_base: endpoint_base(bind),
            slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_RUNS),
        })
    }
}

/// `ws://HOST:PORT` of the daemon as seen from this host.
fn endpoint_base(bind: SocketAddr) -> String {
    let ip = bind.ip();
    let host = if ip.is_unspecified() || ip.is_loopback() {
        match ip {
            std::net::IpAddr::V6(v6) if !v6.is_unspecified() => "[::1]".to_string(),
            _ => "127.0.0.1".to_string(),
        }
    } else {
        match ip {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            std::net::IpAddr::V4(v4) => v4.to_string(),
        }
    };
    format!("ws://{host}:{}", bind.port())
}

#[derive(Deserialize, Serialize)]
pub(crate) struct RunRequest {
    /// Run in this profile's browser (started when it is not running). The run is
    /// forwarded to the node that has the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile: Option<String>,
    /// Run in this running session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<Uuid>,
    /// A tab (page id) of the session to run in; by default a new tab, closed afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tab: Option<String>,
    /// Address to open before the script or steps run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// A function expression, `async ({ page, context, browser, args, log }) => value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) script: Option<String>,
    /// Steps (`{"action": "click", "selector": "..."}`), instead of a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) steps: Option<Vec<serde_json::Value>>,
    /// Values for the script (`args`) and steps (`value_arg`). Never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) args: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeout_seconds: Option<u64>,
    /// The default timeout of each Playwright action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) action_timeout_seconds: Option<u64>,
    /// Leave the tab open (its id is in the answer's `page`).
    #[serde(default)]
    pub(crate) keep_open: bool,
    /// A screenshot of the tab when the run fails (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screenshot_on_error: Option<bool>,
    /// A session the run starts stays when the run ends (until its idle timeout);
    /// otherwise it ends with the run. Implied by `keep_open`.
    #[serde(default)]
    pub(crate) persistent: bool,
    /// For a session started by the run: minutes without use before it stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_timeout_minutes: Option<u64>,
    /// Who runs it, shown on the dashboard ("Home Assistant · HR WORKS").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client: Option<String>,
}

impl RunRequest {
    fn validate(&self) -> Result<(), String> {
        if self.profile.is_some() && self.session_id.is_some() {
            return Err("Give a profile or a session_id, not both".into());
        }
        match (&self.script, &self.steps) {
            (Some(_), Some(_)) => return Err("Give a script or steps, not both".into()),
            (None, None) => return Err("A script or steps are required".into()),
            (_, Some(steps)) if steps.len() > MAX_STEPS => {
                return Err(format!("At most {MAX_STEPS} steps"))
            }
            (_, Some(steps)) => {
                if let Some(i) = steps
                    .iter()
                    .position(|s| s.get("action").and_then(|a| a.as_str()).is_none())
                {
                    return Err(format!("Step {i} has no action"));
                }
            }
            _ => {}
        }
        if self.tab.is_some() && self.profile.is_none() && self.session_id.is_none() {
            return Err("A tab needs its profile or session_id".into());
        }
        if self.args.as_ref().is_some_and(|a| !a.is_object()) {
            return Err("args must be an object".into());
        }
        Ok(())
    }

    /// The CDP endpoint of the session on this node.
    fn endpoint(&self, base: &str) -> String {
        let mut url = url::Url::parse(&format!("{base}/chromium")).expect("valid base");
        {
            let mut q = url.query_pairs_mut();
            if let Some(ref p) = self.profile {
                q.append_pair("profile", p);
            }
            if let Some(id) = self.session_id {
                q.append_pair("session_id", &id.to_string());
            }
            if self.persistent || self.keep_open {
                q.append_pair("persistent", "true");
            }
            if let Some(m) = self.idle_timeout_minutes {
                q.append_pair("idle_timeout_minutes", &m.to_string());
            }
            q.append_pair("client", self.client.as_deref().unwrap_or("Run API"));
        }
        url.to_string()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        )
    }
}

/// The node the profile is on, when that is another node of the mesh.
async fn profile_peer(state: &AppState, profile: &str) -> Result<Option<String>, ApiError> {
    if profiles::find(&state.config.profiles_dir, profile)
        .await
        .is_some()
    {
        return Ok(None);
    }
    let snapshot = state.overview_snapshot(Duration::from_secs(10)).await;
    let overview: MeshOverview = serde_json::from_str(&snapshot.json)
        .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "The mesh overview is unreadable"))?;
    overview
        .profiles
        .iter()
        .find(|p| {
            !p.is_local
                && (p.profile.name == profile
                    || p.profile.id.is_some_and(|id| id.to_string() == profile))
        })
        .map(|p| Some(p.node_name.clone()))
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("No node has a profile '{profile}'"),
            )
        })
}

/// Hands the run to the node that has the profile and passes its answer on.
async fn forward(state: &AppState, peer: &str, req: &RunRequest) -> Result<Response, ApiError> {
    let base = get_peer_url(state, peer).await.ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Peer '{peer}' not found in mesh"),
        )
    })?;
    let url = format!("{}/api/run", base.trim_end_matches('/'));
    info!(peer, "Run forwarded to the profile's node");
    let res = state
        .peer_request(state.peer_client.post(url))
        .header(FORWARDED_HEADER, "1")
        .json(req)
        .timeout(req.timeout() + Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("Peer '{peer}': {e}")))?;
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = res
        .bytes()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("Peer '{peer}': {e}")))?;
    Ok((
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

pub(crate) async fn api_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RunRequest>,
) -> Result<Response, ApiError> {
    state.check_auth(&headers)?;
    // Scripts run on the host: a node open to anyone must not take them.
    if state.cluster_secret.is_none() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "Runs need a cluster secret on the node",
        ));
    }
    req.validate()
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    if let Some(ref profile) = req.profile {
        if !headers.contains_key(FORWARDED_HEADER) {
            if let Some(peer) = profile_peer(&state, profile).await? {
                return forward(&state, &peer, &req).await;
            }
        }
    }
    let runner = state.runner.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::NOT_IMPLEMENTED,
            "Runs are not enabled on this node (--run-node, --playwright-core)",
        )
    })?;
    if let Some(id) = req.session_id {
        let session = state.sessions.read().await.get(&id).cloned();
        match session {
            None => return Err(api_error(StatusCode::NOT_FOUND, "Session not found")),
            Some(s) if s.bidi.is_some() => {
                return Err(api_error(
                    StatusCode::CONFLICT,
                    "Runs work in Chromium sessions only",
                ))
            }
            Some(_) => {}
        }
    }

    let _slot = runner
        .slots
        .acquire()
        .await
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "Shutting down"))?;
    let started = Instant::now();
    let out = run(&state, runner, &req).await?;
    let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let timed_out = out
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    info!(
        profile = req.profile.as_deref().unwrap_or(""),
        steps = req.steps.as_ref().map_or(0, |s| s.len()),
        script = req.script.is_some(),
        ok,
        ms = started.elapsed().as_millis() as u64,
        "Run finished"
    );
    let status = match (ok, timed_out) {
        (true, _) => StatusCode::OK,
        (false, true) => StatusCode::GATEWAY_TIMEOUT,
        (false, false) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    Ok((status, [(header::CACHE_CONTROL, "no-store")], Json(out)).into_response())
}

/// Runs the request in a Node process of its own and returns what it printed.
async fn run(
    state: &AppState,
    runner: &Runner,
    req: &RunRequest,
) -> Result<serde_json::Value, ApiError> {
    let internal = |e: std::io::Error| {
        error!(error = %e, "Run could not start");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The runner could not start",
        )
    };
    // The runner and Playwright's temporary files; removed when the run ends.
    let dir = tempfile::Builder::new()
        .prefix("browser-daemon-run-")
        .tempdir()
        .map_err(internal)?;
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&tmp).map_err(internal)?;
    let script = dir.path().join("runner.mjs");
    std::fs::write(&script, RUNNER_JS).map_err(internal)?;

    let timeout = req.timeout();
    let input = serde_json::json!({
        "endpoint": req.endpoint(&runner.endpoint_base),
        "tab": req.tab,
        "url": req.url,
        "script": req.script,
        "steps": req.steps,
        "args": req.args,
        "keep_open": req.keep_open,
        "screenshot_on_error": req.screenshot_on_error.unwrap_or(true),
        "timeout_ms": timeout.as_millis() as u64,
        "action_timeout_ms": req.action_timeout_seconds.unwrap_or(DEFAULT_ACTION_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS) * 1000,
    });

    let mut cmd = tokio::process::Command::new(&runner.node);
    cmd.arg("--permission")
        .arg(format!(
            "--allow-fs-read={}",
            runner.playwright_core.display()
        ))
        .arg(format!("--allow-fs-read={}", dir.path().display()))
        // Playwright looks whether it runs under WSL.
        .arg("--allow-fs-read=/proc/sys/fs/binfmt_misc/WSLInterop")
        .arg("--allow-fs-read=/run/WSL")
        .arg(format!("--allow-fs-write={}", tmp.display()));
    if runner.allow_net {
        cmd.arg("--allow-net");
    }
    cmd.arg(&script)
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", "/nonexistent")
        .env("TMPDIR", &tmp)
        .env("HOME", &tmp)
        .env("BD_PLAYWRIGHT_CORE", &runner.playwright_core)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Errors may quote the script and its values: never kept.
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some((token, _)) = state.auth.issue(auth::now()) {
        cmd.env("BD_RUN_TOKEN", token);
    }
    let mut child = cmd.spawn().map_err(internal)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let body = serde_json::to_vec(&input).unwrap_or_default();
    let io = async move {
        stdin.write_all(&body).await?;
        drop(stdin);
        let mut out = Vec::new();
        (&mut stdout)
            .take(MAX_OUTPUT_BYTES)
            .read_to_end(&mut out)
            .await?;
        Ok::<_, std::io::Error>(out)
    };
    // The runner stops itself at the deadline; this is for one that hangs.
    let out = match tokio::time::timeout(timeout + Duration::from_secs(15), io).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(internal(e)),
        Err(_) => {
            let _ = child.kill().await;
            return Ok(
                serde_json::json!({ "ok": false, "timed_out": true, "error": "The runner did not answer in time" }),
            );
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
    let line = out
        .split(|b| *b == b'\n')
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    serde_json::from_slice(line).map_err(|_| {
        warn!("Run ended without an answer");
        api_error(
            StatusCode::BAD_GATEWAY,
            "The runner ended without an answer",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: serde_json::Value) -> RunRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_run_needs_exactly_one_of_script_and_steps() {
        assert!(req(serde_json::json!({})).validate().is_err());
        assert!(req(serde_json::json!({ "script": "x", "steps": [] }))
            .validate()
            .is_err());
        assert!(req(serde_json::json!({ "script": "async () => 1" }))
            .validate()
            .is_ok());
        assert!(req(serde_json::json!({ "steps": [{ "action": "url" }] }))
            .validate()
            .is_ok());
        assert!(req(serde_json::json!({ "steps": [{ "selector": "a" }] }))
            .validate()
            .is_err());
    }

    #[test]
    fn a_run_names_one_browser() {
        let id = Uuid::new_v4();
        assert!(
            req(serde_json::json!({ "script": "x", "profile": "work", "session_id": id }))
                .validate()
                .is_err()
        );
        assert!(req(serde_json::json!({ "script": "x", "tab": "ABC" }))
            .validate()
            .is_err());
        assert!(
            req(serde_json::json!({ "script": "x", "profile": "work", "tab": "ABC" }))
                .validate()
                .is_ok()
        );
        assert!(req(serde_json::json!({ "script": "x", "args": [1] }))
            .validate()
            .is_err());
    }

    #[test]
    fn the_endpoint_names_the_profile_and_the_client() {
        let r = req(
            serde_json::json!({ "script": "x", "profile": "my work", "idle_timeout_minutes": 5 }),
        );
        assert_eq!(
            r.endpoint("ws://127.0.0.1:3001"),
            "ws://127.0.0.1:3001/chromium?profile=my+work&idle_timeout_minutes=5&client=Run+API"
        );
        let kept = req(serde_json::json!({ "script": "x", "profile": "work", "keep_open": true }));
        assert_eq!(
            kept.endpoint("ws://127.0.0.1:3001"),
            "ws://127.0.0.1:3001/chromium?profile=work&persistent=true&client=Run+API"
        );
    }

    #[test]
    fn the_daemon_is_reached_on_loopback_when_it_listens_everywhere() {
        assert_eq!(
            endpoint_base("0.0.0.0:3001".parse().unwrap()),
            "ws://127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_base("127.0.0.1:3001".parse().unwrap()),
            "ws://127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_base("[::1]:3001".parse().unwrap()),
            "ws://[::1]:3001"
        );
        assert_eq!(
            endpoint_base("10.0.0.2:3001".parse().unwrap()),
            "ws://10.0.0.2:3001"
        );
    }

    #[test]
    fn timeouts_are_capped() {
        assert_eq!(
            req(serde_json::json!({ "script": "x" })).timeout(),
            Duration::from_secs(60)
        );
        assert_eq!(
            req(serde_json::json!({ "script": "x", "timeout_seconds": 100000 })).timeout(),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
    }
}
