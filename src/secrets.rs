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
    pub uris: Vec<Uri>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Uri {
    pub uri: String,
    /// Bitwarden's match type: None/0 base domain, 1 host, 2 starts with,
    /// 3 exact, 4 regex, 5 never.
    pub match_type: Option<u64>,
}

pub async fn rbw(rbw: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(rbw)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
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
    let unlocked = Command::new(rbw_bin)
        .arg("unlocked")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
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

pub fn host_of(uri: &str) -> Option<String> {
    let parsed = url::Url::parse(uri)
        .ok()
        .filter(|u| u.host_str().is_some())
        .or_else(|| url::Url::parse(&format!("https://{uri}")).ok())?;
    Some(
        parsed
            .host_str()?
            .trim_end_matches('.')
            .to_ascii_lowercase(),
    )
}

/// The registrable part of a host, roughly: the last two labels, three for
/// country suffixes like co.uk. IP addresses stay whole.
pub fn base_domain(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let keep = if n >= 3
        && labels[n - 1].len() == 2
        && matches!(
            labels[n - 2],
            "co" | "com" | "org" | "net" | "ac" | "gov" | "edu" | "ne" | "or"
        ) {
        3
    } else {
        2
    };
    if n <= keep {
        return host;
    }
    let skip: usize = labels[..n - keep].iter().map(|l| l.len() + 1).sum();
    &host[skip..]
}

/// Whether the vault entry is meant for `host`, as Bitwarden's autofill would
/// decide (regex URIs are not supported and never match).
pub fn site_matches(host: &str, uris: &[Uri]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    uris.iter().any(|u| {
        let Some(h) = host_of(&u.uri) else {
            return false;
        };
        match u.match_type {
            Some(4) | Some(5) => false,
            Some(1..=3) => h == host,
            _ => base_domain(&h) == base_domain(&host),
        }
    })
}

/// Replaces every filled secret in the strings of `v` with MASK.
pub fn redact(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => {
            for secret in secrets.iter().filter(|s| s.chars().count() >= MIN_REDACT) {
                if s.contains(secret.as_str()) {
                    *s = s.replace(secret.as_str(), MASK);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact(x, secrets)),
        Value::Object(o) => o.values_mut().for_each(|x| redact(x, secrets)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uri(u: &str, m: Option<u64>) -> Uri {
        Uri {
            uri: u.into(),
            match_type: m,
        }
    }

    #[test]
    fn subdomains_of_the_entry_site_match() {
        let uris = [uri("https://frigate.video/plus/", None)];
        assert!(site_matches("auth.frigate.video", &uris));
        assert!(site_matches("frigate.video", &uris));
        assert!(!site_matches("frigate.video.evil.test", &uris));
        assert!(!site_matches("evil-frigate.video", &uris));
    }

    #[test]
    fn host_and_exact_matches_need_the_same_host() {
        let uris = [uri("https://ssl6.hrworks.de/o/dashboard", Some(1))];
        assert!(site_matches("ssl6.hrworks.de", &uris));
        assert!(!site_matches("login.hrworks.de", &uris));
        assert!(!site_matches("x.test", &[uri("https://x.test", Some(5))]));
        assert!(!site_matches(
            "x.test",
            &[uri("^https://x\\.test", Some(4))]
        ));
    }

    #[test]
    fn uris_without_scheme_and_country_suffixes() {
        assert!(site_matches(
            "accounts.google.com",
            &[uri("google.com", None)]
        ));
        assert!(site_matches(
            "login.bbc.co.uk",
            &[uri("https://www.bbc.co.uk", None)]
        ));
        assert!(!site_matches(
            "evil.co.uk",
            &[uri("https://www.bbc.co.uk", None)]
        ));
        assert!(site_matches(
            "10.5.1.1",
            &[uri("http://10.5.1.1:8123", None)]
        ));
        assert!(!site_matches(
            "10.5.1.2",
            &[uri("http://10.5.1.1:8123", None)]
        ));
        assert!(!site_matches("anything.test", &[]));
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
        let mut short = json!("abc");
        redact(&mut short, &["abc".into()]);
        assert_eq!(short, "abc");
    }
}
