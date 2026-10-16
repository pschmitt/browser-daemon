//! Logins filled from the vault, as the Bitwarden extension fills them: the live
//! view's vault button lists the entries for the page's site (via the passkey
//! helper, which holds the vault) and the helper sends the picked one here
//! (`POST /api/pages/{id}/autofill`). The values go into the page's login form
//! from an isolated world (Chromium) or sandbox (Firefox), only in frames on a
//! site the entry is saved for. They are never logged and never reach the view.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::bidi::BidiHub;

/// What to fill. No Debug: the values must not end up in a log line.
#[derive(Deserialize, Default)]
pub struct Creds {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub totp: Option<String>,
}

/// Runs in the page (as `(FILL)(creds)`): fills the visible login form and says
/// which fields it filled, as a JSON string (a plain value for CDP and BiDi alike).
/// A password field takes the password and the text field before it the username;
/// without one (a username-first sign-in) only the username field is filled. A
/// one-time code goes into a field marked for it (or a row of one-digit boxes).
const FILL: &str = r#"(c) => {
  const inputs = [];
  const walk = (root) => root.querySelectorAll("*").forEach((el) => {
    if (el.tagName === "INPUT") inputs.push(el);
    if (el.shadowRoot) walk(el.shadowRoot);
  });
  walk(document);
  const type = (el) => (el.getAttribute("type") || "").toLowerCase();
  const ac = (el) => (el.getAttribute("autocomplete") || "").toLowerCase();
  const hint = (el) => [el.name, el.id, el.getAttribute("placeholder"), el.getAttribute("aria-label"), ac(el)].join(" ").toLowerCase();
  const shown = (el) => {
    if (el.disabled || el.readOnly) return false;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const s = getComputedStyle(el);
    return s.visibility !== "hidden" && s.display !== "none" && Number(s.opacity) > 0;
  };
  const textTypes = new Set(["", "text", "email", "tel", "number", "url"]);
  const visible = inputs.filter(shown);
  const pw = visible.filter((el) => type(el) === "password");
  const texty = visible.filter((el) => textTypes.has(type(el)));
  const before = (a, b) => !!(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
  const put = (el, v) => {
    el.focus();
    setter.call(el, v);
    el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, inputType: "insertReplacementText", data: v }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const userLike = (el) => /username|email/.test(ac(el));
  const filled = [];
  let otp = [];
  if (c.totp) {
    const boxes = texty.filter((el) => el.maxLength === 1);
    const otpHint = /one.?time|otp|2fa|mfa|two.?factor|verification|security.?code|auth.?code|passcode|token/;
    const one = texty.find((el) => ac(el) === "one-time-code") || (!pw.length && texty.find((el) => otpHint.test(hint(el))));
    if (one) otp = [one];
    else if (boxes.length === c.totp.length) otp = boxes;
  }
  if (c.password && pw.length) {
    const p = pw.find((el) => ac(el) !== "new-password") || pw[0];
    if (c.username) {
      const prior = texty.filter((el) => (!p.form || el.form === p.form) && before(el, p) && !otp.includes(el));
      const u = prior.findLast(userLike) || prior.at(-1);
      if (u) { put(u, c.username); filled.push("username"); }
    }
    put(p, c.password);
    filled.push("password");
  } else if (c.username && !otp.length) {
    const u = texty.find(userLike) || texty.find((el) => type(el) === "email")
      || texty.find((el) => /user|login|mail|account|identifier|benutzer|kennung|anmelde/.test(hint(el)));
    if (u) { put(u, c.username); filled.push("username"); }
  }
  if (otp.length === 1) put(otp[0], c.totp);
  else otp.forEach((el, i) => put(el, c.totp[i]));
  if (otp.length) filled.push("totp");
  return JSON.stringify({ filled });
}"#;

fn expression(creds: &Creds) -> String {
    let c = json!({
        "username": creds.username,
        "password": creds.password,
        "totp": creds.totp,
    });
    format!("({FILL})({c})")
}

/// The fields the script filled, from its JSON answer.
fn filled(answer: Option<&str>) -> Vec<String> {
    answer
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| {
            v["filled"].as_array().map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Frames of a page (top first) as (id, url), from a CDP frame tree or a BiDi
/// context tree.
pub(crate) fn frames(node: &Value, id: &str, children: &str, out: &mut Vec<(String, String)>) {
    let frame = if id == "context" {
        node
    } else {
        &node["frame"]
    };
    if let (Some(i), Some(u)) = (frame[id].as_str(), frame["url"].as_str()) {
        out.push((i.to_string(), u.to_string()));
    }
    for c in node[children].as_array().into_iter().flatten() {
        frames(c, id, children, out);
    }
}

pub(crate) struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
}

impl Cdp {
    pub(crate) async fn connect(ws_url: &str) -> Result<Self, String> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("Could not reach the page: {e}"))?;
        Ok(Self { ws, id: 0 })
    }

    pub(crate) async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    pub(crate) async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        let msg = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| format!("Could not talk to the page: {e}"))?;
        loop {
            let msg = self
                .ws
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
                return Err(e["message"].as_str().unwrap_or("error").to_string());
            }
            return Ok(v["result"].clone());
        }
    }
}

/// Fills the login form of the Chromium page at `ws_url`, in the first frame on
/// an allowed site (`frame_ok`, by URL) that has one.
pub async fn fill_chromium(
    ws_url: &str,
    frame_ok: impl Fn(&str) -> bool,
    creds: &Creds,
) -> Result<Vec<String>, String> {
    let work = async {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("Could not reach the page: {e}"))?;
        let mut cdp = Cdp { ws, id: 0 };
        let tree = cdp.call("Page.getFrameTree", json!({})).await?;
        let mut all = Vec::new();
        frames(&tree["frameTree"], "id", "childFrames", &mut all);
        let expr = expression(creds);
        let mut done = Vec::new();
        for (frame, url) in all.into_iter().filter(|(_, u)| frame_ok(u)) {
            // A frame of another process (a cross-site iframe) has no world here.
            let Ok(world) = cdp
                .call(
                    "Page.createIsolatedWorld",
                    json!({ "frameId": frame, "worldName": "bd-autofill" }),
                )
                .await
            else {
                continue;
            };
            let Some(ctx) = world["executionContextId"].as_u64() else {
                continue;
            };
            let res = cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": expr, "contextId": ctx, "returnByValue": true }),
                )
                .await;
            let got = res
                .ok()
                .map(|r| filled(r.pointer("/result/value").and_then(Value::as_str)))
                .unwrap_or_default();
            if !got.is_empty() {
                tracing::info!(frame_url = %url.split('?').next().unwrap_or_default(), fields = ?got, "Login filled from the vault");
                done = got;
                break;
            }
        }
        let _ = cdp.ws.close(None).await;
        Ok(done)
    };
    tokio::time::timeout(Duration::from_secs(15), work)
        .await
        .map_err(|_| "The page did not answer in time".to_string())?
}

/// The same for a Firefox tab (BiDi context), in a sandbox of each frame.
pub async fn fill_firefox(
    hub: &BidiHub,
    context: &str,
    frame_ok: impl Fn(&str) -> bool,
    creds: &Creds,
) -> Result<Vec<String>, String> {
    let tree = hub
        .call("browsingContext.getTree", json!({ "root": context }))
        .await?;
    let mut all = Vec::new();
    for c in tree["contexts"].as_array().into_iter().flatten() {
        frames(c, "context", "children", &mut all);
    }
    let expr = expression(creds);
    for (ctx, url) in all.into_iter().filter(|(_, u)| frame_ok(u)) {
        let Ok(res) = hub
            .call(
                "script.evaluate",
                json!({
                    "expression": expr,
                    "target": { "context": ctx, "sandbox": "bd-autofill" },
                    "awaitPromise": false,
                    "resultOwnership": "none",
                }),
            )
            .await
        else {
            continue;
        };
        let got = filled(res.pointer("/result/value").and_then(Value::as_str));
        if !got.is_empty() {
            tracing::info!(frame_url = %url.split('?').next().unwrap_or_default(), fields = ?got, "Login filled from the vault");
            return Ok(got);
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_trees_flatten_top_first() {
        let cdp = json!({ "frame": { "id": "A", "url": "https://a.test/" }, "childFrames": [
            { "frame": { "id": "B", "url": "https://b.test/" }, "childFrames": [
                { "frame": { "id": "C", "url": "about:blank" } } ] } ] });
        let mut out = Vec::new();
        frames(&cdp, "id", "childFrames", &mut out);
        let ids: Vec<&str> = out.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["A", "B", "C"]);

        let bidi = json!({ "context": "X", "url": "https://x.test/", "children": [
            { "context": "Y", "url": "https://y.test/", "children": [] } ] });
        let mut out = Vec::new();
        frames(&bidi, "context", "children", &mut out);
        assert_eq!(out[1], ("Y".to_string(), "https://y.test/".to_string()));
    }

    #[test]
    fn values_are_passed_as_json_not_code() {
        let e = expression(&Creds {
            username: Some("me\"); alert(1); (\"".into()),
            password: Some("p\\w".into()),
            totp: None,
        });
        assert!(
            e.ends_with(r#"({"password":"p\\w","totp":null,"username":"me\"); alert(1); (\""})"#)
        );
        assert_eq!(
            filled(Some(r#"{"filled":["username","password"]}"#)),
            ["username", "password"]
        );
        assert!(filled(Some("nope")).is_empty());
    }
}
