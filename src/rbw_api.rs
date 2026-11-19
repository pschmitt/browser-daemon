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
    consumer: &'static str,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .finish()
    }
}

/// The lock state of the accounts this client may see (`GET /v1/status`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub agent_version: Option<String>,
    pub accounts: Vec<AccountState>,
}
#[derive(Deserialize)]
pub struct AccountState {
    pub account: String,
    pub state: String,
}

/// A login read by item id: only the fields this client was granted. The TOTP
/// is the current code, never the seed.
pub struct Login {
    pub name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub totp: Option<String>,
    pub uris: Vec<Uri>,
    /// Custom fields worth filling (text ones, linked ones as the login field
    /// they stand for), as `{"name", "value"}`; only with a `custom_all` grant.
    pub custom: Vec<serde_json::Value>,
}

/// A login of a URL search (`GET /v1/items?url=`): metadata only, as far as
/// the client's grants reach.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchItem {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
    pub folder: Option<String>,
    pub favicon_host: Option<String>,
    pub site: bool,
    pub has_password: bool,
    pub has_totp: bool,
}
#[derive(Deserialize)]
struct SearchItems {
    items: Vec<SearchItem>,
}
#[derive(Deserialize)]
struct Passkeys {
    items: Vec<serde_json::Value>,
}
/// A custom field as `custom_all` lists it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomField {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<u64>,
    linked_id: Option<u64>,
    value: Option<String>,
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

/// The client key must be a file nobody else can read. systemd's credentials
/// (LoadCredential, in `credentials_dir`) are private to the unit but show as
/// 0440: their ACL mask lands in the group bits.
fn private_key_check(key: &Path, credentials_dir: Option<&std::ffi::OsStr>) -> Result<(), String> {
    let metadata = std::fs::metadata(key).map_err(|_| "Cannot read RBW API private key")?;
    let credential = credentials_dir.is_some_and(|dir| !dir.is_empty() && key.starts_with(dir));
    if !metadata.is_file() || (!credential && metadata.permissions().mode() & 0o077 != 0) {
        return Err("RBW API private key must be a private file (mode 0600)".into());
    }
    Ok(())
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
        private_key_check(key, std::env::var_os("CREDENTIALS_DIRECTORY").as_deref())?;
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
        Ok(Self {
            base,
            http,
            consumer: "browser-daemon-mcp",
        })
    }

    /// The name this client gives itself in `X-Rbw-Consumer` (rbw's audit log).
    pub fn consumer(mut self, consumer: &'static str) -> Self {
        self.consumer = consumer;
        self
    }

    fn url(&self, segments: &[&str]) -> Result<url::Url, String> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| "Invalid RBW API origin")?
            .clear()
            .extend(segments);
        Ok(url)
    }

    async fn answer<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, String> {
        let status = response.status();
        if status == reqwest::StatusCode::LOCKED {
            return Err("The vault is locked".into());
        }
        if !status.is_success() {
            return Err(format!(
                "RBW API refused the request (HTTP {})",
                status.as_u16()
            ));
        }
        response
            .json()
            .await
            .map_err(|_| "Invalid RBW API response".into())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, segments: &[&str]) -> Result<T, String> {
        self.get_query(segments, &[]).await
    }

    async fn get_query<T: serde::de::DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, String> {
        let mut url = self.url(segments)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let response = self
            .http
            .get(url)
            .header("X-Rbw-Consumer", self.consumer)
            .send()
            .await
            .map_err(|_| "RBW API request failed")?;
        Self::answer(response).await
    }

    /// The logins rbw matches for the page (its URI rules and its
    /// `defaultUriMatch`), or with words those matching them, the site's first.
    pub async fn search(&self, url: &str, query: Option<&str>) -> Result<Vec<SearchItem>, String> {
        let mut params = vec![("url", url)];
        if let Some(query) = query {
            params.push(("q", query));
        }
        let found: SearchItems = self.get_query(&["v1", "items"], &params).await?;
        Ok(found.items)
    }

    /// The passkeys for the relying party, as `rbw passkey find` lists them.
    pub async fn passkeys(&self, rp_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let found: Passkeys = self
            .get_query(&["v1", "passkeys"], &[("rp_id", rp_id)])
            .await?;
        Ok(found.items)
    }

    /// Has rbw sign one WebAuthn assertion with a granted passkey; the key
    /// never leaves rbw. None when this client has no such passkey (or may not
    /// use it), so another account's client can be asked.
    async fn assert(&self, body: &serde_json::Value) -> Result<Option<serde_json::Value>, String> {
        let response = self
            .http
            .post(self.url(&["v1", "passkeys", "assert"])?)
            .header("X-Rbw-Consumer", self.consumer)
            .json(body)
            .send()
            .await
            .map_err(|_| "RBW API request failed")?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN => Ok(None),
            _ => Self::answer(response).await.map(Some),
        }
    }

    /// The custom fields of an item worth filling, from its `custom_all` grant:
    /// text ones as they are, a linked one as the username (100) or password
    /// (101) it stands for, read through that field's own grant.
    async fn custom_fields(
        &self,
        item: &Item,
        login: &Login,
    ) -> Result<Vec<serde_json::Value>, String> {
        if !item.fields.iter().any(|f| f == "custom_all") {
            return Ok(Vec::new());
        }
        let value = self.field(&item.id, &["custom_all"]).await?;
        let fields: Vec<CustomField> =
            serde_json::from_str(&value).map_err(|_| "Invalid RBW API custom field list")?;
        let granted = |f: &str| item.fields.iter().any(|g| g == f);
        let mut out = Vec::new();
        for field in fields {
            let Some(name) = field.name.filter(|n| !n.is_empty()) else {
                continue;
            };
            let value = match (field.kind, field.linked_id) {
                (Some(0) | None, _) => field.value,
                (Some(3), Some(100)) => match &login.username {
                    Some(v) => Some(v.clone()),
                    None if granted("username") => {
                        self.optional_field(&item.id, &["username"]).await?
                    }
                    None => None,
                },
                (Some(3), Some(101)) => match &login.password {
                    Some(v) => Some(v.clone()),
                    None if granted("password") => {
                        self.optional_field(&item.id, &["password"]).await?
                    }
                    None => None,
                },
                _ => None,
            };
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                out.push(json!({ "name": name, "value": value }));
            }
        }
        Ok(out)
    }

    /// The lock state of the accounts granted to this client.
    pub async fn status(&self) -> Result<Status, String> {
        self.get(&["v1", "status"]).await
    }

    /// Unlocks one account with its master password (needs an unlock grant).
    /// The password goes in the request body only, never in a log or an error.
    pub async fn unlock(&self, account: &str, password: &str) -> Result<(), String> {
        let body = Zeroizing::new(
            serde_json::to_vec(&json!({ "account": account, "password": password }))
                .map_err(|_| "Cannot encode the unlock request")?,
        );
        let response = self
            .http
            .post(self.url(&["v1", "unlock"])?)
            .header("X-Rbw-Consumer", self.consumer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| "RBW API request failed")?;
        match response.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::FORBIDDEN => {
                Err(format!("This client may not unlock the account {account}"))
            }
            reqwest::StatusCode::UNPROCESSABLE_ENTITY => Err("rbw refused the password".into()),
            s => Err(format!("RBW API refused the unlock (HTTP {})", s.as_u16())),
        }
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

    /// The item's granted URIs with their saved match types.
    async fn uris(&self, item: &Item) -> Result<Vec<Uri>, String> {
        if item.fields.iter().any(|field| field == "uri_all") {
            let value = self.field(&item.id, &["uri_all"]).await?;
            return decode_uris(&value);
        }
        let mut uris = Vec::new();
        for token in &item.fields {
            if let Some(index) = token.strip_prefix("uri/") {
                let index: usize = index.parse().map_err(|_| "Invalid RBW API URI grant")?;
                let index = index.to_string();
                let uri = self.field(&item.id, &["uri", &index]).await?;
                let policy = self.field(&item.id, &["uri_match", &index]).await?;
                let match_type: Option<u64> =
                    serde_json::from_str(&policy).map_err(|_| "Invalid RBW API URI match type")?;
                uris.push(Uri { uri, match_type });
            }
        }
        Ok(uris)
    }

    /// The wanted login fields of an item; a field not wanted or not granted is
    /// None (and never read).
    async fn read_login(&self, item: &Item, want: &[String]) -> Result<Login, String> {
        let granted = |f: &str| want.iter().any(|w| w == f) && item.fields.iter().any(|g| g == f);
        let read = |f: &'static str| async move {
            if granted(f) {
                self.optional_field(&item.id, &[f]).await
            } else {
                Ok(None)
            }
        };
        let mut login = Login {
            name: item.name.clone(),
            username: read("username").await?,
            password: read("password").await?,
            totp: read("totp").await?,
            uris: self.uris(item).await?,
            custom: Vec::new(),
        };
        if want.iter().any(|w| w == "custom") {
            login.custom = self.custom_fields(item, &login).await?;
        }
        Ok(login)
    }

    async fn read_item(&self, item: &Item, field: &str, card: bool) -> Result<Secret, String> {
        let uris = if card {
            Vec::new()
        } else {
            self.uris(item).await?
        };
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

/// Whether every account the clients may see is unlocked (accounts no longer
/// configured do not count). False when none can be seen or rbw is unreachable.
pub async fn unlocked(clients: &[Client]) -> bool {
    let mut any = false;
    for client in clients {
        let Ok(status) = client.status().await else {
            return false;
        };
        for account in &status.accounts {
            match account.state.as_str() {
                "unlocked" => any = true,
                "unconfigured" => {}
                _ => return false,
            }
        }
    }
    any
}

/// The rbw-agent's version, as the first client's status reports it.
pub async fn agent_version(clients: &[Client]) -> Option<String> {
    clients.first()?.status().await.ok()?.agent_version
}

/// Unlocks every locked account the clients may unlock with the password.
pub async fn unlock(clients: &[Client], password: &str) -> Result<(), String> {
    let mut tried = false;
    for client in clients {
        for account in client.status().await?.accounts {
            if account.state == "locked" {
                tried = true;
                client.unlock(&account.account, password).await?;
            }
        }
    }
    if !tried && !unlocked(clients).await {
        return Err("No locked account to unlock".into());
    }
    Ok(())
}

/// The logins of every client for the page (see [`Client::search`]), the
/// site's first, at most `max`.
pub async fn search(
    clients: &[Client],
    url: &str,
    query: Option<&str>,
    max: usize,
) -> Result<Vec<SearchItem>, String> {
    let mut found = Vec::new();
    for client in clients {
        found.extend(client.search(url, query).await?);
    }
    // Stable: each client's own order stays within the site and other matches.
    found.sort_by_key(|item| !item.site);
    found.truncate(max);
    Ok(found)
}

/// The passkeys of every client for the relying party.
pub async fn passkeys(clients: &[Client], rp_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let mut found = Vec::new();
    for client in clients {
        found.extend(client.passkeys(rp_id).await?);
    }
    Ok(found)
}

/// An assertion signed by whichever client holds a matching passkey. The user
/// approved the sign-in (in the live view, or an agent's armed sign-in), so it
/// counts as verified, as `rbw passkey assert` does.
pub async fn assert(
    clients: &[Client],
    item: Option<&str>,
    rp_id: &str,
    origin: &str,
    challenge: &str,
    allow: &[String],
) -> Result<serde_json::Value, String> {
    let mut body = json!({
        "rpId": rp_id,
        "origin": origin,
        "challenge": challenge,
        "allowCredentials": allow,
        "userVerified": true,
    });
    if let Some(item) = item {
        body["itemId"] = json!(item);
    }
    for client in clients {
        if let Some(assertion) = client.assert(&body).await? {
            return Ok(assertion);
        }
    }
    Err(format!(
        "No passkey for {rp_id} is granted to the secret agent"
    ))
}

/// The wanted fields (`username`, `password`, `totp`, `custom`) and the URIs of the
/// login with this item id, from whichever client was granted it.
pub async fn login(clients: &[Client], id: &str, want: &[String]) -> Result<Login, String> {
    for client in clients {
        let items: Items = client.get(&["v1", "items"]).await?;
        if let Some(item) = items.items.into_iter().find(|item| item.id == id) {
            return client.read_login(&item, want).await;
        }
    }
    Err("RBW API item is not granted or was not found".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_accepts_0600_and_systemd_credentials_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = |name: &str, mode: u32| {
            let path = dir.path().join(name);
            std::fs::write(&path, "key").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let private = key("private", 0o600);
        // How a LoadCredential file looks to the service: its ACL mask shows as 0440.
        let credential = key("credential", 0o440);
        let shared = key("shared", 0o644);
        let credentials = Some(dir.path().as_os_str());
        let elsewhere = Some(std::ffi::OsStr::new("/run/credentials/other.service"));

        assert!(private_key_check(&private, None).is_ok());
        assert!(private_key_check(&credential, credentials).is_ok());
        assert!(private_key_check(&credential, None).is_err());
        assert!(private_key_check(&credential, elsewhere).is_err());
        assert!(private_key_check(&credential, Some(std::ffi::OsStr::new(""))).is_err());
        // Only the credential directory relaxes the mode, and only for files in it.
        assert!(private_key_check(&shared, None).is_err());
        assert!(private_key_check(&dir.path().join("missing"), credentials).is_err());
        assert!(private_key_check(dir.path(), credentials).is_err());
    }

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
    fn search_and_custom_field_answers_decode() {
        let found: SearchItems = serde_json::from_str(
            r#"{"items":[{"id":"i","name":"Bank","fields":["username"],"username":"me","folder":null,"faviconHost":"bank.example","site":true,"hasPassword":true,"hasTotp":false}]}"#,
        )
        .unwrap();
        let item = &found.items[0];
        assert!(item.site && item.has_password && !item.has_totp);
        assert_eq!(item.favicon_host.as_deref(), Some("bank.example"));
        let fields: Vec<CustomField> = serde_json::from_str(
            r#"[{"name":"pin","type":0,"linkedId":null,"value":"1234"},{"name":"u","type":3,"linkedId":100,"value":null}]"#,
        )
        .unwrap();
        assert_eq!(fields[0].kind, Some(0));
        assert_eq!(fields[1].linked_id, Some(100));
        assert!(fields[1].value.is_none());
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
