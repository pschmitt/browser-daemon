//! Text typed into a page over CDP, for `POST /api/pages/{id}/insert-text`:
//! `browser-daemon mcp` fills vault secrets this way, so they never pass through
//! a Playwright script (Playwright MCP echoes every script it runs). The text is
//! never logged.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// Inserts `text` into the focused element of the page at `ws_url` (as if
/// pasted), then presses Enter when asked.
pub async fn insert_text(ws_url: &str, text: &str, enter: bool) -> Result<(), String> {
    let work = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("Could not reach the page: {e}"))?;
        let mut calls = vec![json!({ "method": "Input.insertText", "params": { "text": text } })];
        if enter {
            for kind in ["keyDown", "keyUp"] {
                calls.push(json!({
                    "method": "Input.dispatchKeyEvent",
                    "params": {
                        "type": kind, "key": "Enter", "code": "Enter",
                        "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13,
                        "text": if kind == "keyDown" { "\r" } else { "" },
                    }
                }));
            }
        }
        for (i, mut call) in calls.into_iter().enumerate() {
            let id = i as u64 + 1;
            call["id"] = json!(id);
            ws.send(Message::Text(call.to_string().into()))
                .await
                .map_err(|e| format!("Could not talk to the page: {e}"))?;
            loop {
                let msg = ws
                    .next()
                    .await
                    .ok_or("The page went away")?
                    .map_err(|e| format!("Could not talk to the page: {e}"))?;
                let Message::Text(t) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                if v["id"].as_u64() != Some(id) {
                    continue; // an event
                }
                if let Some(e) = v.get("error") {
                    return Err(format!(
                        "The page refused the input: {}",
                        e["message"].as_str().unwrap_or("error")
                    ));
                }
                break;
            }
        }
        let _ = ws.close(None).await;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(15), work)
        .await
        .map_err(|_| "The page did not answer in time".to_string())?
}
