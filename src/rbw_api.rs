//! Restricted RBW agent API client for MCP fills. Values never enter tool output.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use crate::secrets::{self, Secret, Uri};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

pub struct Client {
    base: url::Url,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Items {
    items: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    name: Option<String>,
    fields: Vec<String>,
}
#[derive(Deserialize)]
struct Field {
    value: Option<String>,
}

fn decode_uris(value: &str) -> Result<Vec<Uri>, String> {
    serde_json::from_str(value).map_err(|_| "Invalid RBW API URI list".into())
}

impl Client {
    pub fn new(origin: &str, ca: &Path, cert: &Path, key: &Path) -> Result<Self, String> {
        let base = url::Url::parse(origin).map_err(|_| "Invalid RBW API origin")?;
        if base.scheme() != "https"
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(
                "RBW API must be an HTTPS origin without credentials, path, query or fragment"
                    .into(),
            );
        }
        let metadata = std::fs::metadata(key).map_err(|_| "Cannot read RBW API private key")?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("RBW API private key must be a private file (mode 0600)".into());
        }
        let ca = std::fs::read(ca).map_err(|_| "Cannot read RBW API CA certificate")?;
        let ca =
            reqwest::Certificate::from_pem(&ca).map_err(|_| "Invalid RBW API CA certificate")?;
        let mut identity = Zeroizing::new(
            std::fs::read(cert).map_err(|_| "Cannot read RBW API client certificate")?,
        );
        identity.push(b'\n');
        let key =
            Zeroizing::new(std::fs::read(key).map_err(|_| "Cannot read RBW API private key")?);
        identity.extend_from_slice(&key);
        let result = reqwest::Identity::from_pem(&identity);
        let identity = result.map_err(|_| "Invalid RBW API client certificate or key")?;
        let http = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            .identity(identity)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| "Cannot configure RBW API client")?;
        Ok(Self { base, http })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, segments: &[&str]) -> Result<T, String> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| "Invalid RBW API origin")?
            .clear()
            .extend(segments);
        let response = self
            .http
            .get(url)
            .header("X-Rbw-Consumer", "browser-daemon-mcp")
            .send()
            .await
            .map_err(|_| "RBW API request failed")?;
        if !response.status().is_success() {
            return Err(format!(
                "RBW API refused the request (HTTP {})",
                response.status().as_u16()
            ));
        }
        response
            .json()
            .await
            .map_err(|_| "Invalid RBW API response".into())
    }

    async fn field(&self, id: &str, field: &[&str]) -> Result<String, String> {
        self.optional_field(id, field)
            .await?
            .ok_or_else(|| "RBW API field is empty".into())
    }

    async fn optional_field(&self, id: &str, field: &[&str]) -> Result<Option<String>, String> {
        let mut path = vec!["v1", "items", id, "fields"];
        path.extend_from_slice(field);
        let value: Field = self.get(&path).await?;
        Ok(value.value.filter(|s| !s.is_empty()))
    }

    async fn resolve(&self, entry: &str, user: Option<&str>) -> Result<Option<Item>, String> {
        let items: Items = self.get(&["v1", "items"]).await?;
        let mut matched = None;
        for item in items
            .items
            .into_iter()
            .filter(|item| item.id == entry || item.name.as_deref() == Some(entry))
        {
            if let Some(user) = user {
                if !item.fields.iter().any(|f| f == "username")
                    || self
                        .optional_field(&item.id, &["username"])
                        .await?
                        .as_deref()
                        != Some(user)
                {
                    continue;
                }
            }
            if matched.is_some() {
                return Err("RBW API item is ambiguous; use the item ID".into());
            }
            matched = Some(item);
        }
        Ok(matched)
    }

    async fn read_item(&self, item: &Item, field: &str, card: bool) -> Result<Secret, String> {
        let mut uris = Vec::new();
        if !card {
            let all_uris = item.fields.iter().any(|field| field == "uri_all");
            if all_uris {
                let value = self.field(&item.id, &["uri_all"]).await?;
                uris = decode_uris(&value)?;
            }
            for token in &item.fields {
                if all_uris {
                    break;
                }
                if let Some(index) = token.strip_prefix("uri/") {
                    let index: usize = index.parse().map_err(|_| "Invalid RBW API URI grant")?;
                    let index = index.to_string();
                    let uri = self.field(&item.id, &["uri", &index]).await?;
                    let policy = self.field(&item.id, &["uri_match", &index]).await?;
                    let match_type: Option<u64> = serde_json::from_str(&policy)
                        .map_err(|_| "Invalid RBW API URI match type")?;
                    uris.push(Uri { uri, match_type });
                }
            }
        }
        let value = if card {
            if field == "exp_date" {
                let month = self.field(&item.id, &["card_exp_month"]).await?;
                let year = self.field(&item.id, &["card_exp_year"]).await?;
                secrets::pick_card_field(
                    &json!({"data": {"type": "Card", "exp_month": month, "exp_year": year}}),
                    field,
                )?
            } else {
                let token = match field {
                    "number" => "card_number",
                    "exp_month" => "card_exp_month",
                    "exp_year" => "card_exp_year",
                    "cvv" => "card_code",
                    _ => return Err("Unsupported card field".into()),
                };
                self.field(&item.id, &[token]).await?
            }
        } else {
            match field {
                "username" | "password" | "totp" => self.field(&item.id, &[field]).await?,
                _ => self.field(&item.id, &["custom", field]).await?,
            }
        };
        let username =
            if !card && field != "username" && item.fields.iter().any(|f| f == "username") {
                self.optional_field(&item.id, &["username"]).await?
            } else {
                None
            };
        Ok(Secret {
            value,
            username,
            uris,
        })
    }
}

pub async fn lookup(
    clients: &[Client],
    entry: &str,
    user: Option<&str>,
    field: &str,
    card: bool,
) -> Result<Secret, String> {
    let mut matched = None;
    for client in clients {
        if let Some(item) = client.resolve(entry, user).await? {
            if matched.is_some() {
                return Err("RBW API item is ambiguous across accounts; use the item ID".into());
            }
            matched = Some((client, item));
        }
    }
    let (client, item) = matched.ok_or("RBW API item is not granted or was not found")?;
    client.read_item(&item, field, card).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_uris_retain_saved_matching_policy() {
        let uris =
            decode_uris(r#"[{"uri":"https://login.example/signin","match_type":3}]"#).unwrap();
        assert!(secrets::url_matches("https://login.example/signin", &uris));
        assert!(!secrets::url_matches("https://login.example/other", &uris));
        assert!(!secrets::url_matches("http://login.example/signin", &uris));
        assert!(
            decode_uris(r#"[{"uri":"https://login.example","match_type":"unexpected"}]"#).is_err()
        );
    }

    #[test]
    fn rejects_insecure_api_origins_before_reading_certificates() {
        for origin in [
            "http://vault.example",
            "https://vault.example/path",
            "https://user@vault.example",
            "https://vault.example/?token=x",
        ] {
            assert!(
                Client::new(
                    origin,
                    Path::new("missing"),
                    Path::new("missing"),
                    Path::new("missing")
                )
                .err()
                .unwrap()
                .contains("HTTPS origin"),
                "{origin}"
            );
        }
    }
}
