//! Text typed into a page over CDP, for `POST /api/pages/{id}/insert-text`:
//! `browser-daemon mcp` fills vault secrets this way, so they never pass through
//! a Playwright script (Playwright MCP echoes every script it runs). The text is
//! never logged.
//!
//! `Input.insertText` goes to whatever has focus, so before typing, on the same
//! connection, the page and the focused frame are checked against the sites
//! the entry is for: a navigation between the caller's check and the typing, or
//! a field inside a frame of another site, gets nothing.

use std::time::Duration;

use serde_json::{json, Value};

use crate::autofill::{frames, Cdp};

/// Whether the frame tree and the focused element are on allowed sites.
/// `allowed` judges a frame's URL.
async fn check_focus(cdp: &mut Cdp, allowed: &impl Fn(&str) -> bool) -> Result<(), String> {
    let tree = cdp.call("Page.getFrameTree", json!({})).await?;
    let root = &tree["frameTree"];
    if !root["frame"]["url"].as_str().is_some_and(allowed) {
        return Err("The page is not on the entry's site anymore".into());
    }
    let focus = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": "document.activeElement" }),
        )
        .await?;
    let Some(object) = focus.pointer("/result/objectId").and_then(Value::as_str) else {
        return Err("Could not tell which field has focus".into());
    };
    let node = cdp
        .call("DOM.describeNode", json!({ "objectId": object }))
        .await?;
    let name = node
        .pointer("/node/nodeName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(name, "IFRAME" | "FRAME" | "OBJECT" | "EMBED") {
        // Focus is inside another document: it and every frame in it must be allowed.
        let Some(frame) = node.pointer("/node/frameId").and_then(Value::as_str) else {
            return Err("Could not tell which frame has focus".into());
        };
        if !frame_tree_allowed(root, frame, allowed) {
            return Err("The focused field is in a frame of another site".into());
        }
    }
    Ok(())
}

/// Whether the frame `id` of the tree, and all its descendants, pass `allowed`.
fn frame_tree_allowed(node: &Value, id: &str, allowed: &impl Fn(&str) -> bool) -> bool {
    if node["frame"]["id"].as_str() == Some(id) {
        let mut all = Vec::new();
        frames(node, "id", "childFrames", &mut all);
        return all.iter().all(|(_, url)| allowed(url));
    }
    node["childFrames"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| frame_tree_allowed(c, id, allowed))
}

/// Inserts `text` into the focused element of the page at `ws_url` (as if
/// pasted), then presses Enter when asked, provided the page and the focused
/// frame are on sites `allowed` accepts.
pub async fn insert_text(
    ws_url: &str,
    text: &str,
    enter: bool,
    allowed: impl Fn(&str) -> bool,
) -> Result<(), String> {
    let work = async {
        let mut cdp = Cdp::connect(ws_url).await?;
        check_focus(&mut cdp, &allowed).await?;
        let mut calls = vec![("Input.insertText", json!({ "text": text }))];
        if enter {
            for kind in ["keyDown", "keyUp"] {
                calls.push((
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": kind, "key": "Enter", "code": "Enter",
                        "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13,
                        "text": if kind == "keyDown" { "\r" } else { "" },
                    }),
                ));
            }
        }
        for (method, params) in calls {
            cdp.call(method, params)
                .await
                .map_err(|e| format!("The page refused the input: {e}"))?;
        }
        cdp.close().await;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(15), work)
        .await
        .map_err(|_| "The page did not answer in time".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_focused_frame_and_everything_in_it_must_be_allowed() {
        let tree = json!({ "frame": { "id": "A", "url": "https://x.test/" }, "childFrames": [
            { "frame": { "id": "B", "url": "https://login.x.test/" }, "childFrames": [
                { "frame": { "id": "C", "url": "https://ads.evil.test/" } } ] },
            { "frame": { "id": "D", "url": "https://sso.x.test/" } } ] });
        let ok = |u: &str| u.contains("x.test") && !u.contains("evil");
        assert!(frame_tree_allowed(&tree, "D", &ok));
        assert!(
            !frame_tree_allowed(&tree, "B", &ok),
            "B holds an evil frame"
        );
        assert!(!frame_tree_allowed(&tree, "C", &ok));
        assert!(!frame_tree_allowed(&tree, "missing", &ok));
        assert!(frame_tree_allowed(&tree, "A", &|_| true));
    }
}
