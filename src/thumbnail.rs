//! A recent small screenshot of the tab a session is showing, for the dashboard's
//! session cards. Taken on request and kept for a few seconds, so many viewers and
//! frequent refreshes cost one capture.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How long a capture is served again before a new one is taken.
const FRESH: Duration = Duration::from_secs(8);
/// Width of the images; the cards show them at most this wide.
const WIDTH: f64 = 640.0;

#[derive(Default)]
pub struct Thumbnails {
    cache: Mutex<HashMap<Uuid, (Instant, Vec<u8>)>>,
}

impl Thumbnails {
    pub fn fresh(&self, id: Uuid) -> Option<Vec<u8>> {
        let cache = self.cache.lock().unwrap();
        cache
            .get(&id)
            .filter(|(at, _)| at.elapsed() < FRESH)
            .map(|(_, jpeg)| jpeg.clone())
    }

    /// The last capture whatever its age (better than nothing while a new one fails).
    pub fn stale(&self, id: Uuid) -> Option<Vec<u8>> {
        self.cache
            .lock()
            .unwrap()
            .get(&id)
            .map(|(_, jpeg)| jpeg.clone())
    }

    pub fn store(&self, id: Uuid, jpeg: Vec<u8>) {
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3600));
        cache.insert(id, (Instant::now(), jpeg));
    }

    pub fn forget(&self, id: Uuid) {
        self.cache.lock().unwrap().remove(&id);
    }
}

/// A JPEG of the tab in front of the Chromium listening on `port`.
pub async fn chromium(http: &reqwest::Client, port: u16) -> Option<Vec<u8>> {
    capture(http, port, WIDTH, 60).await.map(|c| c.jpeg)
}

/// A picture of a tab, with the page it shows.
pub struct Capture {
    pub jpeg: Vec<u8>,
    pub url: Option<String>,
    pub title: Option<String>,
}

/// The tab in front of the Chromium listening on `port`, at most `width` wide.
pub async fn capture(
    http: &reqwest::Client,
    port: u16,
    width: f64,
    quality: u8,
) -> Option<Capture> {
    use base64::Engine as _;
    let targets: Vec<serde_json::Value> = http
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let page = crate::recorder::pick_visible(crate::recorder::page_candidates(targets)).await?;
    let ws_url = page.get("webSocketDebuggerUrl")?.as_str()?.to_string();
    let text = |key: &str| page.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let (url, title) = (text("url"), text("title"));
    let shot = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.ok()?;
        let mut next = 0u64;
        let mut ask = |method: &str, params: serde_json::Value| {
            next += 1;
            (
                next,
                serde_json::json!({ "id": next, "method": method, "params": params }),
            )
        };
        let (id, cmd) = ask("Page.getLayoutMetrics", serde_json::json!({}));
        ws.send(Message::Text(cmd.to_string().into())).await.ok()?;
        let metrics = answer(&mut ws, id).await?;
        let vp = metrics.get("cssVisualViewport")?;
        let (w, h) = (
            vp.get("clientWidth")?.as_f64()?,
            vp.get("clientHeight")?.as_f64()?,
        );
        if w < 1.0 || h < 1.0 {
            return None;
        }
        let scale = (width / w).min(1.0);
        let (id, cmd) = ask(
            "Page.captureScreenshot",
            serde_json::json!({
                "format": "jpeg",
                "quality": quality,
                "optimizeForSpeed": true,
                "clip": { "x": 0, "y": 0, "width": w, "height": h, "scale": scale },
            }),
        );
        ws.send(Message::Text(cmd.to_string().into())).await.ok()?;
        let data = answer(&mut ws, id).await?;
        let _ = ws.close(None).await;
        base64::engine::general_purpose::STANDARD
            .decode(data.get("data")?.as_str()?)
            .ok()
    };
    let jpeg = tokio::time::timeout(Duration::from_secs(8), shot)
        .await
        .ok()
        .flatten()?;
    Some(Capture { jpeg, url, title })
}

/// The `result` of the reply to command `id`.
async fn answer<S>(ws: &mut S, id: u64) -> Option<serde_json::Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(text) = msg {
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return v.get("result").cloned();
            }
        }
    }
    None
}
