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
) -> Result<(Option<String>, String, bool), String> {
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
    let mut out_of_process = false;
    if matches!(name, "IFRAME" | "FRAME" | "OBJECT" | "EMBED") {
        // Focus is inside another document: it and every frame in it must be allowed.
        let Some(frame) = node.pointer("/node/frameId").and_then(Value::as_str) else {
            return Err("Could not tell which frame has focus".into());
        };
        if require_frame {
            if frame_present(root, frame) {
                card_frame_allowed(root, frame, frame_allowed)?;
            } else {
                out_of_process = true;
            }
        } else if !frame_tree_allowed(root, frame, frame_allowed) {
            return Err("The focused field is in a frame of another site".into());
        }
        in_frame = Some(frame.to_string());
    }
    if require_frame && in_frame.is_none() {
        return Err("The focused field is not in an authorized frame".into());
    }
    Ok((in_frame, top, out_of_process))
}

fn frame_present(node: &Value, id: &str) -> bool {
    node["frame"]["id"].as_str() == Some(id)
        || node["childFrames"]
            .as_array()
            .is_some_and(|children| children.iter().any(|child| frame_present(child, id)))
}

/// Only a directly embedded iframe with the configured origin may use the
/// separate Chromium target path. Never trust a target from another page.
fn card_target_allowed(
    info: &Value,
    frame: &str,
    top: &str,
    allowed: &impl Fn(&str) -> bool,
) -> bool {
    let target = &info["targetInfo"];
    target["targetId"].as_str() == Some(frame)
        && target["type"].as_str() == Some("iframe")
        && target["parentFrameId"].as_str() == Some(top)
        && target["url"].as_str().is_some_and(allowed)
        && !frame.is_empty()
        && frame.bytes().all(|b| b.is_ascii_hexdigit())
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
        let (in_frame, top, out_of_process) =
            check_focus(&mut cdp, &page_allowed, &frame_allowed, require_frame).await?;
        let protected = if out_of_process {
            let frame = in_frame
                .as_deref()
                .ok_or("Could not tell which frame has focus")?;
            let info = cdp
                .call("Target.getTargetInfo", json!({"targetId": frame}))
                .await?;
            if !card_target_allowed(&info, frame, &top, &frame_allowed) {
                return Err("The focused payment frame is not authorized for this page".into());
            }
            let (prefix, _) = ws_url.rsplit_once('/').ok_or("Invalid page connection")?;
            let mut child = Cdp::connect(&format!("{prefix}/{frame}")).await?;
            let tree = child.call("Page.getFrameTree", json!({})).await?;
            card_frame_allowed(&tree["frameTree"], frame, &frame_allowed)?;
            let protected = guard_focused(&mut child, frame).await;
            child.close().await;
            let (focused, current_top, still_out_of_process) =
                check_focus(&mut cdp, &page_allowed, &frame_allowed, true).await?;
            let info = cdp
                .call("Target.getTargetInfo", json!({"targetId": frame}))
                .await?;
            if focused.as_deref() != Some(frame)
                || current_top != top
                || !still_out_of_process
                || !card_target_allowed(&info, frame, &top, &frame_allowed)
            {
                return Err("The focused payment frame changed before input".into());
            }
            protected
        } else {
            guard_focused(&mut cdp, in_frame.as_deref().unwrap_or(&top)).await
        };
        if !protected {
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

    #[test]
    fn out_of_process_target_must_be_the_authorized_direct_child() {
        let allowed =
            |url: &str| crate::secrets::exact_https_origin(url, "https://payments.example");
        let good = json!({"targetInfo": {"targetId": "ABCD12", "type": "iframe", "parentFrameId": "PAGE", "url": "https://payments.example/frame"}});
        assert!(card_target_allowed(&good, "ABCD12", "PAGE", &allowed));
        for (key, value) in [
            ("parentFrameId", "OTHER"),
            ("type", "page"),
            ("targetId", "BEEF"),
            ("url", "https://evil.example/"),
        ] {
            let mut bad = good.clone();
            bad["targetInfo"][key] = json!(value);
            assert!(
                !card_target_allowed(&bad, "ABCD12", "PAGE", &allowed),
                "{key}"
            );
        }
        assert!(!card_target_allowed(&good, "../escape", "PAGE", &allowed));
    }

    #[tokio::test]
    #[ignore = "requires Chromium on PATH"]
    async fn card_fill_reaches_an_authorized_out_of_process_frame() {
        use axum::{routing::get, Router};
        use std::process::Stdio;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/",
                get(move || async move {
                    axum::response::Html(format!(
                        r#"<iframe src="http://localhost:{port}/field"></iframe>"#
                    ))
                }),
            )
            .route(
                "/field",
                get(|| async { axum::response::Html("<input id='card'>") }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut browser = tokio::process::Command::new("chromium")
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--no-first-run",
                "--site-per-process",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", directory.path().display()))
            .arg(format!("http://127.0.0.1:{port}/"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = reqwest::Client::new();
        let (ws_url, frame) = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(file) =
                    tokio::fs::read_to_string(directory.path().join("DevToolsActivePort")).await
                {
                    let debug_port = file.lines().next().unwrap();
                    if let Ok(response) = client
                        .get(format!("http://127.0.0.1:{debug_port}/json/list"))
                        .send()
                        .await
                    {
                        if let Ok(targets) = response.json::<Vec<Value>>().await {
                            let page = targets
                                .iter()
                                .find(|t| t["type"] == "page")
                                .and_then(|t| t["webSocketDebuggerUrl"].as_str());
                            if let Some(ws) = page {
                                if let Ok(mut cdp) = Cdp::connect(ws).await {
                                    if let Ok(info) = cdp.call("Target.getTargets", json!({})).await
                                    {
                                        if let Some(frame) = info["targetInfos"]
                                            .as_array()
                                            .and_then(|a| {
                                                a.iter().find(|t| {
                                                    t["type"] == "iframe"
                                                        && t["url"].as_str().is_some_and(|u| {
                                                            u.starts_with("http://localhost:")
                                                        })
                                                })
                                            })
                                            .and_then(|t| t["targetId"].as_str())
                                        {
                                            break (ws.to_string(), frame.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let (prefix, _) = ws_url.rsplit_once('/').unwrap();
        let mut child = Cdp::connect(&format!("{prefix}/{frame}")).await.unwrap();
        child
            .call(
                "Runtime.evaluate",
                json!({"expression":"document.querySelector('#card').focus()"}),
            )
            .await
            .unwrap();
        let page = |url: &str| url.starts_with(&format!("http://127.0.0.1:{port}/"));
        let payment = |url: &str| url.starts_with(&format!("http://localhost:{port}/"));
        assert!(insert_text(&ws_url, "fixture", false, page, page, false)
            .await
            .is_err());
        assert!(
            insert_text(&ws_url, "fixture", false, page, |_| false, true)
                .await
                .is_err()
        );
        insert_text(&ws_url, "fixture", false, page, payment, true)
            .await
            .unwrap();
        let value = child
            .call(
                "Runtime.evaluate",
                json!({"expression":"document.querySelector('#card').value", "returnByValue":true}),
            )
            .await
            .unwrap();
        assert_eq!(value["result"]["value"], "fixture");
        child.close().await;
        browser.kill().await.unwrap();
        server.abort();
    }
}
