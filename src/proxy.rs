use axum::extract::ws::{Message as ClientMsg, WebSocket as ClientWs};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as ServerMsg;
use tracing::debug;

use crate::idle::Activity;
use crate::recorder::{FrameSender, ProxyRecordingFrame};
use std::sync::Arc;

/// Where the frames a client receives come from, so only the visible tab is recorded.
pub struct FrameSource {
    /// The session's DevTools port.
    pub port: u16,
    /// The page the connection is to (a page endpoint); None on the browser
    /// endpoint, where the page of a frame follows from its CDP session.
    pub page: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn bridge_websockets(
    client_ws: ClientWs,
    server_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    frame_tx: Option<FrameSender>,
    frame_source: FrameSource,
    // The session's idle clock, and whether this client is a mere observer.
    activity: Option<(Arc<Activity>, bool)>,
    initial_url: Option<String>,
    initial_title: Option<String>,
    // Chromium clients: Playwright forces `prefers-color-scheme: light` on every
    // context it attaches to. The session's own appearance wins, so that is dropped.
    keep_session_appearance: bool,
    // A profile's session: clients that ask for a context of their own (Playwright
    // does, and its context lives in memory only) get the profile's own, so
    // what they log in to is kept with the profile.
    use_profile_context: bool,
) {
    let (client_tx, mut client_rx) = client_ws.split();
    let client_tx = tokio::sync::Mutex::new(client_tx);
    let (mut server_tx, mut server_rx) = server_ws.split();
    let alias = std::sync::Mutex::new(ContextAlias::default());
    let targets = std::sync::Mutex::new(TargetMap::default());
    let visibility = Visibility::new(frame_source.port);

    let client_to_server = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            if let (Some((activity, observer)), ClientMsg::Text(text)) = (&activity, &msg) {
                activity.note_command(text.as_str(), *observer);
            }
            let server_msg = match msg {
                ClientMsg::Text(text) => {
                    if frame_tx.is_some() {
                        targets.lock().unwrap().request(text.as_str());
                    }
                    let text = if use_profile_context {
                        alias
                            .lock()
                            .unwrap()
                            .request(text.as_str())
                            .map_or(text, Into::into)
                    } else {
                        text
                    };
                    let rewritten = keep_session_appearance
                        .then(|| without_color_scheme_override(text.as_str()))
                        .flatten();
                    ServerMsg::Text(rewritten.unwrap_or_else(|| text.as_str().to_owned()).into())
                }
                ClientMsg::Binary(bin) => ServerMsg::Binary(bin),
                ClientMsg::Ping(p) => ServerMsg::Ping(p),
                ClientMsg::Pong(p) => ServerMsg::Pong(p),
                ClientMsg::Close(frame) => {
                    let _ = server_tx
                        .send(ServerMsg::Close(frame.map(|f| {
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: f.code.into(),
                                reason: f.reason.as_str().into(),
                            }
                        })))
                        .await;
                    break;
                }
            };
            if let Err(e) = server_tx.send(server_msg).await {
                debug!(error = %e, "Forwarding to backend failed");
                break;
            }
        }
    };

    let server_to_client = async {
        let mut current_url = initial_url;
        let current_title = initial_title;

        while let Some(Ok(msg)) = server_rx.next().await {
            let msg = match (use_profile_context, msg) {
                (true, ServerMsg::Text(text)) => {
                    let rewritten = alias.lock().unwrap().event(text.as_str());
                    ServerMsg::Text(rewritten.map_or(text, Into::into))
                }
                (_, other) => other,
            };
            if frame_tx.is_some() {
                if let ServerMsg::Text(ref text) = msg {
                    targets.lock().unwrap().event(text.as_str());
                }
            }
            // Tap into screencast frames and navigation events for recording
            if let (Some(ref tx), ServerMsg::Text(ref text)) = (&frame_tx, &msg) {
                let s = text.as_str();
                if s.contains("\"Page.screencastFrame\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if val.get("method").and_then(|v| v.as_str())
                            == Some("Page.screencastFrame")
                        {
                            if let Some(data) = val.pointer("/params/data").and_then(|v| v.as_str())
                            {
                                // Only the tab in front is recorded: a screencast of a
                                // background tab (Playwright pages, a second handoff
                                // view) would show the wrong picture. A frame whose
                                // page cannot be told is kept.
                                let page = frame_source.page.clone().or_else(|| {
                                    targets.lock().unwrap().target_of(&val).map(str::to_string)
                                });
                                let show = match page {
                                    Some(ref id) => visibility.is_visible(id).await,
                                    None => true,
                                };
                                if show {
                                    let _ = tx.send(ProxyRecordingFrame {
                                        url: current_url.clone(),
                                        title: current_title.clone(),
                                        data: data.to_string(),
                                    });
                                }
                            }
                        }
                    }
                } else if s.contains("\"Page.frameNavigated\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if let Some(url) = val.pointer("/params/frame/url").and_then(|v| v.as_str())
                        {
                            if !url.is_empty() && url != "about:blank" {
                                current_url = Some(url.to_string());
                            }
                        }
                    }
                } else if s.contains("\"Page.navigatedWithinDocument\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if let Some(url) = val.pointer("/params/url").and_then(|v| v.as_str()) {
                            if !url.is_empty() && url != "about:blank" {
                                current_url = Some(url.to_string());
                            }
                        }
                    }
                }
            }

            let client_msg = match msg {
                ServerMsg::Text(text) => ClientMsg::Text(text.as_str().into()),
                ServerMsg::Binary(bin) => ClientMsg::Binary(bin),
                ServerMsg::Ping(p) => ClientMsg::Ping(p),
                ServerMsg::Pong(p) => ClientMsg::Pong(p),
                ServerMsg::Close(frame) => {
                    let _ = client_tx
                        .lock()
                        .await
                        .send(ClientMsg::Close(frame.map(|f| {
                            axum::extract::ws::CloseFrame {
                                code: f.code.into(),
                                reason: f.reason.as_str().into(),
                            }
                        })))
                        .await;
                    break;
                }
                ServerMsg::Frame(_) => continue,
            };
            if let Err(e) = client_tx.lock().await.send(client_msg).await {
                debug!(error = %e, "Forwarding to client failed");
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_server => {},
        _ = server_to_client => {},
    }
}

/// The page each CDP session of a browser connection is attached to, learnt from
/// `Target.attachedToTarget` events and the answers to `Target.attachToTarget`.
#[derive(Default)]
struct TargetMap {
    /// CDP session id -> target id (pages only).
    sessions: std::collections::HashMap<String, String>,
    /// Request id of a `Target.attachToTarget` -> target id.
    attaching: std::collections::HashMap<u64, String>,
}

impl TargetMap {
    fn request(&mut self, text: &str) {
        if !text.contains("\"Target.attachToTarget\"") {
            return;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        if let (Some(id), Some(target)) = (msg["id"].as_u64(), msg["params"]["targetId"].as_str()) {
            if self.attaching.len() < 1024 {
                self.attaching.insert(id, target.to_string());
            }
        }
    }

    fn event(&mut self, text: &str) {
        let attached = text.contains("\"Target.attachedToTarget\"");
        let detached = text.contains("\"Target.detachedFromTarget\"");
        let answer = !self.attaching.is_empty() && text.contains("\"sessionId\"");
        if !(attached || detached || answer) {
            return;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        match msg["method"].as_str() {
            Some("Target.attachedToTarget") => {
                let info = &msg["params"]["targetInfo"];
                if let (Some(session), Some(target), Some("page")) = (
                    msg["params"]["sessionId"].as_str(),
                    info["targetId"].as_str(),
                    info["type"].as_str(),
                ) {
                    self.sessions
                        .insert(session.to_string(), target.to_string());
                }
            }
            Some("Target.detachedFromTarget") => {
                if let Some(session) = msg["params"]["sessionId"].as_str() {
                    self.sessions.remove(session);
                }
            }
            None => {
                if let (Some(id), Some(session)) =
                    (msg["id"].as_u64(), msg["result"]["sessionId"].as_str())
                {
                    if let Some(target) = self.attaching.remove(&id) {
                        self.sessions.insert(session.to_string(), target);
                    }
                }
            }
            _ => {}
        }
    }

    /// The page a message of the browser connection comes from, by its
    /// top-level `sessionId`.
    fn target_of(&self, msg: &serde_json::Value) -> Option<&str> {
        self.sessions
            .get(msg["sessionId"].as_str()?)
            .map(String::as_str)
    }
}

/// How long a page's visibility is trusted before it is asked again.
const VISIBLE_TTL: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a page is the one in front (`document.visibilityState`), asked of
/// the browser itself and remembered for a moment.
struct Visibility {
    port: u16,
    seen: Arc<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, bool)>>>,
}

impl Visibility {
    fn new(port: u16) -> Self {
        Self {
            port,
            seen: Arc::default(),
        }
    }

    /// The first time for a page the answer is waited for; after that the last
    /// answer is used while a fresh one is fetched in the background.
    async fn is_visible(&self, page: &str) -> bool {
        let known = self.seen.lock().unwrap().get(page).copied();
        match known {
            Some((at, v)) if at.elapsed() < VISIBLE_TTL => v,
            Some((_, v)) => {
                // Marked fresh now so frames do not start a probe each.
                self.seen
                    .lock()
                    .unwrap()
                    .insert(page.to_string(), (std::time::Instant::now(), v));
                let (seen, url, page) = (self.seen.clone(), self.url(page), page.to_string());
                tokio::spawn(async move {
                    let v = crate::recorder::is_visible(&url).await;
                    seen.lock()
                        .unwrap()
                        .insert(page, (std::time::Instant::now(), v));
                });
                v
            }
            None => {
                let v = crate::recorder::is_visible(&self.url(page)).await;
                let mut seen = self.seen.lock().unwrap();
                if seen.len() > 256 {
                    seen.clear();
                }
                seen.insert(page.to_string(), (std::time::Instant::now(), v));
                v
            }
        }
    }

    fn url(&self, page: &str) -> String {
        format!("ws://127.0.0.1:{}/devtools/page/{page}", self.port)
    }
}

/// What the proxy knows about the contexts of a client of a profile's session.
///
/// Playwright makes a browser context of its own on connecting (`Target.createBrowserContext`)
/// and keeps its pages there: it lives in memory, so nothing a page logs in to would
/// be kept with the profile. The pages are therefore created in the profile's own
/// (default) context, and the two ids are swapped on the way: the client's context
/// id stands for the default one.
#[derive(Default)]
struct ContextAlias {
    /// Id of the client's own `Target.createBrowserContext` request.
    create_id: u64,
    /// The context the client got (its stand-in for the default one).
    client: Option<String>,
    /// The profile's own context, seen on its first page.
    default: Option<String>,
}

impl ContextAlias {
    /// A request of the client: the one that creates a context is noted; calls aimed at
    /// the client's context are sent to the default one (no id), except its disposal.
    fn request(&mut self, text: &str) -> Option<String> {
        if !text.contains("rowserContext")
            || text.contains("\"sessionId\"") && !text.contains("Target.createTarget")
        {
            return None;
        }
        let mut msg: serde_json::Value = serde_json::from_str(text).ok()?;
        let method = msg["method"].as_str()?.to_string();
        if method == "Target.createBrowserContext" {
            self.create_id = msg["id"].as_u64().unwrap_or(0);
            return None;
        }
        let client = self.client.as_deref()?;
        if method == "Target.disposeBrowserContext"
            || msg["params"]["browserContextId"].as_str() != Some(client)
        {
            return None;
        }
        msg["params"].as_object_mut()?.remove("browserContextId");
        Some(msg.to_string())
    }

    /// A message of the browser: the answer to the context request is noted; the
    /// profile's own context id, wherever it appears, becomes the client's.
    fn event(&mut self, text: &str) -> Option<String> {
        if self.create_id != 0 && self.client.is_none() && text.contains("browserContextId") {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) {
                if msg["id"].as_u64() == Some(self.create_id) {
                    self.client = msg["result"]["browserContextId"]
                        .as_str()
                        .map(str::to_string);
                    return None;
                }
            }
        }
        let client = self.client.clone()?;
        const KEY: &str = "\"browserContextId\":\"";
        if !text.contains(KEY) {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut changed = false;
        while let Some(at) = rest.find(KEY) {
            let value_start = at + KEY.len();
            out.push_str(&rest[..value_start]);
            rest = &rest[value_start..];
            let end = rest.find('"').unwrap_or(rest.len());
            let id = &rest[..end];
            // The first context that is not the client's is the profile's own.
            if id != client && self.default.is_none() {
                self.default = Some(id.to_string());
            }
            if self.default.as_deref() == Some(id) {
                out.push_str(&client);
                changed = true;
            } else {
                out.push_str(id);
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        changed.then_some(out)
    }
}

/// A DevTools `Emulation.setEmulatedMedia` call with its `prefers-color-scheme`
/// override emptied (no emulation), or None when the message has none.
fn without_color_scheme_override(text: &str) -> Option<String> {
    if !text.contains("setEmulatedMedia") || !text.contains("prefers-color-scheme") {
        return None;
    }
    let mut msg: serde_json::Value = serde_json::from_str(text).ok()?;
    if msg.get("method")?.as_str()? != "Emulation.setEmulatedMedia" {
        return None;
    }
    let mut changed = false;
    for feature in msg.pointer_mut("/params/features")?.as_array_mut()? {
        if feature.get("name").and_then(|n| n.as_str()) == Some("prefers-color-scheme") {
            feature["value"] = serde_json::Value::String(String::new());
            changed = true;
        }
    }
    changed.then(|| msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clients_context_stands_for_the_profiles_own() {
        let mut alias = ContextAlias::default();
        // Its request to create a context is sent on; the answer tells its id.
        assert!(alias
            .request(r#"{"id":4,"method":"Target.createBrowserContext","params":{"disposeOnDetach":true}}"#)
            .is_none());
        assert!(alias
            .event(r#"{"id":4,"result":{"browserContextId":"CLIENT"}}"#)
            .is_none());
        // Its pages are created in the default context: no id.
        let sent = alias
            .request(r#"{"id":6,"method":"Target.createTarget","params":{"url":"about:blank","browserContextId":"CLIENT"}}"#)
            .unwrap();
        assert!(!sent.contains("browserContextId"), "{sent}");
        // The page that comes up carries the profile's id: the client sees its own.
        let seen = alias
            .event(r#"{"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"t","type":"page","browserContextId":"PROFILE"}}}"#)
            .unwrap();
        assert!(
            seen.contains(r#""browserContextId":"CLIENT""#) && !seen.contains("PROFILE"),
            "{seen}"
        );
        // Later messages, and calls about the client's context, follow the same rule.
        assert!(alias
            .event(r#"{"method":"Target.attachedToTarget","params":{"targetInfo":{"browserContextId":"PROFILE"}}}"#)
            .unwrap()
            .contains("CLIENT"));
        assert!(alias
            .request(r#"{"id":7,"method":"Browser.setDownloadBehavior","params":{"behavior":"allow","browserContextId":"CLIENT"}}"#)
            .unwrap()
            .contains(r#""behavior":"allow"}"#));
        // Its disposal still goes through (the real, empty context).
        assert!(alias
            .request(r#"{"id":9,"method":"Target.disposeBrowserContext","params":{"browserContextId":"CLIENT"}}"#)
            .is_none());
        // Nothing to rewrite in unrelated messages.
        assert!(alias
            .event(r#"{"method":"Page.frameNavigated","params":{}}"#)
            .is_none());
        assert!(alias
            .request(r#"{"id":8,"method":"Page.navigate","params":{}}"#)
            .is_none());
    }

    #[test]
    fn frames_are_traced_to_their_page() {
        let mut map = TargetMap::default();
        map.event(r#"{"method":"Target.attachedToTarget","params":{"sessionId":"S1","targetInfo":{"targetId":"T1","type":"page"}}}"#);
        map.event(r#"{"method":"Target.attachedToTarget","params":{"sessionId":"W","targetInfo":{"targetId":"TW","type":"worker"}}}"#);
        map.request(r#"{"id":5,"method":"Target.attachToTarget","params":{"targetId":"T2","flatten":true}}"#);
        map.event(r#"{"id":5,"result":{"sessionId":"S2"}}"#);
        let frame = |s: &str| serde_json::json!({"method":"Page.screencastFrame","sessionId":s});
        assert_eq!(map.target_of(&frame("S1")), Some("T1"));
        assert_eq!(map.target_of(&frame("S2")), Some("T2"));
        assert_eq!(map.target_of(&frame("W")), None, "not a page");
        assert_eq!(map.target_of(&frame("nope")), None);
        assert_eq!(map.target_of(&serde_json::json!({"method":"x"})), None);
        map.event(r#"{"method":"Target.detachedFromTarget","params":{"sessionId":"S1"}}"#);
        assert_eq!(map.target_of(&frame("S1")), None);
    }

    #[test]
    fn playwrights_light_override_is_cleared() {
        let out = without_color_scheme_override(
            r#"{"id":7,"method":"Emulation.setEmulatedMedia","params":{"media":"","features":[{"name":"prefers-color-scheme","value":"light"},{"name":"prefers-reduced-motion","value":"reduce"}]}}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["params"]["features"][0]["value"], "");
        assert_eq!(v["params"]["features"][1]["value"], "reduce");
        assert!(without_color_scheme_override(r#"{"id":1,"method":"Page.navigate"}"#).is_none());
        assert!(without_color_scheme_override(
            r#"{"id":1,"method":"Emulation.setEmulatedMedia","params":{"features":[{"name":"prefers-reduced-motion","value":"reduce"}]}}"#
        )
        .is_none());
    }
}
