//! Live-view extensions: UI bundles (a manifest, pages, scripts, styles,
//! icons) the live view runs in sandboxed frames, such as rbw's vault (which
//! rbw ships). The daemon only serves their files from the directories given
//! with `--extension-dir`; what they may ask for is the live view's (see
//! docs/extensions.md).

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::state::AppState;

/// The version of the contract between the live view and an extension.
pub const API: u32 = 1;

/// What an extension may ask the live view for.
const PERMISSIONS: &[&str] = &["vault"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub name: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Image for its toolbar button (and the dashboard), relative to the bundle.
    pub icon: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    /// A button in the live view's toolbar that opens `entry` in a popover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<Slot>,
    /// Asked when a page started from the view wants a passkey: `entry` in a dialog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passkey: Option<Slot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub entry: String,
    /// The toolbar button's tooltip and label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

struct Extension {
    manifest: Manifest,
    dir: PathBuf,
}

#[derive(Default)]
pub struct Registry {
    list: Vec<Extension>,
}

impl Registry {
    /// The extensions in `dirs`, each a directory with a `manifest.json`.
    pub fn load(dirs: &[PathBuf]) -> Result<Self, String> {
        let mut list: Vec<Extension> = Vec::new();
        for dir in dirs {
            let raw = std::fs::read(dir.join("manifest.json"))
                .map_err(|e| format!("{}: {e}", dir.join("manifest.json").display()))?;
            let manifest = parse(&raw).map_err(|e| format!("{}: {e}", dir.display()))?;
            for entry in slots(&manifest).chain([manifest.icon.as_str()]) {
                if read_dir_file(dir, entry).is_none() {
                    return Err(format!("{}: {entry} is missing", dir.display()));
                }
            }
            if let Some(other) = list.iter().find(|e| e.manifest.name == manifest.name) {
                return Err(format!(
                    "{}: {:?} is loaded from {} already",
                    dir.display(),
                    manifest.name,
                    other.dir.display()
                ));
            }
            list.push(Extension {
                manifest,
                dir: dir.clone(),
            });
        }
        Ok(Self { list })
    }

    fn get(&self, name: &str) -> Option<&Extension> {
        self.list.iter().find(|e| e.manifest.name == name)
    }

    /// A file of an extension, read now (an extension directory being worked
    /// on shows its changes on the next load of the live view).
    fn file(&self, name: &str, path: &str) -> Option<Vec<u8>> {
        read_dir_file(&self.get(name)?.dir, path)
    }
}

fn slots(m: &Manifest) -> impl Iterator<Item = &str> {
    [m.toolbar.as_ref(), m.passkey.as_ref()]
        .into_iter()
        .flatten()
        .map(|s| s.entry.as_str())
}

/// Only plain relative paths inside the directory: no `..`, no absolute paths,
/// no hidden files, and nothing a symlink leads out of.
fn read_dir_file(dir: &Path, path: &str) -> Option<Vec<u8>> {
    if !safe_path(path) {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let file = root.join(path).canonicalize().ok()?;
    if !file.starts_with(&root) || !file.is_file() {
        return None;
    }
    std::fs::read(file).ok()
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path).components().all(|c| match c {
            Component::Normal(part) => part
                .to_str()
                .is_some_and(|s| !s.starts_with('.') && !s.contains('\\')),
            _ => false,
        })
}

fn parse(raw: &[u8]) -> Result<Manifest, String> {
    let m: Manifest = serde_json::from_slice(raw).map_err(|e| format!("manifest.json: {e}"))?;
    if m.api != API {
        return Err(format!(
            "manifest.json: api {} (this browser-daemon speaks {API})",
            m.api
        ));
    }
    if m.name.is_empty()
        || m.name.len() > 40
        || !m
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("manifest.json: name must be lowercase letters, digits and -".into());
    }
    if let Some(p) = m
        .permissions
        .iter()
        .find(|p| !PERMISSIONS.contains(&p.as_str()))
    {
        return Err(format!("manifest.json: unknown permission {p:?}"));
    }
    if m.passkey.is_some() && !m.permissions.iter().any(|p| p == "vault") {
        return Err("manifest.json: the passkey slot needs the vault permission".into());
    }
    for path in slots(&m).chain([m.icon.as_str()]) {
        if !safe_path(path) {
            return Err(format!(
                "manifest.json: {path:?} is not a path inside the bundle"
            ));
        }
    }
    Ok(m)
}

#[derive(Serialize)]
struct Listed<'a> {
    #[serde(flatten)]
    manifest: &'a Manifest,
    /// Where its files are served.
    base: String,
}

/// `GET /handoff/extensions`: what the live view loads.
pub(crate) async fn list(State(state): State<AppState>) -> Response {
    let list: Vec<Listed> = state
        .view_extensions
        .list
        .iter()
        .map(|e| Listed {
            manifest: &e.manifest,
            base: format!("/handoff/ext/{}/", e.manifest.name),
        })
        .collect();
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        Json(serde_json::json!({ "api": API, "extensions": list })),
    )
        .into_response()
}

/// `GET /handoff/ext/<name>/<path>`: an extension's file. Its pages are
/// sandboxed (an opaque origin: no cookie, no storage of this site, no access
/// to the live view) and may only load their own bundle, not connect anywhere.
pub(crate) async fn file(
    State(state): State<AppState>,
    AxumPath((name, path)): AxumPath<(String, String)>,
) -> Response {
    let Some(body) = state.view_extensions.file(&name, &path) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let kind = content_type(&path);
    let mut res = (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(kind)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            // The sandboxed pages are an opaque origin: their scripts and styles
            // are cross-origin loads.
            (
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            ),
        ],
        body,
    )
        .into_response();
    if kind.starts_with("text/html") {
        res.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
    }
    res
}

/// For an extension's pages, wherever they are opened.
pub(crate) const CSP: &str = "sandbox allow-scripts; default-src 'none'; \
    script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
    font-src 'self' data:; connect-src 'none'; frame-ancestors 'self'; \
    base-uri 'none'; form-action 'none'";

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_bundle() {
        for bad in [
            "",
            "../x",
            "/etc/passwd",
            "a/../../b",
            ".hidden",
            "a/.git/x",
            "a\\b",
            "./a",
        ] {
            assert!(!safe_path(bad), "{bad}");
        }
        for good in ["popup.html", "vendor/theme.css", "a/b/c.js"] {
            assert!(safe_path(good), "{good}");
        }
    }

    fn bundle(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            format!(
                r#"{{"api": 1, "name": "{name}", "title": "{name}", "icon": "i.png", "permissions": ["vault"], "toolbar": {{"entry": "p.html"}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("i.png"), b"png").unwrap();
        std::fs::write(dir.path().join("p.html"), b"<!doctype html>").unwrap();
        dir
    }

    #[test]
    fn a_directory_serves_its_files_and_nothing_else() {
        let dir = bundle("rbw");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"no").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let r = Registry::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(r.list.len(), 1);
        assert_eq!(
            r.file("rbw", "p.html").as_deref(),
            Some(&b"<!doctype html>"[..])
        );
        assert!(r.file("rbw", "../secret.txt").is_none());
        assert!(r.file("rbw", "link.txt").is_none());
        assert!(r.file("rbw", "missing.js").is_none());
        assert!(r.file("other", "p.html").is_none());
    }

    #[test]
    fn names_are_unique_and_entries_exist() {
        let (a, b) = (bundle("rbw"), bundle("rbw"));
        assert!(Registry::load(&[a.path().to_path_buf(), b.path().to_path_buf()]).is_err());
        std::fs::remove_file(b.path().join("p.html")).unwrap();
        assert!(Registry::load(&[b.path().to_path_buf()]).is_err());
        assert!(Registry::load(&[]).unwrap().list.is_empty());
    }

    #[test]
    fn manifests_are_checked() {
        let bad = [
            r#"{"api": 2, "name": "x", "title": "x", "icon": "i.png"}"#,
            r#"{"api": 1, "name": "X Y", "title": "x", "icon": "i.png"}"#,
            r#"{"api": 1, "name": "x", "title": "x", "icon": "../i.png"}"#,
            r#"{"api": 1, "name": "x", "title": "x", "icon": "i.png", "permissions": ["cookies"]}"#,
            r#"{"api": 1, "name": "x", "title": "x", "icon": "i.png", "passkey": {"entry": "p.html"}}"#,
            r#"{"api": 1, "name": "x", "title": "x", "icon": "i.png", "extra": 1}"#,
        ];
        for m in bad {
            assert!(parse(m.as_bytes()).is_err(), "{m}");
        }
    }
}
