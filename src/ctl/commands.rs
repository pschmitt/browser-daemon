//! Command-line argument and subcommand definitions.

use super::*;

// ------------------------------------------------------------------ commands

#[derive(Args, Debug)]
pub struct SessionArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: SessionCmd,
}

#[derive(Subcommand, Debug)]
pub enum SessionCmd {
    /// List the sessions of the mesh.
    #[command(visible_alias = "ls")]
    List {
        /// Only this node.
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one session.
    Show {
        /// Id (or its start), or name.
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Start a session; prints its id and WebSocket URL.
    New(NewSession),
    /// Stop sessions.
    #[command(visible_aliases = ["rm", "stop"])]
    Delete {
        #[arg(required = true)]
        sessions: Vec<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Stop every session (of one node).
    StopAll {
        #[arg(long)]
        node: Option<String>,
        /// Do not ask.
        #[arg(short, long)]
        yes: bool,
    },
    /// Rename a session (empty: back to the generated name).
    Rename {
        session: String,
        name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Switch a session's proxy now (`none`: direct).
    Proxy {
        session: String,
        proxy: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Minutes of inactivity before a session stops (0: never).
    IdleTimeout {
        session: String,
        minutes: u64,
        #[arg(long)]
        node: Option<String>,
    },
    /// Start, stop, pause or resume a session's recording.
    Record {
        session: String,
        action: RecordAction,
        #[arg(long)]
        node: Option<String>,
    },
    /// Share a session publicly: anyone with the printed link watches its front tab
    /// (view only, no sign-in) until it stops or is unshared.
    Share {
        session: String,
        /// The link stops working after this long (`30m`, `12h`, `7d`, or `never`).
        #[arg(long, value_parser = parse_expiry, default_value = "1h")]
        expires: u64,
        /// Whoever has the link may also control the session (click, scroll, type),
        /// not only watch it (Chromium only).
        #[arg(long, conflicts_with = "view_only")]
        control: bool,
        /// Only watching (the default for a new link; turns control off).
        #[arg(long)]
        view_only: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        session: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Stop sharing a session: its public link stops working at once.
    Unshare {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// List a session's tabs.
    Tabs {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Open a URL in a new tab.
    Open {
        session: String,
        /// Page to open.
        #[arg(value_name = "URL")]
        page: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Close a tab (by target id, see `session tabs`).
    CloseTab {
        session: String,
        tab: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Keep a throwaway session's data (logins, cookies, storage) as a new profile.
    /// The browser restarts on the same pages as a session of the profile; prints its id.
    #[command(visible_alias = "save")]
    SaveAsProfile {
        session: String,
        /// Name of the new profile.
        name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Print a session's WebSocket URL (for Playwright or Puppeteer).
    Url {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// List what a session's pages downloaded (kept on its node until it ends).
    Downloads {
        session: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Save a file a session's page downloaded.
    Download {
        session: String,
        /// Download id (or its start), or its file name.
        download: String,
        /// Where to save it (default: its own name, here).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace an existing file.
        #[arg(short, long)]
        force: bool,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum RecordAction {
    Start,
    Stop,
    Pause,
    Resume,
}

#[derive(Args, Debug)]
pub struct NewSession {
    /// Node to start it on (default: the one asked).
    #[arg(long)]
    pub node: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_enum)]
    pub engine: Option<Engine>,
    /// Profile to use (name); its cookies and logins persist.
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub width: Option<u32>,
    #[arg(long)]
    pub height: Option<u32>,
    /// Record the session (default: the node's setting).
    #[arg(long, overrides_with = "no_record")]
    pub record: bool,
    #[arg(long)]
    pub no_record: bool,
    #[arg(long, overrides_with = "no_adblock")]
    pub adblock: bool,
    #[arg(long)]
    pub no_adblock: bool,
    /// Proxy id, or `none` for direct.
    #[arg(long)]
    pub proxy: Option<String>,
    #[arg(long, value_enum)]
    pub color_scheme: Option<Scheme>,
    /// Minutes of inactivity before it stops (0: never).
    #[arg(long)]
    pub idle_timeout: Option<u64>,
    /// Memory limit of the browser in MB.
    #[arg(long)]
    pub max_memory_mb: Option<u64>,
    #[arg(long, value_enum)]
    pub screen: Option<Screen>,
    /// Stop the session when its last client disconnects.
    #[arg(long)]
    pub ephemeral: bool,
    /// Do not open the profile's default tabs (a blank tab instead).
    #[arg(long)]
    pub no_default_tabs: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Engine {
    Chromium,
    Firefox,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Scheme {
    Dark,
    Light,
}

/// A profile setting: on, off, or the node's default.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Toggle {
    On,
    Off,
    Default,
}

impl Toggle {
    pub(super) fn json(self) -> Value {
        match self {
            Toggle::On => json!(true),
            Toggle::Off => json!(false),
            Toggle::Default => Value::Null,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Screen {
    Window,
    Native,
    Phone,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: ProfileCmd,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// List the profiles of the mesh.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one profile.
    Show {
        profile: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        profile: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Create an empty profile (or a copy of one).
    New {
        name: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long, value_enum)]
        engine: Option<Engine>,
        /// Copy this profile (on the same node).
        #[arg(long)]
        clone_from: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Page its sessions open with (repeat for several tabs).
        #[arg(long = "tab")]
        tabs: Vec<String>,
        /// Record its sessions (default: the node's setting).
        #[arg(long, value_enum)]
        record: Option<Toggle>,
        /// Its recordings take the sound (default: the node's setting).
        #[arg(long, value_enum)]
        record_audio: Option<Toggle>,
    },
    /// Change a profile's name or defaults.
    Set {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Proxy id for new sessions, or `none`.
        #[arg(long)]
        proxy: Option<String>,
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Default tabs: pages its sessions open with (repeat for several; replaces the list).
        #[arg(long = "tab")]
        tabs: Vec<String>,
        /// Remove all default tabs.
        #[arg(long)]
        clear_tabs: bool,
        /// Record its sessions: on, off, or the node's default.
        #[arg(long, value_enum)]
        record: Option<Toggle>,
        /// Its recordings take the sound: on, off, or the node's default.
        #[arg(long, value_enum)]
        record_audio: Option<Toggle>,
        /// Its picture on the dashboard: a png, svg, webp, jpeg or gif file.
        #[arg(long, value_name = "FILE")]
        icon: Option<String>,
        /// Remove its picture.
        #[arg(long, conflicts_with = "icon")]
        clear_icon: bool,
    },
    /// Copy a profile under a new name (same node).
    Clone {
        profile: String,
        new_name: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Delete a profile.
    #[command(visible_alias = "rm")]
    Delete {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// Stop its session first.
        #[arg(long)]
        force: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Wipe a profile's cookies and logins (a safety backup is kept).
    Clear {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// Stop its session first.
        #[arg(long)]
        force: bool,
        /// Skip the safety backup.
        #[arg(long)]
        no_backup: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Server-side backups of a profile.
    #[command(subcommand)]
    Backup(BackupCmd),
    /// Download a profile as a .tar.gz archive.
    Export {
        profile: String,
        #[arg(long)]
        node: Option<String>,
        /// File to write (default: <profile>.tar.gz).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace the file if it exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Create a profile from an archive made by `export`.
    Import {
        name: String,
        file: PathBuf,
        #[arg(long)]
        node: Option<String>,
        /// Replace a profile of that name (asks first).
        #[arg(long)]
        overwrite: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Copy or move a profile to another node.
    Transfer {
        profile: String,
        /// Target node.
        #[arg(long)]
        to: String,
        #[arg(long)]
        node: Option<String>,
        /// Name on the target (default: the same).
        #[arg(long)]
        name: Option<String>,
        /// Delete the source afterwards.
        #[arg(long = "move")]
        move_: bool,
        /// Replace a profile of that name on the target (asks first, as does --move).
        #[arg(long)]
        overwrite: bool,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    /// List a profile's backups.
    #[command(visible_alias = "ls")]
    List {
        profile: String,
        #[arg(long)]
        node: Option<String>,
    },
    /// Back a profile up (it must not be in use).
    Create {
        profile: String,
        label: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Replace the profile's data with a backup (a safety backup of the current data is kept).
    Restore {
        profile: String,
        backup: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a backup.
    #[command(visible_alias = "rm")]
    Delete {
        profile: String,
        backup: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct RecordingArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: RecordingCmd,
}

#[derive(Subcommand, Debug)]
pub enum RecordingCmd {
    /// List the recordings of the mesh.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        node: Option<String>,
    },
    /// Show one recording's details.
    Show { recording: String },
    /// Keep recordings: the cleanup (age, count and storage limits) never removes them.
    /// Nothing is kept unless asked.
    Keep {
        #[arg(required = true)]
        recordings: Vec<String>,
    },
    /// Stop keeping recordings: the cleanup may remove them again.
    Unkeep {
        #[arg(required = true)]
        recordings: Vec<String>,
    },
    /// Share a recording publicly: anyone with the printed link can play it, without
    /// signing in. A shared recording is also kept.
    Share {
        recording: String,
        /// The link stops working after this long (`30m`, `12h`, `7d`, or `never`).
        #[arg(long, value_parser = parse_expiry, default_value = "1h")]
        expires: u64,
    },
    /// Add a note (with TEXT), or list the notes (without).
    Note {
        recording: String,
        /// The note; `-` reads it from stdin.
        text: Option<String>,
    },
    /// Stop sharing a recording: its public link stops working at once.
    Unshare { recording: String },
    /// Delete recordings.
    #[command(visible_alias = "rm")]
    Delete {
        #[arg(required = true)]
        recordings: Vec<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Merge recordings of one node into a new one, in the order given (sound too).
    Merge {
        #[arg(required = true, num_args = 2..)]
        recordings: Vec<String>,
        /// Delete the originals once the merge is written.
        #[arg(long)]
        delete: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Download a recording's frames (JSON lines), or a WebM video of it with its sound.
    Export {
        recording: String,
        /// File to write (default: <id>.jsonl, or <id>.webm).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// A WebM video (VP9, with the sound), encoded on its node.
        #[arg(long)]
        webm: bool,
        /// Cap still periods at 5 seconds (concise mode).
        #[arg(long)]
        concise: bool,
        /// Replace the file if it exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Apply the retention limits now.
    Vacuum {
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct NodeArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: NodeCmd,
}

#[derive(Subcommand, Debug)]
pub enum NodeCmd {
    /// List the nodes of the mesh.
    #[command(visible_alias = "ls")]
    List,
    /// A node's settings, version and proxies.
    Show { node: Option<String> },
    /// Change a node's settings (proxy default, per-engine defaults).
    Set {
        /// Node (default: the one asked).
        node: Option<String>,
        /// Proxy id sessions use unless they choose another (`none`: direct).
        #[arg(long)]
        default_proxy: Option<String>,
        /// Engine the options below apply to.
        #[arg(long, value_enum)]
        engine: Option<Engine>,
        /// Block ads in new sessions of the engine.
        #[arg(long)]
        adblock: Option<bool>,
        /// Appearance of new sessions of the engine.
        #[arg(long, value_enum)]
        color_scheme: Option<Scheme>,
        /// Live views mark taps and clicks for the engine.
        #[arg(long)]
        show_touches: Option<bool>,
    },
    /// Add a peer.
    Add {
        /// The peer's base URL.
        #[arg(value_name = "URL")]
        peer: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a peer.
    #[command(visible_alias = "rm")]
    Remove { node: String },
}

// -------------------------------------------------------------------- client

#[derive(Args, Debug)]
pub struct ExtensionArgs {
    #[command(flatten)]
    pub conn: Conn,
    #[command(subcommand)]
    pub cmd: ExtensionCmd,
}

#[derive(Subcommand, Debug)]
pub enum ExtensionCmd {
    /// Make a single-use pairing code for the exact origin shown in the popup.
    Pair {
        #[arg(long)]
        origin: String,
        /// Seconds the browser may remain connected (60–28800; 0 disables expiry).
        #[arg(long, default_value_t = 3600)]
        lifetime_secs: u64,
        /// Remember this browser without key expiry, allowing reconnection after restart.
        #[arg(long)]
        remember: bool,
    },
    /// List connected browsers and explicitly shared tabs on this daemon.
    List,
    /// End a browser's grant now.
    Revoke { id: Uuid },
    /// Act on one shared tab. JSON params may be supplied on stdin with `-`.
    Command {
        id: Uuid,
        tab: u32,
        action: String,
        #[arg(default_value = "{}")]
        params: String,
    },
}
