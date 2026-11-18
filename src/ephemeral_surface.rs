//! What an ephemeral node (`--ephemeral-node`) serves: its browser and the
//! session APIs of its one session, and nothing of the mesh or the web UI.
//!
//! Such a node runs untrusted pages and is reached only by its coordinator
//! (which keeps the dashboard and relays the live view) and by the clients the
//! coordinator hands its address to. Its pages, and a browser exploit behind
//! them, must not find a dashboard, peers, the mesh overview, runs, profile
//! transfers or the vault agent's routes on it.
//!
//! It also cannot promote itself: its settings (vault access, default proxy,
//! labels, engine defaults) are read-only over the API. The cluster secret it
//! holds does not tell the coordinator from code running on the node, so a
//! change is made where the node is provisioned (its settings file), never
//! through a request.

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Pages, scripts and icons of the web UI.
const UI: &[&str] = &[
    "/",
    "/dashboard",
    "/favicon.ico",
    "/favicon.svg",
    "/manifest.webmanifest",
    "/manifest.json",
    "/site.webmanifest",
    "/sw.js",
    "/icon-192.png",
    "/icon-512.png",
    "/icon-maskable-512.png",
    "/download-worker.js",
];

/// Subtrees of the web UI (a trailing slash is part of the match).
const UI_TREES: &[&str] = &[
    "/assets/",
    "/extension/",
    "/handoff",
    "/share/",
    "/devtools/frontend/",
];

/// What reaches other nodes, or serves the mesh, the vault agent or shared browsers.
const MESH_TREES: &[&str] = &[
    "/api/peers",
    "/api/mesh/overview",
    "/api/mesh/placement",
    "/api/ws/overview",
    "/api/ephemeral",
    "/api/run",
    "/api/profiles",
    "/api/label-sync",
    "/api/secret-agent",
    "/api/extension",
    "/api/auth/token",
    "/api/shares",
];

/// Settings of the node and its labels: read, never changed.
const READ_ONLY: &[&str] = &["/api/settings", "/api/labels", "/api/item-labels"];

/// Whether an ephemeral node answers `method` on `path`.
pub(crate) fn allowed(method: &Method, path: &str) -> bool {
    if UI.contains(&path) {
        return false;
    }
    let under = |tree: &&str| {
        let bare = tree.trim_end_matches('/');
        path == bare || path.starts_with(&format!("{bare}/"))
    };
    if UI_TREES.iter().any(under) || MESH_TREES.iter().any(under) {
        return false;
    }
    if READ_ONLY.iter().any(under) {
        return matches!(*method, Method::GET | Method::HEAD);
    }
    true
}

pub(crate) async fn restrict(req: Request, next: Next) -> Response {
    if allowed(req.method(), req.uri().path()) {
        next.run(req).await
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_browser_and_its_session_not_the_ui_or_the_mesh() {
        let get = Method::GET;
        for ok in [
            "/health",
            "/api/settings",
            "/api/labels",
            "/api/mesh/node",
            "/api/sessions",
            "/api/sessions/1234/record",
            "/chromium",
            "/firefox",
            "/devtools/page/ABC",
            "/json/list",
            "/video",
            "/api/pages/ABC/auth",
        ] {
            assert!(allowed(&get, ok), "{ok}");
        }
        for no in [
            "/",
            "/dashboard",
            "/sw.js",
            "/assets/common.js",
            "/handoff/",
            "/handoff",
            "/handoff/ext/rbw/x.js",
            "/share/node/token",
            "/extension/",
            "/devtools/frontend/1/inspector.html",
            "/api/peers",
            "/api/peers/other/api/sessions",
            "/api/mesh/overview",
            "/api/ws/overview",
            "/api/ephemeral/nodes",
            "/api/run",
            "/api/profiles/work/transfer",
            "/api/secret-agent/next",
            "/api/extension/pair",
            "/api/auth/token",
        ] {
            assert!(!allowed(&get, no), "{no}");
        }
    }

    #[test]
    fn settings_and_labels_cannot_be_changed() {
        for path in [
            "/api/settings",
            "/api/labels/work",
            "/api/item-labels/session/1",
        ] {
            for m in [Method::PATCH, Method::PUT, Method::POST, Method::DELETE] {
                assert!(!allowed(&m, path), "{m} {path}");
            }
            assert!(allowed(&Method::GET, path), "GET {path}");
        }
        // The session's own routes still change things.
        assert!(allowed(&Method::POST, "/api/sessions/1/proxy"));
        assert!(allowed(&Method::POST, "/api/sessions"));
    }
}
