//! Passkey sign-ins for AI agents: `POST /api/sessions/{id}/agent-passkey`.
//!
//! The live view's passkey dialog needs a person. An agent (`browser-daemon
//! mcp`, tool `browser_passkey`) arms a Chromium session instead: for a while,
//! every `navigator.credentials.get()` of every page and frame of the session,
//! on one of the sites it was armed for, is signed by the secret agent
//! (`rbw passkey assert`, as for the live view) and answered in the page. Nothing
//! is armed on one page, so sign-ins that hop across origins or ask the moment a
//! page loads (Microsoft's `login.microsoft.com/common/bridge/fido`) work.
//!
//! The origin and site come from the browser's own target list, never from page
//! script; rbw refuses an origin outside the passkey's relying party as well.
//! Autofill (conditional) requests are left to the browser. A request the vault
//! cannot answer falls back to the browser's own `get()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use uuid::Uuid;

use crate::passkeys::{Broker, Signing, Status};
use crate::secrets;

/// Longest an agent may arm a session for at once.
pub const MAX_ARMED: Duration = Duration::from_secs(30 * 60);
/// How long a page waits for the vault before it asks the browser instead.
const PAGE_WAIT: Duration = Duration::from_secs(330);

const BINDING: &str = "__bdAgentPasskey";

/// Wraps `navigator.credentials.get()`: a WebAuthn request (not conditional)
/// goes to the binding, and the answer becomes a PublicKeyCredential. No
/// binding (disarmed) or no answer: the browser's own `get()`.
const HOOK: &str = r#"(() => {
  const c = navigator.credentials;
  if (!c || c.__bdAgentHooked) return;
  Object.defineProperty(c, "__bdAgentHooked", { value: true });
  const get = c.get.bind(c), waiting = new Map();
  const b64url = (b) => btoa(String.fromCharCode(...new Uint8Array(b.buffer ? b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength) : b))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const bytes = (s) => { s = s.replace(/-/g, "+").replace(/_/g, "/"); s += "===".slice((s.length + 3) % 4); return Uint8Array.from(atob(s), (ch) => ch.charCodeAt(0)).buffer; };
  const own = (o, props) => { for (const [k, v] of Object.entries(props)) Object.defineProperty(o, k, { value: v, enumerable: true }); return o; };
  const credential = (a) => {
    const r = a.response;
    const response = own(Object.create(AuthenticatorAssertionResponse.prototype), {
      clientDataJSON: bytes(r.clientDataJSON), authenticatorData: bytes(r.authenticatorData),
      signature: bytes(r.signature), userHandle: r.userHandle ? bytes(r.userHandle) : null,
    });
    const cred = own(Object.create(PublicKeyCredential.prototype), {
      id: a.id, rawId: bytes(a.rawId), type: "public-key", response, authenticatorAttachment: a.authenticatorAttachment || "platform",
    });
    Object.defineProperty(cred, "getClientExtensionResults", { writable: true, configurable: true, value: () => ({}) });
    Object.defineProperty(cred, "toJSON", { writable: true, configurable: true, value: () => ({ id: a.id, rawId: a.id, type: "public-key", authenticatorAttachment: a.authenticatorAttachment || "platform", response: r, clientExtensionResults: {} }) });
    return cred;
  };
  // The vault is a platform authenticator with user verification: say so, or sites
  // such as GitHub warn of "partial passkey support" in a browser without one.
  const PKC = globalThis.PublicKeyCredential;
  if (PKC) {
    PKC.isUserVerifyingPlatformAuthenticatorAvailable = () => Promise.resolve(true);
    const caps = PKC.getClientCapabilities?.bind(PKC);
    if (caps) PKC.getClientCapabilities = () => caps().then((o) => ({ ...o, userVerifyingPlatformAuthenticator: true, passkeyPlatformAuthenticator: true }));
  }
  globalThis.__bdAgentPasskeyAnswer = (id, answer) => { const go = waiting.get(id); waiting.delete(id); go?.(answer); };
  c.get = function (opts) {
    const pk = opts && opts.publicKey;
    if (!pk || opts.mediation === "conditional" || typeof globalThis.__bdAgentPasskey !== "function") return get(opts);
    const id = Math.random().toString(36).slice(2);
    let req;
    try { req = { id, rpId: pk.rpId || location.hostname, challenge: b64url(pk.challenge), allow: (pk.allowCredentials || []).map((x) => b64url(x.id)) }; } catch { return get(opts); }
    return new Promise((go) => {
      waiting.set(id, go);
      setTimeout(() => globalThis.__bdAgentPasskeyAnswer(id, null), PAGE_WAIT_MS);
      try { globalThis.__bdAgentPasskey(JSON.stringify(req)); } catch { globalThis.__bdAgentPasskeyAnswer(id, null); }
    }).then((answer) => answer ? credential(answer) : get(opts));
  };
})()"#;

fn hook() -> String {
    HOOK.replace("PAGE_WAIT_MS", &PAGE_WAIT.as_millis().to_string())
}

/// Whether `rp_id` is one of `sites` (or a subdomain of one); `*` allows any.
pub fn site_allowed(sites: &[String], rp_id: &str) -> bool {
    sites
        .iter()
        .any(|s| s == "*" || rp_id == s || rp_id.ends_with(&format!(".{s}")))
}

/// What one session is armed for.
struct Armed {
    sites: Vec<String>,
    until: Instant,
    /// Bumped on every arm, so an ended watcher does not drop a newer one.
    generation: u64,
}

/// The armed sessions of this node and their watchers.
#[derive(Default)]
pub struct Watchers {
    armed: Mutex<HashMap<Uuid, Armed>>,
    generation: AtomicU64,
}

impl Watchers {
    /// Arms `session` for `sites` until `until`, or disarms it (`sites` empty).
    /// Starts a watcher on the browser at `browser_ws` unless one runs already.
    pub fn arm(
        self: &Arc<Self>,
        broker: Arc<Broker>,
        session: Uuid,
        browser_ws: String,
        sites: Vec<String>,
        until: Instant,
    ) {
        let mut armed = self.armed.lock().unwrap();
        if sites.is_empty() {
            armed.remove(&session);
            return;
        }
        if let Some(a) = armed.get_mut(&session) {
            a.sites = sites;
            a.until = until;
            return;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        armed.insert(
            session,
            Armed {
                sites,
                until,
                generation,
            },
        );
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.watch(&broker, session, &browser_ws).await {
                warn!(session_id = %session, "Agent passkeys stopped: {e}");
            }
            let mut armed = me.armed.lock().unwrap();
            if armed
                .get(&session)
                .is_some_and(|a| a.generation == generation)
            {
                armed.remove(&session);
            }
        });
    }

    /// The sites `session` is armed for now (none: not armed, or expired).
    fn sites(&self, session: Uuid) -> Option<Vec<String>> {
        let armed = self.armed.lock().unwrap();
        let a = armed.get(&session)?;
        (a.until > Instant::now()).then(|| a.sites.clone())
    }

    async fn watch(
        &self,
        broker: &Arc<Broker>,
        session: Uuid,
        browser_ws: &str,
    ) -> Result<(), String> {
        let (ws, _) = tokio_tungstenite::connect_async(browser_ws)
            .await
            .map_err(|e| format!("Could not reach the browser: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink
                    .send(Message::Text(msg.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let cdp = Arc::new(Conn {
            out,
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });
        // CDP session id -> target id, target id -> address (from the browser).
        let mut sessions: HashMap<String, String> = HashMap::new();
        let urls: Arc<Mutex<HashMap<String, String>>> = Arc::default();

        cdp.send(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        );
        cdp.send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            None,
        );
        info!(session_id = %session, "Agent passkeys armed");
        // Runs while armed: sites() is None once the time is up or disarmed.
        while let Some(left) = self
            .sites(session)
            .and_then(|_| self.until(session))
            .map(|u| u.saturating_duration_since(Instant::now()))
        {
            let msg = match tokio::time::timeout(left, stream.next()).await {
                Err(_) => continue, // expired, or armed for longer meanwhile
                Ok(None) => return Err("The browser went away".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(Message::Text(t)))) => t,
                Ok(Some(Ok(_))) => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&msg) else {
                continue;
            };
            if let Some(id) = v["id"].as_u64() {
                cdp.answered(id, v);
                continue;
            }
            let cdp_session = v["sessionId"].as_str().map(str::to_string);
            let params = &v["params"];
            match v["method"].as_str().unwrap_or_default() {
                "Target.attachedToTarget" => {
                    let child = params["sessionId"].as_str().unwrap_or_default().to_string();
                    let info = &params["targetInfo"];
                    let target = info["targetId"].as_str().unwrap_or_default().to_string();
                    if matches!(info["type"].as_str(), Some("page" | "iframe")) {
                        urls.lock().unwrap().insert(
                            target.clone(),
                            info["url"].as_str().unwrap_or_default().into(),
                        );
                        sessions.insert(child.clone(), target);
                        let cdp = cdp.clone();
                        tokio::spawn(async move { cdp.setup(&child).await });
                    } else {
                        cdp.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&child));
                    }
                }
                "Target.targetInfoChanged" => {
                    let info = &params["targetInfo"];
                    if let (Some(t), Some(u)) = (info["targetId"].as_str(), info["url"].as_str()) {
                        urls.lock().unwrap().insert(t.into(), u.into());
                    }
                }
                "Target.detachedFromTarget" => {
                    if let Some(s) = params["sessionId"].as_str() {
                        sessions.remove(s);
                    }
                }
                "Runtime.bindingCalled" if params["name"] == BINDING => {
                    // Disarmed meanwhile: answered all the same (no), so the
                    // page asks the browser instead of waiting.
                    let sites = self.sites(session).unwrap_or_default();
                    let Some(cdp_session) = cdp_session else {
                        continue;
                    };
                    let Some(target) = sessions.get(&cdp_session).cloned() else {
                        continue;
                    };
                    let url = urls
                        .lock()
                        .unwrap()
                        .get(&target)
                        .cloned()
                        .unwrap_or_default();
                    let payload = params["payload"].as_str().unwrap_or_default().to_string();
                    let (cdp, broker) = (cdp.clone(), broker.clone());
                    tokio::spawn(async move {
                        let (id, answer) =
                            match sign(&broker, &target, &url, &payload, &sites).await {
                                Ok((id, answer)) => (id, answer),
                                Err((id, e)) => {
                                    warn!(session_id = %session, "Agent passkey not signed: {e}");
                                    (id, Value::Null)
                                }
                            };
                        let Some(id) = id else { return };
                        let expr = format!(
                            "globalThis.__bdAgentPasskeyAnswer?.({}, {})",
                            json!(id),
                            answer
                        );
                        cdp.send(
                            "Runtime.evaluate",
                            json!({ "expression": expr }),
                            Some(&cdp_session),
                        );
                    });
                }
                _ => {}
            }
        }
        info!(session_id = %session, "Agent passkeys disarmed");
        Ok(())
    }

    fn until(&self, session: Uuid) -> Option<Instant> {
        self.armed.lock().unwrap().get(&session).map(|a| a.until)
    }
}

/// Has the secret agent sign one page request. Gives the page's request id and
/// the assertion, or the request id (when it could be read) and why not.
async fn sign(
    broker: &Broker,
    target: &str,
    url: &str,
    payload: &str,
    sites: &[String],
) -> Result<(Option<String>, Value), (Option<String>, String)> {
    let req: Value =
        serde_json::from_str(payload).map_err(|_| (None, "Unreadable request".to_string()))?;
    let id = req["id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_string);
    let fail = |e: &str| (id.clone(), e.to_string());
    if id.is_none() {
        return Err(fail("Unreadable request id"));
    }
    let page = url::Url::parse(url)
        .ok()
        .filter(|u| {
            u.scheme() == "https" || (u.scheme() == "http" && u.host_str() == Some("localhost"))
        })
        .ok_or_else(|| fail("The page has no secure web origin"))?;
    let host = page.host_str().unwrap_or_default();
    let rp_id = req["rpId"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !secrets::rp_id_valid_for(host, &rp_id) {
        return Err(fail(&format!(
            "{host} may not ask for a passkey of {rp_id}"
        )));
    }
    if !site_allowed(sites, &rp_id) {
        return Err(fail(&format!("Not armed for {rp_id}")));
    }
    let plain = |s: &str| {
        !s.is_empty()
            && s.len() <= 1024
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    let challenge = req["challenge"]
        .as_str()
        .filter(|s| plain(s))
        .ok_or_else(|| fail("Bad challenge"))?;
    let allow: Vec<String> = req["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| plain(s))
        .map(str::to_string)
        .collect();
    let request = broker
        .start(
            target.to_string(),
            rp_id.clone(),
            "assert",
            None,
            Signing {
                origin: Some(page.origin().ascii_serialization()),
                challenge: Some(challenge.to_string()),
                allow,
            },
        )
        .map_err(|e| fail(&e))?;
    info!(rp_id = %rp_id, "Agent passkey sign-in sent to the secret agent");
    let deadline = Instant::now() + PAGE_WAIT;
    while Instant::now() < deadline {
        match broker.status(request) {
            Some(Status::Ready { info: Some(answer) }) => return Ok((id, answer)),
            Some(Status::Ready { info: None }) => {
                return Err(fail("The secret agent sent no assertion"))
            }
            Some(Status::Failed { error }) => return Err(fail(&error)),
            Some(Status::Pending { .. }) => tokio::time::sleep(Duration::from_millis(300)).await,
            None => return Err(fail("The request was forgotten")),
        }
    }
    Err(fail("The secret agent did not answer"))
}

/// One CDP connection, written from several tasks.
struct Conn {
    out: mpsc::UnboundedSender<Value>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
}

impl Conn {
    fn send(&self, method: &str, params: Value, session: Option<&str>) -> oneshot::Receiver<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let _ = self.out.send(msg);
        rx
    }

    async fn call(&self, method: &str, params: Value, session: &str) -> Result<Value, String> {
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            self.send(method, params, Some(session)),
        )
        .await
        .map_err(|_| format!("{method}: no answer"))?
        .map_err(|_| format!("{method}: connection closed"))?;
        match answer.get("error") {
            Some(e) => Err(format!(
                "{method}: {}",
                e["message"].as_str().unwrap_or("error")
            )),
            None => Ok(answer["result"].clone()),
        }
    }

    fn answered(&self, id: u64, answer: Value) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(answer);
        }
    }

    /// Hooks a page or frame (and its child frames), then lets it run.
    async fn setup(&self, session: &str) {
        let steps = [
            ("Runtime.enable", json!({})), // bindings only fire once Runtime is enabled
            ("Runtime.addBinding", json!({ "name": BINDING })),
            ("Page.enable", json!({})),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": hook(), "runImmediately": true }),
            ),
            (
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            ),
        ];
        for (method, params) in steps {
            if let Err(e) = self.call(method, params, session).await {
                tracing::debug!("Agent passkey hook: {e}");
            }
        }
        let _ = self
            .call("Runtime.runIfWaitingForDebugger", json!({}), session)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_cover_their_subdomains_only() {
        let sites = vec!["microsoft.com".to_string(), "auth.brkn.lol".to_string()];
        assert!(site_allowed(&sites, "login.microsoft.com"));
        assert!(site_allowed(&sites, "microsoft.com"));
        assert!(site_allowed(&sites, "auth.brkn.lol"));
        assert!(!site_allowed(&sites, "brkn.lol"));
        assert!(!site_allowed(&sites, "evilmicrosoft.com"));
        assert!(site_allowed(&["*".to_string()], "any.test"));
        assert!(!site_allowed(&[], "any.test"));
    }

    #[test]
    fn the_hook_waits_as_long_as_the_node() {
        let h = hook();
        assert!(!h.contains("PAGE_WAIT_MS"));
        assert!(h.contains("330000"));
        assert!(h.contains(BINDING));
    }

    #[tokio::test]
    async fn requests_off_the_armed_sites_or_page_are_refused() {
        let broker = Broker::default();
        let req = |rp: &str| {
            json!({ "id": "abc", "rpId": rp, "challenge": "AAEC", "allow": [] }).to_string()
        };
        let sites = vec!["example.com".to_string()];
        let err = |r: Result<(Option<String>, Value), (Option<String>, String)>| r.unwrap_err();
        // Another site than armed for.
        let (id, e) = err(sign(
            &broker,
            "T",
            "https://login.other.test/",
            &req("other.test"),
            &sites,
        )
        .await);
        assert_eq!(id.as_deref(), Some("abc"));
        assert!(e.contains("Not armed"), "{e}");
        // A page asking for a passkey of a site it is not on.
        let (_, e) = err(sign(
            &broker,
            "T",
            "https://evil.test/",
            &req("example.com"),
            &sites,
        )
        .await);
        assert!(e.contains("may not ask"), "{e}");
        // Plain http.
        let (_, e) = err(sign(
            &broker,
            "T",
            "http://example.com/",
            &req("example.com"),
            &sites,
        )
        .await);
        assert!(e.contains("secure"), "{e}");
        // Allowed, but no secret agent polls this broker.
        let (_, e) = err(sign(
            &broker,
            "T",
            "https://login.example.com/",
            &req("example.com"),
            &sites,
        )
        .await);
        assert!(e.contains("No secret agent"), "{e}");
    }
}
