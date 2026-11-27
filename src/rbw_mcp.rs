//! Local, read-only RBW MCP client. Secrets only pass through private files.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::secrets::{self, Secret};

async fn call(
    input: &mut tokio::process::ChildStdin,
    output: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let mut request =
        json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}).to_string();
    request.push('\n');
    input
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "RBW MCP input failed")?;
    let mut line = String::new();
    loop {
        line.clear();
        let n = tokio::time::timeout(Duration::from_secs(130), output.read_line(&mut line))
            .await
            .map_err(|_| "RBW MCP timed out")?
            .map_err(|_| "RBW MCP output failed")?;
        if n == 0 {
            return Err("RBW MCP stopped".into());
        }
        let answer: Value = serde_json::from_str(&line).map_err(|_| "Invalid RBW MCP response")?;
        if answer["id"].as_u64() != Some(id) {
            continue;
        }
        if answer.get("error").is_some() || answer["result"]["isError"] == true {
            return Err("RBW MCP refused the vault request (check vault unlock and entry)".into());
        }
        return Ok(answer["result"].clone());
    }
}

fn content(result: &Value) -> Result<&str, String> {
    result["content"]
        .as_array()
        .and_then(|v| v.iter().find_map(|c| c["text"].as_str()))
        .ok_or_else(|| "RBW MCP returned no content".into())
}

pub async fn lookup(
    rbw: &Path,
    entry: &str,
    user: Option<&str>,
    field: &str,
    card: bool,
) -> Result<Secret, String> {
    if card
        && !matches!(
            field,
            "number" | "exp_month" | "exp_year" | "exp_date" | "cvv"
        )
    {
        return Err("Unsupported card field".into());
    }
    let mut child = Command::new(rbw)
        .args(["mcp", "--read-only", "--cli"])
        .arg(rbw)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Could not start RBW MCP")?;
    let mut input = child.stdin.take().ok_or("RBW MCP has no input")?;
    let mut output = BufReader::new(child.stdout.take().ok_or("RBW MCP has no output")?);
    call(&mut input, &mut output, 1, "initialize", json!({"protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"browser-daemon", "version":"1"}})).await?;
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .map_err(|_| "RBW MCP input failed")?;
    let mut selector = json!({"entry":entry, "exact":true});
    if let Some(user) = user {
        selector["user"] = json!(user);
    }
    let meta = call(
        &mut input,
        &mut output,
        2,
        "tools/call",
        json!({"name":"get", "arguments":selector}),
    )
    .await?;
    let meta: Value =
        serde_json::from_str(content(&meta)?).map_err(|_| "Invalid RBW entry metadata")?;
    let expected = if card { "Card" } else { "Login" };
    if meta["data"]["type"].as_str() != Some(expected) {
        return Err("Wrong vault entry type".into());
    }
    if !card && !matches!(field, "username" | "password" | "totp") {
        // `rbw get --field` resolves built-ins before custom fields. Never let
        // a requested custom field fall through to notes or another built-in.
        let reserved = [
            "note",
            "notes",
            "user",
            "username",
            "password",
            "totp",
            "totp_code",
            "code",
            "uris",
            "urls",
            "sites",
            "identityname",
            "city",
            "state",
            "postcode",
            "zipcode",
            "zip",
            "country",
            "phone",
            "ssn",
            "license",
            "passport",
            "number",
            "card",
            "exp",
            "exp_month",
            "month",
            "exp_year",
            "year",
            "cvv",
            "cardholder",
            "cardholder_name",
            "brand",
            "type",
            "name",
            "email",
            "address",
            "address1",
            "address2",
            "address3",
            "fingerprint",
            "public_key",
            "private_key",
            "title",
            "first_name",
            "middle_name",
            "last_name",
        ];
        let custom = meta["fields"].as_array().is_some_and(|fields| {
            fields
                .iter()
                .any(|item| item["name"].as_str() == Some(field))
        });
        if !custom || reserved.contains(&field.to_ascii_lowercase().as_str()) {
            return Err("Not a permitted custom field".into());
        }
    }
    let id = meta["id"].as_str().ok_or("RBW entry has no ID")?;
    let uris = if card {
        Vec::new()
    } else {
        secrets::uris(&meta)
    };
    let username = if card {
        None
    } else {
        meta["data"]["username"].as_str().map(str::to_string)
    };
    let directory = if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        tempfile::tempdir_in(runtime)
    } else {
        tempfile::tempdir()
    }
    .map_err(|_| "Could not create private RBW scratch directory")?;
    let path = directory.path().join("field");
    let path = path.to_str().ok_or("Invalid RBW scratch path")?;
    let fields: Vec<&str> = if card && field == "exp_date" {
        vec!["exp_month", "exp_year"]
    } else if !card && field == "totp" {
        vec!["totp_code"]
    } else {
        vec![field]
    };
    let mut values = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let result = call(
            &mut input,
            &mut output,
            3 + index as u64,
            "tools/call",
            json!({"name":"secret_to_file", "arguments":{"entry":id, "field":field, "path":path}}),
        )
        .await?;
        content(&result)?;
        let value = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| "Could not read RBW field")?;
        tokio::fs::remove_file(path)
            .await
            .map_err(|_| "Could not remove RBW field")?;
        if value.is_empty() {
            return Err("RBW field is empty".into());
        }
        values.push(value);
    }
    let value = if card && field == "exp_date" {
        secrets::pick_card_field(
            &json!({"data":{"type":"Card", "exp_month":values[0], "exp_year":values[1]}}),
            field,
        )?
    } else {
        values.remove(0)
    };
    Ok(Secret {
        value,
        username,
        uris,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn rejects_unsupported_card_fields_before_starting_rbw() {
        let error = lookup(Path::new("/missing"), "card", None, "private_key", true)
            .await
            .err();
        assert_eq!(error.as_deref(), Some("Unsupported card field"));
    }

    #[tokio::test]
    async fn reads_redacted_metadata_and_private_field_from_rbw_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("rbw");
        std::fs::write(&executable, r##"#!/bin/sh
while IFS= read -r line
do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}' ;;
    *'"name":"get"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"id\":\"id-1\",\"data\":{\"type\":\"Login\",\"username\":\"alice\",\"uris\":[{\"uri\":\"https://example.test/login\",\"match_type\":3}]}}"}]}}' ;;
    *'"name":"secret_to_file"'*)
      path=$(printf '%s' "$line" | sed -n 's/.*"path":"\([^"]*\)".*/\1/p')
      printf '%s' 'test-value' > "$path"
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"written"}]}}' ;;
  esac
done
"##).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = lookup(&executable, "example", None, "password", false)
            .await
            .unwrap();
        assert_eq!(secret.value, "test-value");
        assert_eq!(secret.username.as_deref(), Some("alice"));
        assert!(secrets::url_matches(
            "https://example.test/login",
            &secret.uris
        ));
        assert!(!secrets::url_matches(
            "https://example.test/other",
            &secret.uris
        ));
    }
}
