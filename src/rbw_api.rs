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

fn resolve_item<'a>(items: &'a [Item], entry: &str) -> Result<&'a Item, String> {
    let mut matches = items
        .iter()
        .filter(|item| item.id == entry || item.name.as_deref() == Some(entry));
    let item = matches
        .next()
        .ok_or("RBW API item is not granted or was not found")?;
    if matches.next().is_some() {
        return Err("RBW API item name is ambiguous; use the item ID".into());
    }
    Ok(item)
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
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
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

    pub async fn lookup(
        &self,
        entry: &str,
        field: &str,
        card: bool,
        uris: Vec<Uri>,
    ) -> Result<Secret, String> {
        let items: Items = self.get(&["v1", "items"]).await?;
        let item = resolve_item(&items.items, entry)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_selection_refuses_ambiguous_names_and_ungranted_ids() {
        let items: Items = serde_json::from_value(json!({"items": [
            {"id": "one", "name": "shared", "fields": ["password"]},
            {"id": "two", "name": "shared", "fields": ["card_number"]}
        ]}))
        .unwrap();
        assert!(resolve_item(&items.items, "shared").is_err());
        assert_eq!(resolve_item(&items.items, "two").unwrap().id, "two");
        assert!(resolve_item(&items.items, "ungranted").is_err());
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
