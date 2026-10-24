//! Vault secrets for `browser_fill_secret` (see mcp.rs): read with rbw, matched
//! against the page they go into, and kept out of everything the agent sees.

use std::path::Path;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

/// What redaction puts where a filled secret would show.
pub const MASK: &str = "•••";

/// Secrets shorter than this are not redacted (they would mangle unrelated text).
const MIN_REDACT: usize = 4;

/// A field of a vault entry and the sites the entry is for.
pub struct Secret {
    pub value: String,
    /// The entry's username, when it has one: `user:password` (HTTP Basic
    /// auth) is masked as well.
    pub username: Option<String>,
    pub uris: Vec<Uri>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Uri {
    pub uri: String,
    /// Bitwarden's match type: None/0 base domain, 1 host, 2 starts with,
    /// 3 exact, 4 regex, 5 never.
    pub match_type: Option<u64>,
}

/// rbw is asked for little; one that hangs must not hold a request forever.
const RBW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn rbw(rbw: &Path, args: &[&str]) -> Result<String, String> {
    let run = Command::new(rbw)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(RBW_TIMEOUT, run)
        .await
        .map_err(|_| "rbw did not answer in time".to_string())?
        .map_err(|e| format!("Could not run rbw: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "rbw: {}",
            err.lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("failed")
                .trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "rbw: not UTF-8".to_string())
}

/// `field` of the vault entry `entry` (`user` picks one of several entries of
/// that name): password, username, totp (the current code) or the name of a
/// custom field. Never falls back to another field; an empty one is an error.
pub async fn lookup(
    rbw_bin: &Path,
    entry: &str,
    user: Option<&str>,
    field: &str,
) -> Result<Secret, String> {
    let unlocked = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new(rbw_bin)
            .arg("unlocked")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|s| s.is_ok_and(|s| s.success()));
    if !unlocked {
        return Err("rbw is locked: unlock it first (rbw skill), then try again".into());
    }
    let mut args = vec!["get", "--raw", entry];
    args.extend(user);
    let raw: Value = serde_json::from_str(&rbw(rbw_bin, &args).await?)
        .map_err(|_| "rbw: unexpected output".to_string())?;
    let value = match field {
        "totp" => {
            let mut args = vec!["code", entry];
            args.extend(user);
            rbw(rbw_bin, &args).await?.trim().to_string()
        }
        _ => pick_field(&raw, field).unwrap_or_default(),
    };
    if value.is_empty() {
        return Err(format!("The entry '{entry}' has no {field}"));
    }
    Ok(Secret {
        value,
        username: pick_field(&raw, "username").filter(|u| !u.is_empty()),
        uris: uris(&raw),
    })
}

/// A field of `rbw get --raw` output: password, username or a custom field.
pub fn pick_field(raw: &Value, field: &str) -> Option<String> {
    match field {
        "password" | "username" => raw["data"][field].as_str().map(str::to_string),
        _ => raw["fields"].as_array()?.iter().find_map(|f| {
            (f["name"].as_str()?.eq_ignore_ascii_case(field))
                .then(|| f["value"].as_str().map(str::to_string))
                .flatten()
        }),
    }
}

pub fn uris(raw: &Value) -> Vec<Uri> {
    raw["data"]["uris"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| {
                    Some(Uri {
                        uri: u["uri"].as_str()?.to_string(),
                        match_type: u["match_type"].as_u64(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The registrable part of a host, by the Public Suffix List (private suffixes
/// such as `github.io` or `pages.dev` included: `a.github.io` and `b.github.io`
/// are different sites). A host that is a suffix itself, an IP address or an
/// unknown single label stays whole.
pub fn base_domain(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host;
    }
    psl::domain_str(host).unwrap_or(host)
}

/// Whether a passkey for `rp_id` may be used on a page at `host`, by the
/// WebAuthn rule: the rp_id is the host or a parent domain of it, and not a
/// public suffix (so never `github.io` or `co.uk`).
pub fn rp_id_valid_for(host: &str, rp_id: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if rp_id.is_empty() || host.is_empty() {
        return false;
    }
    if host == rp_id {
        return true;
    }
    host.ends_with(&format!(".{rp_id}"))
        && rp_id.parse::<std::net::IpAddr>().is_err()
        && psl::domain_str(rp_id).is_some()
}

/// A vault entry's URI as a URL: an address without a scheme is https.
fn parse_uri(uri: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(uri)
        .ok()
        .filter(|u| u.host_str().is_some())
        .or_else(|| url::Url::parse(&format!("https://{uri}")).ok())?;
    matches!(parsed.scheme(), "http" | "https").then_some(parsed)
}

/// Whether an entry saved for `entry` may be used on `page`: an https entry is
/// never used on plain http, and a port the entry names (or the default one)
/// is the one the page must be on.
fn same_scheme_and_port(page: &url::Url, entry: &url::Url) -> bool {
    if page.scheme() != entry.scheme() && !(page.scheme() == "https" && entry.scheme() == "http") {
        return false;
    }
    match entry.port() {
        Some(p) => page.port_or_known_default() == Some(p),
        None => page.port().is_none(),
    }
}

/// Whether the vault entry may be filled into the page at `page_url`: the
/// match type is honoured (starts with, exact), and scheme and port count.
pub fn url_matches(page_url: &str, uris: &[Uri]) -> bool {
    let Ok(page) = url::Url::parse(page_url) else {
        return false;
    };
    let Some(host) = page
        .host_str()
        .filter(|_| matches!(page.scheme(), "http" | "https"))
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
    else {
        return false;
    };
    uris.iter().any(|u| {
        let Some(entry) = parse_uri(&u.uri) else {
            return false;
        };
        let Some(entry_host) = entry
            .host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        else {
            return false;
        };
        let trim = |p: &str| p.trim_end_matches('/').to_string();
        match u.match_type {
            Some(4) | Some(5) => false,
            Some(1) => entry_host == host && same_scheme_and_port(&page, &entry),
            Some(2) => {
                entry_host == host
                    && same_scheme_and_port(&page, &entry)
                    && trim(page.path()).starts_with(&trim(entry.path()))
            }
            Some(3) => {
                entry_host == host
                    && same_scheme_and_port(&page, &entry)
                    && trim(page.path()) == trim(entry.path())
            }
            _ => {
                base_domain(&entry_host) == base_domain(&host)
                    && same_scheme_and_port(&page, &entry)
            }
        }
    })
}

/// The forms a secret takes when a page or a tool echoes it: as typed, escaped
/// for JSON, percent-encoded (as a form body or a URL) and escaped for HTML.
fn variants(secret: &str) -> Vec<String> {
    let json = serde_json::to_string(secret).unwrap_or_default();
    let json = json
        .get(1..json.len().saturating_sub(1))
        .unwrap_or_default();
    let form = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
    let html = secret
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    let mut out: Vec<String> = Vec::new();
    for v in [
        secret.to_string(),
        json.to_string(),
        form.replace('+', "%20"),
        form,
        html.clone(),
        html.replace("&#39;", "&#x27;"),
    ] {
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    for v in base64_forms(secret) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    // Longest first: an escaped form contains the shorter ones.
    out.sort_by_key(|v| std::cmp::Reverse(v.len()));
    out
}

/// Shortest base64 fragment worth masking (a shorter one would hit unrelated text).
const MIN_B64: usize = 6;

/// The base64 text of `secret` as it can appear inside a longer encoded
/// string (a header, a data URL): standard and url-safe alphabet, each of the
/// three byte alignments, cut to the characters the secret alone determines.
fn base64_forms(secret: &str) -> Vec<String> {
    use base64::Engine;
    let engines: [&base64::engine::GeneralPurpose; 2] = [
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    let mut out: Vec<String> = Vec::new();
    for engine in engines {
        for pad in 0..3usize {
            let mut bytes = vec![b'x'; pad];
            bytes.extend_from_slice(secret.as_bytes());
            let enc = engine.encode(&bytes);
            // Characters that carry bits of the padding bytes are not ours.
            let skip = [0, 2, 3][pad];
            // The last character holds zero fill bits unless the length is a
            // multiple of three: what follows would change it.
            let drop = usize::from(bytes.len() % 3 != 0);
            let end = enc.len().saturating_sub(drop);
            let Some(part) = enc.get(skip..end) else {
                continue;
            };
            if part.len() >= MIN_B64 && !out.iter().any(|o| o == part) {
                out.push(part.to_string());
            }
        }
    }
    out
}

/// Replaces every filled secret in the strings of `v` with MASK, in every
/// form `variants` lists. A secret shorter than MIN_REDACT would mangle
/// unrelated text, so it is only masked where it is the whole string.
pub fn redact(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                if secret.chars().count() < MIN_REDACT {
                    if s == secret {
                        *s = MASK.to_string();
                    }
                    continue;
                }
                for variant in variants(secret) {
                    if s.contains(variant.as_str()) {
                        *s = s.replace(variant.as_str(), MASK);
                    }
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact(x, secrets)),
        Value::Object(o) => o.values_mut().for_each(|x| redact(x, secrets)),
        _ => {}
    }
}

/// `redact` for a plain string (an error message).
pub fn redact_str(text: &str, secrets: &[String]) -> String {
    let mut v = Value::String(text.to_string());
    redact(&mut v, secrets);
    match v {
        Value::String(s) => s,
        _ => String::new(),
    }
}

/// Masks the secrets in the text files (snapshots, console logs, ...) under
/// `dir` written since `since`. Playwright MCP keeps page snapshots in files
/// and its answer only links them, so the mask on the answer does not reach
/// them. A snapshot shows an input's value whatever it looks like on screen.
/// Returns how many files were changed.
pub fn redact_files(dir: &Path, since: std::time::SystemTime, secrets: &[String]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            changed += redact_files(&path, since, secrets);
            continue;
        }
        let text_file = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "yml" | "yaml" | "log" | "md" | "txt" | "json" | "html"));
        if !text_file || meta.modified().is_ok_and(|m| m < since) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let masked = redact_str(&text, secrets);
        if masked != text && std::fs::write(&path, masked).is_ok() {
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_forms_are_masked_at_every_alignment() {
        use base64::Engine;
        let secret = "s3cr/t+pässwörd?~";
        for engine in [
            &base64::engine::general_purpose::STANDARD,
            &base64::engine::general_purpose::URL_SAFE,
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ] {
            for prefix in ["", "a", "ab", "abc"] {
                for suffix in ["", "z", "zy"] {
                    let enc = engine.encode(format!("{prefix}{secret}{suffix}"));
                    let mut v = Value::String(format!("Authorization: Basic {enc}!"));
                    redact(&mut v, &[secret.to_string()]);
                    let text = v.as_str().unwrap();
                    assert!(text.contains(MASK), "{prefix:?} {suffix:?}: {text}");
                    let full = engine.encode(secret);
                    assert!(!text.contains(&full[..full.len().min(8)]), "{text}");
                }
            }
        }
    }

    #[test]
    fn basic_auth_pairs_are_masked() {
        use base64::Engine;
        let enc = base64::engine::general_purpose::STANDARD.encode("alice:hunter22!");
        let mut v = Value::String(format!("Basic {enc}"));
        redact(&mut v, &["alice:hunter22!".to_string()]);
        assert_eq!(v.as_str().unwrap(), format!("Basic {MASK}"));
    }

    fn uri(u: &str, m: Option<u64>) -> Uri {
        Uri {
            uri: u.into(),
            match_type: m,
        }
    }

    #[test]
    fn fields_never_fall_back_to_notes() {
        let raw = json!({
            "data": { "username": "me@x.test", "password": null, "uris": [] },
            "fields": [{ "name": "Long-Lived Access Token", "value": "tok" }],
            "notes": "api key: secret"
        });
        assert_eq!(pick_field(&raw, "password"), None);
        assert_eq!(pick_field(&raw, "username").as_deref(), Some("me@x.test"));
        assert_eq!(
            pick_field(&raw, "long-lived access token").as_deref(),
            Some("tok")
        );
        assert_eq!(pick_field(&raw, "notes"), None);
    }

    #[test]
    fn filled_secrets_are_masked_everywhere() {
        let mut v = json!({ "content": [{ "type": "text", "text": "value: hunter22, again hunter22" }], "n": 1 });
        redact(&mut v, &["hunter22".into(), "abc".into()]);
        assert_eq!(
            v["content"][0]["text"],
            format!("value: {MASK}, again {MASK}")
        );
        let mut short = json!("abc d");
        redact(&mut short, &["abc".into()]);
        assert_eq!(short, "abc d");
    }

    #[test]
    fn shared_hosting_suffixes_are_separate_sites() {
        for (page, entry) in [
            ("evil.github.io", "https://a.github.io"),
            ("evil.pages.dev", "https://good.pages.dev"),
            ("x.netlify.app", "https://y.netlify.app"),
            ("evil.duckdns.org", "https://mine.duckdns.org"),
            ("evil.me.uk", "https://good.me.uk"),
            ("evil.co.uk", "https://www.bbc.co.uk"),
            ("evil.com.au", "https://good.com.au"),
        ] {
            assert!(
                !url_matches(&format!("https://{page}/"), &[uri(entry, None)]),
                "{page} vs {entry}"
            );
        }
        assert!(url_matches(
            "https://login.a.github.io/",
            &[uri("https://a.github.io", None)]
        ));
        assert_eq!(base_domain("a.b.github.io"), "b.github.io");
        assert_eq!(base_domain("github.io"), "github.io");
        assert_eq!(base_domain("localhost"), "localhost");
        assert_eq!(base_domain("10.0.0.1"), "10.0.0.1");
        assert_eq!(base_domain("auth.frigate.video"), "frigate.video");
    }

    #[test]
    fn fills_look_at_scheme_port_and_match_type() {
        let https = [uri("https://example.com", None)];
        assert!(url_matches("https://login.example.com/x?y=1", &https));
        assert!(!url_matches("http://example.com/", &https), "downgrade");
        assert!(
            !url_matches("https://example.com:8443/", &https),
            "other port"
        );
        assert!(!url_matches("ftp://example.com/", &https));
        assert!(!url_matches("about:blank", &https));
        assert!(!url_matches("not a url", &https));
        // An http entry also fills its https page, on the default port.
        let http = [uri("http://example.com", None)];
        assert!(url_matches("https://example.com/", &http));
        assert!(url_matches("http://example.com/", &http));
        // An entry naming a port is for that port.
        let ha = [uri("http://10.5.1.1:8123", None)];
        assert!(url_matches("http://10.5.1.1:8123/lovelace", &ha));
        assert!(!url_matches("http://10.5.1.1:8124/", &ha));
        assert!(!url_matches("http://10.5.1.1/", &ha));
        // Host, starts with, exact.
        let host = [uri("https://ssl6.hrworks.de/o/dashboard", Some(1))];
        assert!(url_matches("https://ssl6.hrworks.de/other", &host));
        assert!(!url_matches("https://login.hrworks.de/", &host));
        let prefix = [uri("https://x.test/app", Some(2))];
        assert!(url_matches("https://x.test/app/login", &prefix));
        assert!(!url_matches("https://x.test/other", &prefix));
        let exact = [uri("https://x.test/login", Some(3))];
        assert!(url_matches("https://x.test/login/", &exact));
        assert!(!url_matches("https://x.test/login/evil", &exact));
        assert!(!url_matches(
            "https://x.test/",
            &[uri("https://x.test", Some(5))]
        ));
        assert!(!url_matches(
            "https://x.test/",
            &[uri("^https://x\\.test", Some(4))]
        ));
        assert!(!url_matches("https://x.test/", &[]));
    }

    #[test]
    fn masking_covers_escaped_and_encoded_copies() {
        let secret = "pa\"ss\\wörd &<'x y".to_string();
        let json_form = serde_json::to_string(&secret).unwrap();
        let form = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
        let html = "pa&quot;ss\\wörd &amp;&lt;&#39;x y";
        let mut body = json!({
            "raw": format!("a {secret} b"),
            "json": format!("{{\"password\":{json_form}}}"),
            "form": format!("user=me&password={form}&x=1"),
            "pct": format!("?p={}", form.replace('+', "%20")),
            "html": format!("<input value=\"{html}\">"),
            "nested": [{ "deep": [format!("{secret}{secret}")] }],
        });
        redact(&mut body, std::slice::from_ref(&secret));
        let text = body.to_string();
        for needle in [
            &secret,
            &json_form,
            &form,
            &form.replace('+', "%20"),
            &html.to_string(),
        ] {
            assert!(!text.contains(needle.as_str()), "left {needle}");
        }
        assert_eq!(body["nested"][0]["deep"][0], format!("{MASK}{MASK}"));
        assert!(text.contains("user=me&password=•••&x=1"));
    }

    #[test]
    fn short_secrets_are_masked_only_when_they_are_the_whole_value() {
        let mut v = json!({ "a": "123", "b": "room 123 is free", "c": ["123"] });
        redact(&mut v, &["123".into()]);
        assert_eq!(v["a"], MASK);
        assert_eq!(v["b"], "room 123 is free");
        assert_eq!(v["c"][0], MASK);
        assert_eq!(
            redact_str("error: hunter22 refused", &["hunter22".into()]),
            format!("error: {MASK} refused")
        );
        let mut empty = json!("keep");
        redact(&mut empty, &[String::new()]);
        assert_eq!(empty, "keep");
    }

    #[test]
    fn a_passkey_is_for_the_page_site_not_a_public_suffix() {
        assert!(rp_id_valid_for("login.example.com", "example.com"));
        assert!(rp_id_valid_for("example.com", "example.com"));
        assert!(rp_id_valid_for("login.example.com", "login.example.com"));
        assert!(!rp_id_valid_for("example.com", "login.example.com"));
        assert!(!rp_id_valid_for("evil.test", "example.com"));
        assert!(!rp_id_valid_for("notexample.com", "example.com"));
        assert!(!rp_id_valid_for("evil.github.io", "github.io"));
        assert!(!rp_id_valid_for("evil.co.uk", "co.uk"));
        assert!(!rp_id_valid_for("a.com", ""));
        assert!(rp_id_valid_for("a.evil.github.io", "evil.github.io"));
    }

    #[test]
    fn files_written_since_are_masked_older_ones_and_other_types_are_not() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("session");
        std::fs::create_dir(&sub).unwrap();
        let line = "- textbox \"Pass\": DummySecret123\n";
        let old = sub.join("old.yml");
        std::fs::write(&old, line).unwrap();
        let since = std::time::SystemTime::now() + Duration::from_secs(60);
        let secrets = vec!["DummySecret123".to_string()];
        assert_eq!(redact_files(dir.path(), since, &secrets), 0, "too old");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), line);

        let since = std::time::SystemTime::now() - Duration::from_secs(60);
        let png = sub.join("shot.png");
        std::fs::write(&png, line).unwrap();
        assert_eq!(redact_files(dir.path(), since, &secrets), 1);
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "- textbox \"Pass\": •••\n"
        );
        assert_eq!(
            std::fs::read_to_string(&png).unwrap(),
            line,
            "not a text file"
        );
        assert_eq!(redact_files(dir.path(), since, &secrets), 0, "nothing left");
    }
}
