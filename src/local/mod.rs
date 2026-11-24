//! The local tab API: the tabs of the user's own browsers, on this machine.
//!
//! The node extension pushes a snapshot of its tabs to a native messaging host
//! (`browser-daemon-native-host`) on every change; the host relays it to the
//! local server (`browser-daemon local serve`), which keeps the state in
//! memory and answers token-authenticated requests on a Unix socket. The
//! `browser-daemon tabs` client asks that server. See docs/local-api.md.

mod client;
mod install;
mod native;
mod server;
mod tokens;

use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

pub use native::run as native_host;

/// The native messaging host's name, registered for Firefox and Chromium.
pub const HOST_NAME: &str = "dev.pschmitt.browser_daemon_local";
/// The program name the browsers start; `main` dispatches on it.
pub const HOST_PROGRAM: &str = "browser-daemon-native-host";
pub const FIREFOX_EXTENSION_ID: &str = "browser-daemon-node@pschmitt.dev";
pub const CHROMIUM_EXTENSION_ID: &str = "cjbnnociajogjgnpihfkhnbehmkdpijf";

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// `$XDG_RUNTIME_DIR/browser-daemon`, where both sockets live.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid cannot fail.
            std::env::temp_dir().join(format!("browser-daemon-{}", unsafe { libc::getuid() }))
        })
        .join("browser-daemon")
}

/// The API socket clients connect to.
pub fn api_socket() -> PathBuf {
    std::env::var_os("BROWSER_DAEMON_LOCAL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("local.sock"))
}

/// The socket native hosts feed the tab state into.
pub fn feeder_socket() -> PathBuf {
    std::env::var_os("BROWSER_DAEMON_LOCAL_FEEDER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("feeder.sock"))
}

/// `$XDG_CONFIG_HOME/browser-daemon`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("browser-daemon")
}

/// One tab as the extension reports it. Private windows are never reported.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tab {
    pub id: i64,
    pub window_id: i64,
    pub index: i64,
    pub title: String,
    pub url: String,
    pub active: bool,
    pub audible: bool,
    pub muted: bool,
    pub pinned: bool,
    pub discarded: bool,
    pub favicon: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Window {
    pub id: i64,
    pub focused: bool,
}

/// What a native host relays from the extension, one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Feed {
    /// First message of a connection: which browser and profile this is.
    Hello {
        /// `firefox` or `chromium`.
        browser: String,
        /// A random id the extension keeps per browser profile.
        profile: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        version: String,
        /// The client's name in tab ids (default `<host>-<browser>`, the name
        /// the extension pairs with by default).
        #[serde(default)]
        label: String,
    },
    /// The complete current state; replaces the previous one.
    Snapshot {
        windows: Vec<Window>,
        tabs: Vec<Tab>,
    },
}

/// `browser-daemon local ...`
#[derive(Args, Debug)]
pub struct LocalArgs {
    #[command(subcommand)]
    pub command: LocalCommand,
}

#[derive(Subcommand, Debug)]
pub enum LocalCommand {
    /// Run the local server (what the systemd user service does).
    Serve(server::ServeArgs),
    /// API tokens: create, list, revoke.
    #[command(subcommand)]
    Token(tokens::TokenCommand),
    /// Register the native messaging host with Firefox and Chromium for this user.
    Install(install::InstallArgs),
    /// The native messaging host itself (the browsers start it).
    #[command(hide = true)]
    NativeHost {
        /// What the browser passes (manifest path, extension id or origin).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
}

pub async fn local(args: LocalArgs) -> R<()> {
    match args.command {
        LocalCommand::Serve(a) => server::serve(a).await,
        LocalCommand::Token(c) => tokens::command(c),
        LocalCommand::Install(a) => install::install(a),
        LocalCommand::NativeHost { .. } => native::run().await,
    }
}

pub use client::{tabs, TabsArgs};
