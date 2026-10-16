//! Which sessions and tabs are playing sound, for a speaker on them in the
//! dashboard and handoff view. A session is audible while a stream plays into
//! its PulseAudio sink. Which of its tabs that is: the only one, or else the ones
//! with media playing (checked only while the session is audible).

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::{AppState, DevToolsTarget};

/// Unmuted audio or video playing in the page or a same-origin frame.
const PLAYING_JS: &str = r#"(() => {
  const playing = (d) => [...d.querySelectorAll("video, audio")]
    .some((m) => !m.paused && !m.muted && m.volume > 0 && m.readyState > 2);
  const walk = (d) => {
    if (playing(d)) return true;
    for (const f of d.querySelectorAll("iframe")) {
      try { if (f.contentDocument && walk(f.contentDocument)) return true; } catch { /* cross-origin */ }
    }
    return false;
  };
  return walk(document);
})()"#;

const TICK: Duration = Duration::from_millis(1500);

/// Evaluate `PLAYING_JS` in a Chromium page through its own DevTools socket.
async fn chromium_page_playing(port: u16, page: &str) -> bool {
    let check = async {
        let url = format!("ws://127.0.0.1:{port}/devtools/page/{page}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.ok()?;
        let req = json!({ "id": 1, "method": "Runtime.evaluate", "params": { "expression": PLAYING_JS, "returnByValue": true } });
        ws.send(Message::Text(req.to_string().into())).await.ok()?;
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(&text).ok()?;
            if v["id"] == 1 {
                let _ = ws.close(None).await;
                return v["result"]["result"]["value"].as_bool();
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(1), check)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Keep `audible` flags on sessions and the set of audible tabs current.
pub fn spawn_monitor(state: AppState) {
    let Some(cfg) = state.config.audio.clone() else {
        return;
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(sinks) = crate::audio::audible_sinks(&cfg).await else {
                continue;
            };
            let sessions: Vec<_> = state.sessions.read().await.values().cloned().collect();
            let mut pages = HashSet::new();
            for s in sessions {
                let on = s.audio_sink.as_ref().is_some_and(|n| sinks.contains(n));
                s.audible.store(on, Ordering::Relaxed);
                if !on {
                    continue;
                }
                if let Some(ref hub) = s.bidi {
                    let contexts = hub.contexts().await;
                    if let [only] = contexts.as_slice() {
                        pages.insert(only.id.clone());
                        continue;
                    }
                    for c in contexts {
                        // A hung tab must not stall the monitor for the hub's 30 s.
                        let r = tokio::time::timeout(
                            Duration::from_secs(2),
                            hub.call(
                                "script.evaluate",
                                json!({ "expression": PLAYING_JS, "target": { "context": c.id }, "awaitPromise": false }),
                            ),
                        )
                        .await;
                        if r.is_ok_and(|v| v.is_ok_and(|v| v["result"]["value"] == true)) {
                            pages.insert(c.id);
                        }
                    }
                    continue;
                }
                let url = format!("http://127.0.0.1:{}/json/list", s.port);
                let Ok(res) = state.http_client.get(&url).send().await else {
                    continue;
                };
                let targets: Vec<DevToolsTarget> = res.json().await.unwrap_or_default();
                let tabs: Vec<_> = targets
                    .into_iter()
                    .filter(|t| t.target_type == "page")
                    .collect();
                if let [only] = tabs.as_slice() {
                    pages.insert(only.id.clone());
                    continue;
                }
                for t in tabs {
                    if chromium_page_playing(s.port, &t.id).await {
                        pages.insert(t.id);
                    }
                }
            }
            if let Ok(mut audible) = state.audible_pages.write() {
                *audible = pages;
            }
        }
    });
}
