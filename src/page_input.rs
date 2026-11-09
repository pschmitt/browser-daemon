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

use crate::autofill::{frames, secret_css, Cdp, GUARD};

/// Whether the frame tree and the focused element are on allowed sites.
/// `allowed` judges a frame's URL. Gives the id of the frame the focused field
/// is in when that is a child frame (None: the top frame).
async fn check_focus(
    cdp: &mut Cdp,
    page_allowed: &impl Fn(&str) -> bool,
    frame_allowed: &impl Fn(&str) -> bool,
    require_frame: bool,
) -> Result<(Option<String>, String), String> {
    let tree = cdp.call("Page.getFrameTree", json!({})).await?;
    let root = &tree["frameTree"];
    if !root["frame"]["url"].as_str().is_some_and(page_allowed) {
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
    let top = root["frame"]["id"].as_str().unwrap_or_default().to_string();
    let mut in_frame = None;
    if matches!(name, "IFRAME" | "FRAME" | "OBJECT" | "EMBED") {
        // Focus is inside another document: it and every frame in it must be allowed.
        let Some(frame) = node.pointer("/node/frameId").and_then(Value::as_str) else {
            return Err("Could not tell which frame has focus".into());
        };
        if require_frame {
            card_frame_allowed(root, frame, frame_allowed)?;
        } else if !frame_tree_allowed(root, frame, frame_allowed) {
            return Err("The focused field is in a frame of another site".into());
        }
        in_frame = Some(frame.to_string());
    }
    if require_frame && in_frame.is_none() {
        return Err("The focused field is not in an authorized frame".into());
    }
    Ok((in_frame, top))
}

/// Reject an unknown or unauthorized frame without revealing its path or query.
fn card_frame_allowed(
    node: &Value,
    id: &str,
    allowed: &impl Fn(&str) -> bool,
) -> Result<(), String> {
    fn find<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
        if node["frame"]["id"].as_str() == Some(id) {
            return Some(node);
        }
        node["childFrames"]
            .as_array()?
            .iter()
            .find_map(|child| find(child, id))
    }
    fn check(node: &Value, allowed: &impl Fn(&str) -> bool) -> Result<(), String> {
        let url = node["frame"]["url"]
            .as_str()
            .ok_or("Browser did not expose a payment frame URL")?;
        if !allowed(url) {
            let origin = url::Url::parse(url)
                .ok()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_else(|| "unknown".to_string());
            return Err(format!(
                "Payment frame origin {origin} is not authorized for this card"
            ));
        }
        if let Some(children) = node["childFrames"].as_array() {
            for child in children {
                check(child, allowed)?;
            }
        }
        Ok(())
    }
    check(
        find(node, id)
            .ok_or("Browser did not expose the focused payment frame for authorization")?,
        allowed,
    )
}

/// Script for `guard_focused`: marks the focused field (looking into shadow
/// roots) and styles it, with the `GUARD` function of autofill.rs.
fn guard_expression() -> String {
    format!(
        r#"(() => {{
  let el = document.activeElement;
  while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
  if (!el || !("value" in el)) return false;
  ({GUARD})(el, {});
  return true;
}})()"#,
        json!(secret_css(false))
    )
}

/// Before the text goes in: marks the focused field `data-bd-secret` and styles
/// it to show dots whatever its `type` becomes (see `secret_css`), from an
/// isolated world of its frame. A field inside a frame nested deeper than the
/// focused one stays unmarked. Returns whether a field was marked.
async fn guard_focused(cdp: &mut Cdp, frame: &str) -> bool {
    let Ok(world) = cdp
        .call(
            "Page.createIsolatedWorld",
            json!({ "frameId": frame, "worldName": "bd-secret" }),
        )
        .await
    else {
        return false;
    };
    let Some(ctx) = world["executionContextId"].as_u64() else {
        return false;
    };
    cdp.call(
        "Runtime.evaluate",
        json!({ "expression": guard_expression(), "contextId": ctx, "returnByValue": true }),
    )
    .await
    .is_ok_and(|r| r.pointer("/result/value").and_then(Value::as_bool) == Some(true))
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
    page_allowed: impl Fn(&str) -> bool,
    frame_allowed: impl Fn(&str) -> bool,
    require_frame: bool,
) -> Result<(), String> {
    let work = async {
        let mut cdp = Cdp::connect(ws_url).await?;
        let (in_frame, top) =
            check_focus(&mut cdp, &page_allowed, &frame_allowed, require_frame).await?;
        if !guard_focused(&mut cdp, in_frame.as_deref().unwrap_or(&top)).await {
            if require_frame {
                return Err("Could not protect the focused field".into());
            }
            tracing::warn!("Could not mask the field a vault value is typed into");
        }
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
    fn the_guard_script_carries_the_style_as_a_json_string() {
        let e = guard_expression();
        assert!(e.contains(r#"-webkit-text-security:disc !important}""#));
        assert!(e.contains("data-bd-secret"));
    }

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

    #[test]
    fn card_frame_is_allowed_only_for_the_configured_payment_origin() {
        let tree = json!({ "frame": { "id": "merchant", "url": "https://shop.example/checkout" }, "childFrames": [
            { "frame": { "id": "payment", "url": "https://payments.example/hosted-field" } },
            { "frame": { "id": "other", "url": "https://evil.example/hosted-field" } }
        ] });
        let card = |url: &str| crate::secrets::exact_https_origin(url, "https://payments.example");
        let login = |url: &str| {
            crate::secrets::url_matches(
                url,
                &[crate::secrets::Uri {
                    uri: "https://shop.example".into(),
                    match_type: Some(1),
                }],
            )
        };
        assert!(card_frame_allowed(&tree, "payment", &card).is_ok());
        assert_eq!(
            card_frame_allowed(&tree, "other", &card).unwrap_err(),
            "Payment frame origin https://evil.example is not authorized for this card"
        );
        assert!(!frame_tree_allowed(&tree, "payment", &login));
        assert!(card_frame_allowed(&tree, "missing", &card)
            .unwrap_err()
            .contains("did not expose"));
        let nested = json!({ "frame": { "id": "payment", "url": "https://payments.example/hosted-field" }, "childFrames": [ { "frame": { "id": "nested", "url": "https://evil.example/path?token=private" } } ] });
        let error = card_frame_allowed(&nested, "payment", &card).unwrap_err();
        assert!(error.contains("https://evil.example"));
        assert!(!error.contains("private"));
        assert!(
            card_frame_allowed(&json!({ "frame": { "id": "payment" } }), "payment", &card).is_err()
        );
    }
}
