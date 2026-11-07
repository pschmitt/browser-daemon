//! Provision short-lived nodes through operator-provided executables.

use super::*;
use std::process::Stdio;

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    id: String,
    url: String,
    #[serde(default)]
    session: Option<Uuid>,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    seen_client: bool,
}

fn lease_dir(state: &AppState) -> PathBuf {
    state.config.state_dir().join("ephemeral-nodes")
}

async fn save(state: &AppState, key: Uuid, lease: &Lease) -> std::io::Result<()> {
    let dir = lease_dir(state);
    fs::create_dir_all(&dir).await?;
    let data = serde_json::to_vec(lease).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{key}.tmp"));
    fs::write(&tmp, data).await?;
    fs::rename(tmp, dir.join(format!("{key}.json"))).await
}

async fn destroy(state: &AppState, key: Uuid, lease: &Lease) {
    let Some(cmd) = &state.ephemeral_down else {
        return;
    };
    loop {
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            tokio::process::Command::new(cmd)
                .arg(&lease.id)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        if matches!(result, Ok(Ok(status)) if status.success()) {
            let _ = fs::remove_file(lease_dir(state).join(format!("{key}.json"))).await;
            return;
        }
        warn!(node = %lease.id, "Ephemeral node teardown failed; retrying");
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn watch(state: AppState, key: Uuid, mut lease: Lease) {
    let Some(id) = lease.session else {
        destroy(&state, key, &lease).await;
        return;
    };
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let url = format!("{}/api/sessions/{id}", lease.url.trim_end_matches('/'));
        let response = state.peer_request(state.http_client.get(&url)).send().await;
        match response {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => break,
            Ok(response) if response.status().is_success() => {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if body["session"]["client_count"].as_u64().unwrap_or(0) > 0 {
                        if !lease.seen_client {
                            lease.seen_client = true;
                            if let Err(e) = save(&state, key, &lease).await {
                                warn!(error = %e, "Could not save ephemeral node state");
                            }
                        }
                    } else if !lease.seen_client
                        && clients::now_secs().saturating_sub(lease.created) > 120
                    {
                        // A session created but never attached must not leave a VM behind.
                        if state
                            .peer_request(
                                state.peer_client.post(format!("{url}/expire-unattached")),
                            )
                            .timeout(Duration::from_secs(60))
                            .send()
                            .await
                            .is_ok_and(|r| {
                                r.status().is_success() || r.status() == StatusCode::NOT_FOUND
                            })
                        {
                            break;
                        }
                    }
                }
            }
            _ => {
                warn!(node = %lease.id, "Cannot inspect ephemeral session; will retry");
                tokio::time::sleep(Duration::from_secs(28)).await;
            }
        }
    }
    destroy(&state, key, &lease).await;
}

pub(crate) async fn recover(state: &AppState) {
    let Ok(mut entries) = fs::read_dir(lease_dir(state)).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(key) = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(data) = fs::read(entry.path()).await {
            if let Ok(lease) = serde_json::from_slice::<Lease>(&data) {
                tokio::spawn(watch(state.clone(), key, lease));
            }
        }
    }
}

pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.check_auth(&headers)?;
    if req.profile.is_some() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Saved profiles cannot be placed on ephemeral nodes",
        ));
    }
    req.persistent = false;
    let Some(up) = state.ephemeral_up.clone() else {
        let overview = fetch_mesh_overview(&state).await;
        let peer = overview
            .peers
            .iter()
            .filter(|peer| peer.status == "online" && peer.ephemeral_provider)
            .min_by(|a, b| a.name.cmp(&b.name))
            .ok_or_else(|| {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "No ephemeral node provider is online",
                )
            })?;
        let response = state
            .peer_request(state.peer_client.post(format!(
                "{}/api/ephemeral/sessions",
                peer.url.trim_end_matches('/')
            )))
            .timeout(Duration::from_secs(1650))
            .json(&req)
            .send()
            .await
            .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Ephemeral provider request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(api_error(
                status,
                "Ephemeral provider could not create a session",
            ));
        }
        let result = response.json().await.map_err(|_| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "Invalid ephemeral provider response",
            )
        })?;
        return Ok(Json(result));
    };
    // Keep provisioning alive if the HTTP caller disconnects; the unused-session
    // timeout will destroy the resulting node.
    let task = tokio::spawn(async move {
        let key = Uuid::new_v4();
        let mut lease = Lease {
            id: key.to_string(),
            url: String::new(),
            session: None,
            created: clients::now_secs(),
            seen_client: false,
        };
        save(&state, key, &lease).await.map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save ephemeral node lease",
            )
        })?;
        let output = tokio::time::timeout(
            // The provider may wait for a VM boot and then for its service.
            Duration::from_secs(1500),
            tokio::process::Command::new(up)
                .arg(&lease.id)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let output = match output {
            Ok(Ok(output)) if output.status.success() => output,
            _ => {
                tokio::spawn(watch(state, key, lease));
                return Err(api_error(
                    StatusCode::BAD_GATEWAY,
                    "Ephemeral provisioning failed or timed out",
                ));
            }
        };
        let descriptor = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
        let url = descriptor.as_ref().and_then(|d| d["url"].as_str());
        let valid = url.and_then(|s| url::Url::parse(s).ok()).is_some_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host().is_some()
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
        });
        if !valid {
            tokio::spawn(watch(state, key, lease));
            return Err(api_error(
                StatusCode::BAD_GATEWAY,
                "Invalid ephemeral node descriptor",
            ));
        }
        lease.url = url.unwrap_or_default().to_string();
        lease.created = clients::now_secs();
        if save(&state, key, &lease).await.is_err() {
            tokio::spawn(async move {
                destroy(&state, key, &lease).await;
            });
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save ephemeral node lease",
            ));
        }
        let response = state
            .peer_request(
                state
                    .peer_client
                    .post(format!("{}/api/sessions", lease.url.trim_end_matches('/'))),
            )
            .json(&req)
            .timeout(Duration::from_secs(120))
            .send()
            .await;
        let session = match response {
            Ok(r) if r.status().is_success() => r.json::<SessionSummary>().await.ok(),
            _ => None,
        };
        let Some(session) = session else {
            tokio::spawn(watch(state, key, lease));
            return Err(api_error(
                StatusCode::BAD_GATEWAY,
                "Could not create session on ephemeral node",
            ));
        };
        lease.session = Some(session.id);
        lease.created = clients::now_secs();
        if save(&state, key, &lease).await.is_err() {
            tokio::spawn(watch(
                state,
                key,
                Lease {
                    session: None,
                    ..lease
                },
            ));
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save ephemeral session lease",
            ));
        }
        let url = lease.url.clone();
        tokio::spawn(watch(state, key, lease));
        Ok(Json(
            serde_json::json!({ "node_url": url, "session": session }),
        ))
    });
    task.await.map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Ephemeral provisioning task failed",
        )
    })?
}
