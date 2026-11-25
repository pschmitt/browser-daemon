//! `browser-daemon tui`: a terminal view of the mesh (sessions, profiles,
//! recordings, nodes) with the dashboard's actions, talking to the HTTP API.
//!
//! Everything the keys do is an [`Action`]: an API call, a menu, a prompt, a
//! confirmation or a form. The item menu (`m`) offers all of them for the selected row.

use std::path::PathBuf;

use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

use crate::profiles::ColorScheme;
use crate::settings::{EngineDefaults, NodeSettings};
use crate::{BrowserEngine, MeshOverview, MeshProfile, MeshRecording, MeshSession};

const REFRESH: Duration = Duration::from_secs(3);
const DEFAULT_SECRET_FILE: &str = "/run/secrets/browser-daemon/cluster-secret";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sessions,
    Profiles,
    Recordings,
    Nodes,
    Engines,
}

const TABS: [Tab; 5] = [
    Tab::Sessions,
    Tab::Profiles,
    Tab::Recordings,
    Tab::Nodes,
    Tab::Engines,
];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Sessions => "Sessions",
            Tab::Profiles => "Profiles",
            Tab::Recordings => "Recordings",
            Tab::Nodes => "Nodes",
            Tab::Engines => "Engines",
        }
    }

    fn glyph(self) -> char {
        match self {
            Tab::Sessions => '\u{f108}',
            Tab::Profiles => '\u{f2bd}',
            Tab::Recordings => '\u{f03d}',
            Tab::Nodes => '\u{f233}',
            Tab::Engines => '\u{f085}',
        }
    }
}

/// An engine, with the defaults its new sessions start with (as set on this node,
/// and applied to every online node).
struct EngineRow {
    engine: BrowserEngine,
    defaults: EngineDefaults,
}

fn tri_label(v: Option<bool>) -> &'static str {
    match v {
        None => "default",
        Some(true) => "on",
        Some(false) => "off",
    }
}

fn scheme_label(v: Option<ColorScheme>) -> &'static str {
    match v {
        None => "default",
        Some(ColorScheme::Dark) => "dark",
        Some(ColorScheme::Light) => "light",
    }
}

/// Stands for the text typed into a prompt, in an action's path, body or label.
const INPUT: &str = "$INPUT";

/// A request to the API, to be sent when chosen.
#[derive(Clone)]
struct Call {
    label: String,
    method: Method,
    /// Node it is for; the path is relative to that node's API.
    node: String,
    path: String,
    body: Option<Value>,
    /// File sent as the request body.
    upload: Option<String>,
    /// File the answer is written to.
    save_to: Option<String>,
    /// Takes a while (archives).
    long: bool,
}

impl Call {
    fn new(label: impl Into<String>, method: Method, node: &str, path: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            method,
            node: node.to_string(),
            path: path.into(),
            body: None,
            upload: None,
            save_to: None,
            long: false,
        }
    }

    fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    fn upload(mut self, file: &str) -> Self {
        self.upload = Some(file.to_string());
        self.long = true;
        self
    }

    fn long(mut self) -> Self {
        self.long = true;
        self
    }

    fn save_to(mut self, file: &str) -> Self {
        self.save_to = Some(file.to_string());
        self.long = true;
        self
    }

    fn with_input(&self, input: &str) -> Self {
        Self {
            label: self.label.replace(INPUT, input),
            path: self.path.replace(INPUT, &urlencode(input)),
            body: self.body.as_ref().map(|b| subst(b, input)),
            upload: self.upload.as_ref().map(|f| f.replace(INPUT, input)),
            save_to: self.save_to.as_ref().map(|f| f.replace(INPUT, input)),
            ..self.clone()
        }
    }
}

fn subst(v: &Value, input: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace(INPUT, input)),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst(x, input)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), subst(x, input)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[derive(Clone)]
enum Action {
    Api(Call),
    /// Several calls at once (one per node), reported as one.
    ApiAll(String, Vec<Call>),
    Open(String),
    OpenBrowserTab(BrowserTarget, String),
    /// Play a recording (node, id): in this terminal (true) or in a player window.
    Play(String, String, bool),
    Say(String),
    Menu {
        title: String,
        items: Vec<(String, Action)>,
    },
    /// Ask for a line of text, then do `then` with it in place of `$INPUT`.
    Input {
        title: String,
        prompt: String,
        initial: String,
        then: Box<Action>,
    },
    Confirm {
        question: String,
        then: Box<Action>,
    },
    Text {
        title: String,
        lines: Vec<String>,
        /// A session whose screenshot goes above the text.
        shot: Option<String>,
    },
    /// The form for a new session or profile; the node it starts on.
    Form(FormKind, Option<String>),
    /// Stop these sessions (node, id), as counted when the confirmation was asked.
    StopAll(Vec<(String, String)>),
    Filter(Option<String>),
    /// A profile's backups and data actions (fetches the backups).
    DataMenu(MeshProfile),
}

impl Action {
    fn with_input(&self, input: &str) -> Self {
        match self {
            Action::Api(c) => Action::Api(c.with_input(input)),
            Action::Open(u) => Action::Open(u.replace(INPUT, &urlencode(input))),
            Action::Say(t) => Action::Say(t.replace(INPUT, input)),
            other => other.clone(),
        }
    }
}

/// How often the selected session's screenshot is fetched again.
/// Nerd Font glyphs, unless `BROWSER_DAEMON_TUI_ICONS=off` (a terminal font without them).
fn icons() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("BROWSER_DAEMON_TUI_ICONS").as_deref(),
            Ok("off" | "0" | "no")
        )
    })
}

/// `glyph` and a space, or nothing when icons are off.
fn glyph(g: char) -> String {
    if icons() {
        format!("{g} ")
    } else {
        String::new()
    }
}

/// The engine's logo in its brand colour, then its name.
fn engine_span(e: BrowserEngine) -> Line<'static> {
    let (g, color) = if e == BrowserEngine::Firefox {
        ('\u{f269}', Color::Rgb(255, 113, 57))
    } else {
        ('\u{f268}', Color::Rgb(66, 133, 244))
    };
    Line::from(vec![
        Span::raw(glyph(g)).fg(color),
        Span::raw(e.to_string()),
    ])
}

/// The web UI's colour for a node (`hueOf` in assets/common.js, at the dark theme's lightness).
fn node_color(name: &str) -> Color {
    let hash = name.to_lowercase().chars().fold(0i32, |h, c| {
        h.wrapping_mul(31).wrapping_add(c as u32 as i32)
    });
    let hue = (f64::from(hash).abs() * 137.508 % 360.0).round();
    let (s, l) = (0.68, 0.68);
    let a = s * f64::min(l, 1.0 - l);
    let channel = |n: f64| {
        let k = (n + hue / 30.0) % 12.0;
        let v = l - a * f64::min(k - 3.0, 9.0 - k).clamp(-1.0, 1.0);
        (v * 255.0).round() as u8
    };
    Color::Rgb(channel(0.0), channel(8.0), channel(4.0))
}

fn node_cell(name: &str) -> Cell<'static> {
    Cell::from(name.to_string()).fg(node_color(name))
}

/// Rows of the screenshot in a session's details popup.
const SHOT_ROWS: usize = 16;
const SHOT_EVERY: Duration = Duration::from_secs(10);

/// What the terminal can draw (kitty graphics, sixel, iTerm2), unless
/// `BROWSER_DAEMON_TUI_IMAGES=off`. None: no images.
///
/// Inside tmux nothing is queried: tmux passes a query on to every client viewing
/// the window, their replies count as input, and with `window-size latest` the
/// window jumps to another client's size. tmux knows each client's terminal type,
/// features and cell size, so the client looking at this pane decides.
fn image_picker() -> Option<Picker> {
    if matches!(
        std::env::var("BROWSER_DAEMON_TUI_IMAGES").as_deref(),
        Ok("off" | "0" | "no")
    ) {
        return None;
    }
    if std::env::var_os("TMUX").is_some() {
        return tmux_picker();
    }
    let started = Instant::now();
    let picker = Picker::from_query_stdio().ok();
    if started.elapsed() >= Duration::from_millis(900) {
        // ratatui-image gives up after 1s but leaves its query thread blocked on stdin,
        // where it swallows keystrokes until it reads a Device Status Report.
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[5n").and_then(|()| out.flush());
        std::thread::sleep(Duration::from_millis(200));
    }
    picker.filter(|p| p.protocol_type() != ProtocolType::Halfblocks)
}

fn tmux(args: &[&str]) -> Option<String> {
    std::process::Command::new("tmux")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The tmux client looking at a window: its terminal and cell size.
#[derive(Debug, PartialEq)]
struct TmuxViewer {
    /// `client_termtype`, e.g. `kitty(0.50.0)` (empty when tmux does not know).
    termtype: String,
    /// `client_termfeatures`, e.g. `RGB,sixel,sync`.
    features: String,
    cell: (u16, u16),
}

const TMUX_CLIENT_FORMAT: &str = "#{client_activity}\t#{window_id}\t#{client_cell_width}\t\
     #{client_cell_height}\t#{client_termtype}\t#{client_termfeatures}";

/// A picker for the tmux client viewing this pane, without querying the terminal.
fn tmux_picker() -> Option<Picker> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let window = tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"])?;
    let clients = tmux(&["list-clients", "-F", TMUX_CLIENT_FORMAT])?;
    let viewer = tmux_viewer(&clients, window.trim())?;
    let protocol = tmux_protocol(&viewer)?;
    // from_fontsize turns on tmux passthrough; the protocol is ours.
    #[allow(deprecated)]
    let mut picker =
        Picker::from_fontsize(ratatui_image::FontSize::new(viewer.cell.0, viewer.cell.1));
    picker.set_protocol_type(protocol);
    Some(picker)
}

/// The most recently active client viewing `window`, from `list-clients` output in
/// TMUX_CLIENT_FORMAT.
fn tmux_viewer(clients: &str, window: &str) -> Option<TmuxViewer> {
    clients
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [activity, window_id, width, height, termtype, features] = fields[..] else {
                return None;
            };
            let (width, height): (u16, u16) = (width.parse().ok()?, height.parse().ok()?);
            (window_id == window && width > 0 && height > 0).then(|| {
                (
                    activity.parse::<u64>().unwrap_or(0),
                    TmuxViewer {
                        termtype: termtype.to_owned(),
                        features: features.to_owned(),
                        cell: (width, height),
                    },
                )
            })
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, viewer)| viewer)
}

/// The image protocol of a tmux client's terminal; None: no images.
fn tmux_protocol(viewer: &TmuxViewer) -> Option<ProtocolType> {
    let termtype = viewer.termtype.to_lowercase();
    if termtype.starts_with("kitty") || termtype.starts_with("ghostty") {
        Some(ProtocolType::Kitty)
    } else if termtype.starts_with("wezterm") || termtype.starts_with("iterm2") {
        Some(ProtocolType::Iterm2)
    } else if viewer.features.split(',').any(|f| f == "sixel") {
        Some(ProtocolType::Sixel)
    } else {
        None
    }
}

/// Size of a terminal cell in pixels (a typical one when the terminal does not say).
fn cell_px() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills the winsize struct we pass.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row)
    } else {
        (8, 16)
    }
}

fn item(label: impl Into<String>, action: Action) -> (String, Action) {
    (label.into(), action)
}

fn confirm_then(question: impl Into<String>, then: Action) -> Action {
    Action::Confirm {
        question: question.into(),
        then: Box::new(then),
    }
}

fn ask(title: &str, prompt: &str, initial: &str, then: Action) -> Action {
    Action::Input {
        title: title.into(),
        prompt: prompt.into(),
        initial: initial.into(),
        then: Box::new(then),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormKind {
    Session,
    Profile,
}

const SESSION_TARGET_FIELD: usize = 10;

struct Field {
    label: &'static str,
    kind: FieldKind,
}

enum FieldKind {
    Text(String),
    /// (value, shown) options and the chosen one.
    Choice {
        options: Vec<(String, String)>,
        sel: usize,
    },
}

impl Field {
    fn text(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text(String::new()),
        }
    }

    fn choice(label: &'static str, options: Vec<(String, String)>) -> Self {
        Self {
            label,
            kind: FieldKind::Choice { options, sel: 0 },
        }
    }

    fn value(&self) -> String {
        match &self.kind {
            FieldKind::Text(t) => t.trim().to_string(),
            FieldKind::Choice { options, sel } => {
                options.get(*sel).map(|o| o.0.clone()).unwrap_or_default()
            }
        }
    }
}

struct Form {
    kind: FormKind,
    fields: Vec<Field>,
    sel: usize,
}

enum Modal {
    Menu {
        title: String,
        items: Vec<(String, Action)>,
        sel: usize,
    },
    Input {
        title: String,
        prompt: String,
        buf: String,
        then: Box<Action>,
    },
    Confirm {
        question: String,
        then: Box<Action>,
    },
    Text {
        title: String,
        lines: Vec<String>,
        scroll: usize,
        shot: Option<String>,
    },
    Form(Form),
}

/// What background tasks report to the event loop. Nothing in the loop waits on
/// the network: requests run in spawned tasks and come back as one of these.
enum Msg {
    /// The overview, or why it could not be fetched.
    Overview(Result<MeshOverview, String>),
    /// An overview the daemon pushed (`/api/ws/overview`).
    Pushed(MeshOverview),
    Browsers(Vec<BrowserTarget>),
    BrowserTab(Result<String, String>),
    /// The push stream connected (true) or dropped (false): polling covers the gaps.
    Live(bool),
    /// An action finished: its message (empty: "done"), or its error.
    Done {
        label: String,
        result: Result<String, String>,
        refresh: bool,
    },
    /// A session's screenshot (None: there is none to show).
    Shot(String, Option<image::DynamicImage>),
    /// A profile's picture, by `icon_key`.
    Icon(String, Option<image::DynamicImage>),
    /// A profile's backups, for its data menu.
    Backups(MeshProfile, Result<Vec<Value>, String>),
    /// A recording downloaded to play here: the event loop hands mpv the terminal.
    PlayHere(tempfile::NamedTempFile),
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Debug)]
struct BrowserTarget {
    daemon: String,
    node: crate::extension::NodeInfo,
}

impl BrowserTarget {
    fn key(&self) -> String {
        format!("browser:{}:{}", self.daemon, self.node.id)
    }
}

struct App {
    tx: tokio::sync::mpsc::UnboundedSender<Msg>,
    /// Requests of actions still running.
    pending: usize,
    /// The overview is being fetched.
    refreshing: bool,
    /// Draws images on this terminal (None: it cannot, or it is switched off).
    picker: Option<Picker>,
    /// A downloaded recording to play in this terminal next (see `play_here`).
    play_here: Option<tempfile::NamedTempFile>,
    /// The screenshots fetched so far, by session id.
    shots: HashMap<String, StatefulProtocol>,
    /// When a session's screenshot was last asked for.
    shot_asked: HashMap<String, Instant>,
    /// The profiles' pictures, and which were asked for already.
    icons: HashMap<String, StatefulProtocol>,
    icon_asked: std::collections::HashSet<String>,
    /// Where the last frame put the tab titles (columns), the table, the popup.
    tab_spans: Vec<(u16, u16)>,
    body: Rect,
    popup: Option<Rect>,
    screen: Rect,
    /// The last left click on a row: when and where, for double clicks.
    last_click: Option<(Instant, u16, u16)>,
    /// Another refresh was asked for while one ran.
    refresh_again: bool,
    /// Frames drawn, for the spinner.
    tick: usize,
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
    ov: Option<MeshOverview>,
    browsers: Vec<BrowserTarget>,
    browsers_refresh: Option<Instant>,
    browsers_pending: bool,
    tab: Tab,
    tables: [TableState; 5],
    status: String,
    error: bool,
    /// The status shown is the refresh's own error.
    refresh_error: bool,
    modal: Option<Modal>,
    /// Show only this node's sessions, profiles and recordings.
    filter: Option<String>,
    help: bool,
    last_refresh: Option<Instant>,
    /// The daemon pushes overviews: no polling needed.
    live: bool,
}

impl App {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .timeout(Duration::from_secs(20));
        if let Some(ref s) = self.secret {
            req = req.header("X-Browser-Cluster-Secret", s);
        }
        req
    }

    /// API path on `node`: local, or through the daemon's peer proxy.
    fn api(&self, node: &str, path: &str) -> String {
        let local = node.is_empty() || self.ov.as_ref().is_some_and(|o| o.local_node.name == node);
        // DevTools endpoints (`/json/...`) sit at the root of a node.
        let root = if path.starts_with("/json") {
            ""
        } else {
            "/api"
        };
        if local {
            format!("{root}{path}")
        } else {
            format!("/api/peers/{}{path}", urlencode(node))
        }
    }

    /// Fetch the overview in the background.
    fn refresh(&mut self) {
        if self.refreshing {
            self.refresh_again = true;
            return;
        }
        self.refreshing = true;
        self.last_refresh = Some(Instant::now());
        let req = self.request(reqwest::Method::GET, "/api/mesh/overview");
        let base = self.base.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Overview(fetch_overview(req, &base).await));
        });
    }

    fn refresh_browsers(&mut self) {
        let Some(ov) = &self.ov else { return };
        if self.browsers_pending
            || self
                .browsers_refresh
                .is_some_and(|last| last.elapsed() < REFRESH)
        {
            return;
        }
        self.browsers_pending = true;
        self.browsers_refresh = Some(Instant::now());
        let requests: Vec<_> = std::iter::once(ov.local_node.name.as_str())
            .chain(
                ov.peers
                    .iter()
                    .filter(|p| p.status == "online")
                    .map(|p| p.name.as_str()),
            )
            .map(|daemon| {
                (
                    daemon.to_string(),
                    self.request(Method::GET, &self.api(daemon, "/extension/nodes")),
                )
            })
            .collect();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::extension::NodeInfo>,
            }
            let mut browsers: Vec<_> = futures_util::future::join_all(requests.into_iter().map(
                |(daemon, req)| async move {
                    let response = req.send().await.ok()?.error_for_status().ok()?;
                    let nodes = response.json::<Nodes>().await.ok()?.nodes;
                    Some(nodes.into_iter().map(move |node| BrowserTarget {
                        daemon: daemon.clone(),
                        node,
                    }))
                },
            ))
            .await
            .into_iter()
            .flatten()
            .flatten()
            .collect();
            browsers.sort_by(|a, b| {
                (&a.node.name, &a.daemon, a.node.id).cmp(&(&b.node.name, &b.daemon, b.node.id))
            });
            let _ = tx.send(Msg::Browsers(browsers));
        });
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Pushed(ov) => {
                self.set_overview(ov);
                if self.refresh_error {
                    self.refresh_error = false;
                    self.set_status("", false);
                }
            }
            Msg::Browsers(browsers) => {
                let sessions = self.selected_key(Tab::Sessions);
                let nodes = self.selected_key(Tab::Nodes);
                self.browsers = browsers;
                self.browsers_pending = false;
                self.reselect(Tab::Sessions, sessions);
                self.reselect(Tab::Nodes, nodes);
                self.clamp_selection();
            }
            Msg::BrowserTab(result) => {
                self.pending = self.pending.saturating_sub(1);
                match result {
                    Ok(url) => self.open(&url),
                    Err(error) => self.set_status(error, true),
                }
            }
            Msg::Live(live) => self.live = live,
            Msg::Shot(id, shot) => {
                if let (Some(img), Some(picker)) = (shot, self.picker.as_ref()) {
                    self.shots.insert(id, picker.new_resize_protocol(img));
                }
            }
            Msg::Icon(key, icon) => {
                if let (Some(img), Some(picker)) = (icon, self.picker.as_ref()) {
                    self.icons.insert(key, picker.new_resize_protocol(img));
                }
            }
            Msg::Overview(res) => {
                self.refreshing = false;
                match res {
                    Ok(ov) => {
                        self.set_overview(ov);
                        // Only the refresh's own error goes away with it; an action's stays.
                        if self.refresh_error {
                            self.refresh_error = false;
                            self.set_status("", false);
                        } else if self.status == "Refreshing…" {
                            self.set_status("Refreshed", false);
                        }
                    }
                    Err(e) => self.refresh_failed(e),
                }
                self.clamp_selection();
                if std::mem::take(&mut self.refresh_again) {
                    self.refresh();
                }
            }
            Msg::Done {
                label,
                result,
                refresh,
            } => {
                self.pending = self.pending.saturating_sub(1);
                match result {
                    Ok(text) if text.is_empty() => self.set_status(format!("{label}: done"), false),
                    Ok(text) => self.set_status(text, false),
                    Err(e) => self.set_status(format!("{label}: {e}"), true),
                }
                if refresh {
                    self.refresh();
                }
            }
            Msg::Backups(p, res) => {
                self.pending = self.pending.saturating_sub(1);
                match res {
                    Ok(list) => {
                        self.set_status("", false);
                        self.show_data_menu(p, list);
                    }
                    Err(e) => self.set_status(format!("Backups: {e}"), true),
                }
            }
            Msg::PlayHere(file) => {
                self.pending = self.pending.saturating_sub(1);
                self.set_status("", false);
                self.play_here = Some(file);
            }
        }
    }

    fn set_overview(&mut self, ov: MeshOverview) {
        // The selection stays on its item when rows come and go.
        let keep: Vec<Option<String>> = TABS.iter().map(|t| self.selected_key(*t)).collect();
        self.ov = Some(ov);
        for (t, key) in TABS.iter().zip(keep) {
            self.reselect(*t, key);
        }
        self.clamp_selection();
    }

    fn refresh_failed(&mut self, msg: String) {
        self.refresh_error = true;
        self.set_status(msg, true);
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = msg.into();
        self.error = error;
    }

    fn keep(&self, node: &str) -> bool {
        self.filter.as_deref().is_none_or(|f| f == node)
    }

    fn sessions(&self) -> Vec<&MeshSession> {
        self.ov
            .iter()
            .flat_map(|o| o.sessions.iter())
            .filter(|s| self.keep(&s.node_name))
            .collect()
    }

    fn browser_sessions(&self) -> impl Iterator<Item = &BrowserTarget> {
        self.browsers
            .iter()
            .filter(|b| b.node.online && self.keep(&b.key()))
    }

    fn session_count(&self) -> usize {
        self.sessions().len() + self.browser_sessions().count()
    }

    fn browser_at_selection(&mut self) -> Option<BrowserTarget> {
        let i = self.selected_index()?;
        match self.tab {
            Tab::Sessions => self
                .browser_sessions()
                .nth(i.checked_sub(self.sessions().len())?)
                .cloned(),
            Tab::Nodes => self
                .browsers
                .get(i.checked_sub(1 + self.ov.as_ref()?.peers.len())?)
                .cloned(),
            _ => None,
        }
    }

    fn browser_view(&self, browser: &BrowserTarget) -> String {
        let daemon = if browser.daemon == self.local_node() {
            String::new()
        } else {
            format!("&daemon={}", urlencode(&browser.daemon))
        };
        format!("{}/handoff/?browser={}{daemon}", self.base, browser.node.id)
    }

    fn browser_menu(&self, browser: BrowserTarget) -> Action {
        let mut items = vec![item("Details", details(&browser.node.name, &browser.node))];
        if browser.node.online {
            items.insert(
                0,
                item("Open browser", Action::Open(self.browser_view(&browser))),
            );
            items.insert(
                1,
                item(
                    "Open a tab",
                    Action::Form(FormKind::Session, Some(browser.key())),
                ),
            );
        }
        items.push(item(
            "Filter by browser",
            Action::Filter(Some(browser.key())),
        ));
        Action::Menu {
            title: browser.node.name,
            items,
        }
    }

    fn profiles(&self) -> Vec<&MeshProfile> {
        self.ov
            .iter()
            .flat_map(|o| o.profiles.iter())
            .filter(|p| self.keep(&p.node_name))
            .collect()
    }

    fn recordings(&self) -> Vec<&MeshRecording> {
        self.ov
            .iter()
            .flat_map(|o| o.recordings.iter())
            .filter(|r| self.keep(&r.node_name))
            .collect()
    }

    fn rows(&self) -> usize {
        let Some(ref ov) = self.ov else { return 0 };
        match self.tab {
            Tab::Sessions => self.session_count(),
            Tab::Profiles => self.profiles().len(),
            Tab::Recordings => self.recordings().len(),
            Tab::Nodes => 1 + ov.peers.len() + self.browsers.len(),
            Tab::Engines => self.engine_rows().len(),
        }
    }

    /// What identifies each row of a tab: node and id for sessions, profiles and
    /// recordings; none for the others (they keep the row number).
    fn row_keys(&self, tab: Tab) -> Option<Vec<String>> {
        Some(match tab {
            Tab::Sessions => self
                .sessions()
                .iter()
                .map(|s| format!("{}/{}", s.node_name, s.session.id))
                .chain(self.browser_sessions().map(BrowserTarget::key))
                .collect(),
            Tab::Profiles => self
                .profiles()
                .iter()
                .map(|p| {
                    let id = p.profile.id.map(|i| i.to_string());
                    format!(
                        "{}/{}",
                        p.node_name,
                        id.unwrap_or_else(|| p.profile.name.clone())
                    )
                })
                .collect(),
            Tab::Recordings => self
                .recordings()
                .iter()
                .map(|r| format!("{}/{}", r.node_name, r.recording.id))
                .collect(),
            Tab::Nodes => self.ov.as_ref().map(|ov| {
                std::iter::once(ov.local_node.name.clone())
                    .chain(ov.peers.iter().map(|p| p.name.clone()))
                    .chain(self.browsers.iter().map(BrowserTarget::key))
                    .collect()
            })?,
            Tab::Engines => return None,
        })
    }

    fn selected_key(&self, tab: Tab) -> Option<String> {
        let i = TABS.iter().position(|t| *t == tab)?;
        let row = self.tables[i].selected()?;
        self.row_keys(tab)?.into_iter().nth(row)
    }

    fn reselect(&mut self, tab: Tab, key: Option<String>) {
        let Some(key) = key else { return };
        let Some(row) = self
            .row_keys(tab)
            .and_then(|k| k.iter().position(|x| *x == key))
        else {
            return;
        };
        if let Some(i) = TABS.iter().position(|t| *t == tab) {
            self.tables[i].select(Some(row));
        }
    }

    fn table(&mut self) -> &mut TableState {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        &mut self.tables[i]
    }

    fn clamp_selection(&mut self) {
        for (i, tab) in TABS.iter().enumerate() {
            let saved = self.tab;
            self.tab = *tab;
            let n = self.rows();
            self.tab = saved;
            let t = &mut self.tables[i];
            match (n, t.selected()) {
                (0, _) => t.select(None),
                (_, None) => t.select(Some(0)),
                (n, Some(s)) if s >= n => t.select(Some(n - 1)),
                _ => {}
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.rows();
        if n == 0 {
            return;
        }
        let cur = self.table().selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, n as isize - 1) as usize;
        self.table().select(Some(next));
    }

    fn selected_index(&mut self) -> Option<usize> {
        self.table().selected()
    }

    fn session(&mut self) -> Option<MeshSession> {
        let i = self.selected_index()?;
        self.sessions().get(i).map(|s| (*s).clone())
    }

    fn profile(&mut self) -> Option<MeshProfile> {
        let i = self.selected_index()?;
        self.profiles().get(i).map(|p| (*p).clone())
    }

    fn recording(&mut self) -> Option<MeshRecording> {
        let i = self.selected_index()?;
        self.recordings().get(i).map(|r| (*r).clone())
    }

    fn proxy_name(&self, node: &str, id: &str) -> String {
        self.node_proxies(node)
            .into_iter()
            .find(|p| p.id == id)
            .map_or_else(|| id.to_string(), |p| p.name)
    }

    fn node_proxies(&self, node: &str) -> Vec<crate::proxies::ProxyInfo> {
        let Some(ref ov) = self.ov else {
            return Vec::new();
        };
        if ov.local_node.name == node {
            return ov.local_node.proxies.clone();
        }
        ov.peers
            .iter()
            .find(|p| p.name == node)
            .map(|p| p.proxies.clone())
            .unwrap_or_default()
    }

    /// Handle a key; false quits.
    fn key(&mut self, k: KeyEvent) -> bool {
        if self.modal.is_some() {
            self.modal_key(k);
            return true;
        }
        if self.help {
            self.help = false;
            return true;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.cycle_tab(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='5') => self.tab = TABS[c as usize - '1' as usize],
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.table().select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => {
                let n = self.rows();
                self.table().select(n.checked_sub(1));
            }
            KeyCode::Char('r') => {
                self.refresh();
                self.set_status("Refreshing…", false);
            }
            KeyCode::Char('f') => self.toggle_filter(),
            KeyCode::Char('m') | KeyCode::Char(' ') => {
                if let Some(a) = self.item_menu() {
                    self.run(a);
                }
            }
            KeyCode::Char('M') => {
                let a = self.tab_menu();
                self.run(a);
            }
            _ => self.tab_key(k),
        }
        true
    }

    fn mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        if self.help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return;
        }
        if let Some(modal) = self.modal.as_ref() {
            let area = self.popup.unwrap_or_default();
            let inside = area.contains(pos);
            let list = matches!(modal, Modal::Menu { .. } | Modal::Text { .. });
            match m.kind {
                MouseEventKind::ScrollDown if list => self.modal_key(key(KeyCode::Down)),
                MouseEventKind::ScrollUp if list => self.modal_key(key(KeyCode::Up)),
                MouseEventKind::Down(MouseButton::Left) if !inside => {
                    if matches!(
                        modal,
                        Modal::Menu { .. } | Modal::Text { .. } | Modal::Confirm { .. }
                    ) {
                        self.modal_key(key(KeyCode::Esc));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(Modal::Menu { items, sel, .. }) = self.modal.as_mut() {
                        // The list is windowed around the selection when it is long.
                        let max = (self.screen.height.saturating_sub(6) as usize).max(3);
                        let start = if items.len() > max {
                            sel.saturating_sub(max / 2).min(items.len() - max)
                        } else {
                            0
                        };
                        let row = (m.row as usize).checked_sub(area.y as usize + 1);
                        if let Some(i) = row.map(|r| r + start).filter(|i| *i < items.len()) {
                            *sel = i;
                            self.modal_key(key(KeyCode::Enter));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if m.row == 0 => {
                if let Some(i) = self
                    .tab_spans
                    .iter()
                    .position(|(a, b)| (*a..*b).contains(&m.column))
                {
                    self.tab = TABS[i];
                }
            }
            MouseEventKind::ScrollDown if self.body.contains(pos) => self.move_by(1),
            MouseEventKind::ScrollUp if self.body.contains(pos) => self.move_by(-1),
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right))
                if self.body.contains(pos) =>
            {
                // Under the top border and the header.
                let Some(row) = (m.row as usize).checked_sub(self.body.y as usize + 2) else {
                    return;
                };
                let i = self.table().offset() + row;
                if i >= self.rows() {
                    return;
                }
                self.table().select(Some(i));
                let double = self.last_click.is_some_and(|(at, x, y)| {
                    at.elapsed() < Duration::from_millis(400) && (x, y) == (m.column, m.row)
                });
                self.last_click = Some((Instant::now(), m.column, m.row));
                if button == MouseButton::Right || double {
                    self.last_click = None;
                    if let Some(a) = self.item_menu() {
                        self.run(a);
                    }
                }
            }
            _ => {}
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.tab = TABS[(i + delta).rem_euclid(TABS.len() as isize) as usize];
    }

    /// The node of the selected row.
    fn selected_node(&mut self) -> Option<String> {
        if let Some(browser) = self.browser_at_selection() {
            return Some(browser.key());
        }
        match self.tab {
            Tab::Sessions => self.session().map(|s| s.node_name),
            Tab::Profiles => self.profile().map(|p| p.node_name),
            Tab::Recordings => self.recording().map(|r| r.node_name),
            Tab::Nodes => self.node_at_selection(),
            Tab::Engines => None,
        }
    }

    fn toggle_filter(&mut self) {
        if let Some(node) = self.filter.take() {
            self.set_status(format!("Showing every node (was {node} only)"), false);
        } else if let Some(node) = self.selected_node() {
            self.set_status(format!("Showing {node} only (f clears)"), false);
            self.filter = Some(node);
        }
        self.clamp_selection();
    }

    fn local_node(&self) -> String {
        self.ov
            .as_ref()
            .map(|o| o.local_node.name.clone())
            .unwrap_or_default()
    }

    fn node_names(&self) -> Vec<String> {
        self.ov
            .iter()
            .flat_map(|o| {
                std::iter::once(o.local_node.name.clone())
                    .chain(o.peers.iter().map(|p| p.name.clone()))
            })
            .collect()
    }

    /// Hotkeys of the selected tab.
    fn tab_key(&mut self, k: KeyEvent) {
        if let Some(browser) = self.browser_at_selection() {
            let action = match k.code {
                KeyCode::Enter | KeyCode::Char('o' | 'n') if browser.node.online => {
                    Some(Action::Open(self.browser_view(&browser)))
                }
                KeyCode::Char('N' | 't') if browser.node.online => {
                    Some(Action::Form(FormKind::Session, Some(browser.key())))
                }
                KeyCode::Char('i') => Some(details(&browser.node.name, &browser.node)),
                _ => None,
            };
            if let Some(action) = action {
                self.run(action);
                return;
            }
            if matches!(k.code, KeyCode::Char('n' | 'N' | 't')) {
                return;
            }
        }
        let action = match (self.tab, k.code) {
            (Tab::Sessions, KeyCode::Enter | KeyCode::Char('o')) => self
                .session()
                .map(|s| Action::Open(handoff_url(&s.node_url, s.session.id))),
            (Tab::Sessions, KeyCode::Char('s') | KeyCode::Char('x') | KeyCode::Delete) => {
                self.session().map(|s| stop_session(&s))
            }
            (Tab::Sessions, KeyCode::Char('R')) => self.session().map(|s| record_toggle(&s)),
            (Tab::Sessions, KeyCode::Char('p')) => {
                let s = self.session();
                s.and_then(|s| self.cycle_session_proxy(&s))
            }
            (Tab::Sessions, KeyCode::Char('w')) => self.session().map(|s| Action::Say(ws_url(&s))),
            (Tab::Sessions, KeyCode::Char('t')) => self.session().map(|s| new_tab(&s)),
            (Tab::Sessions, KeyCode::Char('e')) => self.session().map(|s| rename_session(&s)),
            (Tab::Sessions, KeyCode::Char('T')) => self.session().map(|s| idle_timeout(&s)),
            (Tab::Sessions, KeyCode::Char('N')) => {
                let node = self.selected_node();
                Some(Action::Form(FormKind::Session, node))
            }
            (Tab::Sessions, KeyCode::Char('X')) => Some(self.stop_all_action()),
            (Tab::Sessions | Tab::Nodes, KeyCode::Char('n')) => {
                let node = self.selected_node().unwrap_or_else(|| self.local_node());
                Some(Action::Api(
                    Call::new(
                        format!("Starting a throwaway session on {node}"),
                        Method::POST,
                        &node,
                        "/sessions",
                    )
                    .body(json!({ "persistent": true })),
                ))
            }
            (Tab::Profiles, KeyCode::Enter | KeyCode::Char('o')) => {
                self.profile().map(|p| launch_profile(&p))
            }
            (Tab::Profiles, KeyCode::Char('p')) => {
                let p = self.profile();
                p.and_then(|p| self.cycle_profile_proxy(&p))
            }
            (Tab::Profiles, KeyCode::Char('d') | KeyCode::Delete) => {
                self.profile().map(|p| delete_profile(&p))
            }
            (Tab::Profiles, KeyCode::Char('e')) => self.profile().map(|p| rename_profile(&p)),
            (Tab::Profiles, KeyCode::Char('b')) => self.profile().map(Action::DataMenu),
            (Tab::Profiles, KeyCode::Char('N')) => {
                let node = self.selected_node();
                Some(Action::Form(FormKind::Profile, node))
            }
            (Tab::Recordings, KeyCode::Char('d') | KeyCode::Delete) => {
                self.recording().map(|r| delete_recording(&r))
            }
            (Tab::Recordings, KeyCode::Enter | KeyCode::Char('p')) => {
                let here = inline_player();
                self.recording()
                    .map(|r| Action::Play(r.node_name.clone(), r.recording.id.to_string(), here))
            }
            (Tab::Recordings, KeyCode::Char('P')) => {
                let here = !inline_player();
                self.recording()
                    .map(|r| Action::Play(r.node_name.clone(), r.recording.id.to_string(), here))
            }
            (Tab::Recordings, KeyCode::Char('o')) => {
                self.recording().map(|r| Action::Open(play_url(&r)))
            }
            (Tab::Recordings, KeyCode::Char('K')) => self.recording().map(|r| keep_recording(&r)),
            (Tab::Recordings, KeyCode::Char('E')) => self.recording().map(|r| export_recording(&r)),
            (Tab::Recordings, KeyCode::Char('v')) => {
                let node = self.selected_node().unwrap_or_else(|| self.local_node());
                Some(vacuum(&node))
            }
            (Tab::Engines, KeyCode::Char('a')) => {
                let row = self.engine_row();
                row.map(|r| {
                    let next = match r.defaults.adblock {
                        None => Some(true),
                        Some(true) => Some(false),
                        Some(false) => None,
                    };
                    self.engine_patch(&r, &format!("Ad blocker: {}", tri_label(next)), |d| {
                        d.adblock = next
                    })
                })
            }
            (Tab::Engines, KeyCode::Char('c')) => {
                let row = self.engine_row();
                row.map(|r| {
                    let next = match r.defaults.color_scheme {
                        None => Some(ColorScheme::Dark),
                        Some(ColorScheme::Dark) => Some(ColorScheme::Light),
                        Some(ColorScheme::Light) => None,
                    };
                    self.engine_patch(&r, &format!("Appearance: {}", scheme_label(next)), |d| {
                        d.color_scheme = next
                    })
                })
            }
            (Tab::Engines, KeyCode::Char('t')) => {
                let row = self.engine_row();
                row.map(|r| {
                    let next = !r.defaults.show_touches;
                    self.engine_patch(
                        &r,
                        &format!("Show touches: {}", if next { "on" } else { "off" }),
                        |d| d.show_touches = next,
                    )
                })
            }
            (Tab::Nodes, KeyCode::Char('a')) => Some(add_peer()),
            (Tab::Nodes, KeyCode::Char('x')) => {
                let local = self.local_node();
                self.node_at_selection()
                    .filter(|n| *n != local)
                    .map(|n| remove_peer(&n))
            }
            (_, KeyCode::Char('i')) => self.details_action(),
            _ => None,
        };
        if let Some(a) = action {
            self.run(a);
        }
    }

    fn details_action(&mut self) -> Option<Action> {
        match self.tab {
            Tab::Sessions => {
                let s = self.session()?;
                Some(details_with_shot(
                    &s.session.name,
                    &s,
                    Some(s.session.id.to_string()),
                ))
            }
            Tab::Profiles => {
                let p = self.profile()?;
                Some(details(&p.profile.name, &p))
            }
            Tab::Recordings => {
                let r = self.recording()?;
                Some(details("Recording", &r))
            }
            Tab::Nodes => {
                let n = self.node_at_selection()?;
                let ov = self.ov.as_ref()?;
                if n == ov.local_node.name {
                    Some(details(&n, &ov.local_node))
                } else {
                    let p = ov.peers.iter().find(|p| p.name == n)?;
                    Some(details(&n, p))
                }
            }
            Tab::Engines => {
                let r = self.engine_row()?;
                Some(details(&format!("{} defaults", r.engine), &r.defaults))
            }
        }
    }

    /// One row per engine some online node supports, with the defaults of this node.
    fn engine_rows(&self) -> Vec<EngineRow> {
        let Some(ov) = self.ov.as_ref() else {
            return Vec::new();
        };
        let mut rows: Vec<EngineRow> = Vec::new();
        let peers = ov.peers.iter().filter(|p| p.status == "online");
        let supported = std::iter::once(&ov.local_node.supported_engines)
            .chain(peers.map(|p| &p.supported_engines));
        for engine in supported.flatten().copied() {
            if rows.iter().any(|r| r.engine == engine) {
                continue;
            }
            rows.push(EngineRow {
                engine,
                defaults: ov.local_node.settings.engines.of(engine).clone(),
            });
        }
        rows
    }

    fn engine_row(&mut self) -> Option<EngineRow> {
        let i = self.selected_index()?;
        self.engine_rows().into_iter().nth(i)
    }

    fn node_settings(&self, node: &str) -> Option<NodeSettings> {
        let ov = self.ov.as_ref()?;
        if ov.local_node.name == node {
            return Some(ov.local_node.settings.clone());
        }
        ov.peers.iter().find(|p| p.name == node)?.settings.clone()
    }

    /// Change one engine's defaults on every online node: a node takes the engines as a whole.
    fn engine_patch(
        &self,
        row: &EngineRow,
        label: &str,
        change: impl Fn(&mut EngineDefaults),
    ) -> Action {
        let Some(ov) = self.ov.as_ref() else {
            return Action::Say("Not connected".into());
        };
        let peers = ov.peers.iter().filter(|p| p.status == "online");
        let nodes = std::iter::once((&ov.local_node.name, Some(ov.local_node.settings.clone())))
            .chain(peers.map(|p| (&p.name, p.settings.clone())));
        let calls: Vec<Call> = nodes
            .filter_map(|(node, settings)| {
                let mut engines = settings?.engines;
                change(match row.engine {
                    BrowserEngine::Chromium => &mut engines.chromium,
                    BrowserEngine::Firefox => &mut engines.firefox,
                });
                Some(Call::new(label, Method::PATCH, node, "/settings").body(
                    json!({ "engines": serde_json::to_value(&engines).unwrap_or(Value::Null) }),
                ))
            })
            .collect();
        if calls.is_empty() {
            return Action::Say("No node's settings are available".into());
        }
        Action::ApiAll(format!("{} ({})", label, row.engine), calls)
    }

    fn engine_menu(&self, row: &EngineRow) -> Action {
        let d = &row.defaults;
        let mark = |on: bool| if on { "✓ " } else { "  " };
        let adblock = [None, Some(true), Some(false)].map(|v| {
            item(
                format!("{}{}", mark(d.adblock == v), tri_label(v)),
                self.engine_patch(row, &format!("Ad blocker: {}", tri_label(v)), |e| {
                    e.adblock = v
                }),
            )
        });
        let scheme = [None, Some(ColorScheme::Dark), Some(ColorScheme::Light)].map(|v| {
            item(
                format!("{}{}", mark(d.color_scheme == v), scheme_label(v)),
                self.engine_patch(row, &format!("Appearance: {}", scheme_label(v)), |e| {
                    e.color_scheme = v
                }),
            )
        });
        Action::Menu {
            title: format!("{} defaults", row.engine),
            items: vec![
                item(
                    format!("Ad blocker: {}…", tri_label(d.adblock)),
                    Action::Menu {
                        title: "Ad blocker".into(),
                        items: adblock.to_vec(),
                    },
                ),
                item(
                    format!("Appearance: {}…", scheme_label(d.color_scheme)),
                    Action::Menu {
                        title: "Appearance".into(),
                        items: scheme.to_vec(),
                    },
                ),
                item(
                    format!(
                        "Show touches: {}",
                        if d.show_touches { "on" } else { "off" }
                    ),
                    self.engine_patch(
                        row,
                        &format!(
                            "Show touches: {}",
                            if d.show_touches { "off" } else { "on" }
                        ),
                        |e| e.show_touches = !d.show_touches,
                    ),
                ),
                item("Details", details(&format!("{} defaults", row.engine), d)),
            ],
        }
    }

    fn proxy_items(
        &self,
        node: &str,
        current: Option<&str>,
        make: impl Fn(Option<&str>) -> Action,
    ) -> Vec<(String, Action)> {
        let mark = |on: bool| if on { "✓ " } else { "  " };
        let mut items = vec![item(
            format!("{}Direct", mark(current.is_none())),
            make(None),
        )];
        for p in self.node_proxies(node) {
            items.push(item(
                format!("{}{}", mark(current == Some(p.id.as_str())), p.name),
                make(Some(&p.id)),
            ));
        }
        items
    }

    fn cycle_session_proxy(&self, s: &MeshSession) -> Option<Action> {
        let proxies = self.node_proxies(&s.node_name);
        if proxies.is_empty() {
            return Some(Action::Say(format!(
                "{} has no proxies configured",
                s.node_name
            )));
        }
        let choices: Vec<Option<String>> = std::iter::once(None)
            .chain(proxies.into_iter().map(|x| Some(x.id)))
            .collect();
        let cur = choices
            .iter()
            .position(|c| *c == s.session.proxy)
            .unwrap_or(0);
        let next = choices[(cur + 1) % choices.len()].clone();
        Some(set_session_proxy(self, s, next.as_deref()))
    }

    fn cycle_profile_proxy(&self, p: &MeshProfile) -> Option<Action> {
        let proxies = self.node_proxies(&p.node_name);
        if proxies.is_empty() {
            return Some(Action::Say(format!(
                "{} has no proxies configured",
                p.node_name
            )));
        }
        let choices: Vec<Option<String>> = std::iter::once(None)
            .chain(proxies.into_iter().map(|x| Some(x.id)))
            .collect();
        let cur = choices
            .iter()
            .position(|c| *c == p.profile.proxy)
            .unwrap_or(0);
        let next = choices[(cur + 1) % choices.len()].clone();
        Some(set_profile_proxy(self, p, next.as_deref()))
    }

    fn stop_all_action(&self) -> Action {
        let targets: Vec<(String, String)> = self
            .sessions()
            .iter()
            .map(|s| (s.node_name.clone(), s.session.id.to_string()))
            .collect();
        let n = targets.len();
        let scope = self
            .filter
            .as_ref()
            .map(|f| format!(" on {f}"))
            .unwrap_or_default();
        confirm_then(
            format!("Stop {n} session{}{scope}?", if n == 1 { "" } else { "s" }),
            Action::StopAll(targets),
        )
    }

    // ----------------------------------------------------------------- menus

    fn item_menu(&mut self) -> Option<Action> {
        if let Some(browser) = self.browser_at_selection() {
            return Some(self.browser_menu(browser));
        }
        match self.tab {
            Tab::Sessions => {
                let s = self.session()?;
                Some(self.session_menu(&s))
            }
            Tab::Profiles => {
                let p = self.profile()?;
                Some(self.profile_menu(&p))
            }
            Tab::Recordings => {
                let r = self.recording()?;
                Some(recording_menu(&r))
            }
            Tab::Nodes => {
                let n = self.node_at_selection()?;
                Some(self.node_menu(&n))
            }
            Tab::Engines => {
                let r = self.engine_row()?;
                Some(self.engine_menu(&r))
            }
        }
    }

    fn tab_menu(&self) -> Action {
        let (title, items) = match self.tab {
            Tab::Sessions => (
                "Sessions",
                vec![
                    item(
                        "New session…",
                        Action::Form(FormKind::Session, self.filter.clone()),
                    ),
                    item("Stop all sessions…", self.stop_all_action()),
                ],
            ),
            Tab::Profiles => (
                "Profiles",
                vec![item(
                    "New profile…",
                    Action::Form(FormKind::Profile, self.filter.clone()),
                )],
            ),
            Tab::Recordings => (
                "Recordings",
                vec![item(
                    "Apply the retention limits now",
                    vacuum(&self.filter.clone().unwrap_or_else(|| self.local_node())),
                )],
            ),
            Tab::Nodes => ("Nodes", vec![item("Add a peer…", add_peer())]),
            Tab::Engines => (
                "Engines",
                vec![item(
                    "Defaults apply to new sessions of the engine: pick a row, then m",
                    Action::Say("Select a row, then press m".into()),
                )],
            ),
        };
        Action::Menu {
            title: title.into(),
            items,
        }
    }

    fn session_menu(&self, s: &MeshSession) -> Action {
        let x = &s.session;
        let mut items = vec![
            item("Take over", Action::Open(handoff_url(&s.node_url, x.id))),
            item("New tab…", new_tab(s)),
            item("Rename…", rename_session(s)),
            item(
                format!(
                    "Auto-stop when idle: {}…",
                    idle_label(x.idle_timeout_seconds)
                ),
                idle_timeout(s),
            ),
            item(
                "Proxy (switches now)…",
                Action::Menu {
                    title: "Proxy".into(),
                    items: self.proxy_items(&s.node_name, x.proxy.as_deref(), |p| {
                        set_session_proxy(self, s, p)
                    }),
                },
            ),
        ];
        if x.is_recording {
            let (verb, label) = if x.recording_paused {
                ("resume-record", "Resume recording")
            } else {
                ("pause-record", "Pause recording")
            };
            items.push(item(label, session_call(s, label, verb)));
            items.push(item(
                "Stop recording",
                session_call(s, "Stopping recording", "stop-record"),
            ));
        } else {
            items.push(item(
                "Start recording",
                session_call(s, "Starting recording", "record"),
            ));
        }
        items.push(item("WebSocket URL", Action::Say(ws_url(s))));
        if x.profile.is_none() {
            items.push(item(
                "Save as profile…",
                ask(
                    "Save as a profile",
                    "Name of the new profile (the browser restarts on the same pages)",
                    "",
                    Action::Api(
                        Call::new(
                            "Saving the profile",
                            Method::POST,
                            &s.node_name,
                            format!("/sessions/{}/save-as-profile", x.id),
                        )
                        .long()
                        .body(json!({ "name": INPUT })),
                    ),
                ),
            ));
        }
        items.push(item("Details", details(&x.name, s)));
        items.push(item("Stop session", stop_session(s)));
        Action::Menu {
            title: x.name.clone(),
            items,
        }
    }

    fn profile_menu(&self, p: &MeshProfile) -> Action {
        let x = &p.profile;
        let base = profile_base(p);
        let patch = |label: &str, body: Value| {
            Action::Api(Call::new(label, Method::PATCH, &p.node_name, base.clone()).body(body))
        };
        let others: Vec<String> = self
            .node_names()
            .into_iter()
            .filter(|n| *n != p.node_name)
            .collect();
        let transfer = |delete_source: bool| {
            let verb = if delete_source { "Move" } else { "Copy" };
            Action::Menu {
                title: format!("{verb} {} to…", x.name),
                items: others
                    .iter()
                    .map(|n| {
                        item(
                            n.clone(),
                            confirm_then(
                                format!("{verb} {} from {} to {n}?", x.name, p.node_name),
                                Action::Api(
                                    Call::new(
                                        format!("{verb} {} to {n}", x.name),
                                        Method::POST,
                                        &p.node_name,
                                        format!("{base}/transfer"),
                                    )
                                    .long()
                                    .body(json!({
                                        "target_node": n,
                                        "delete_source": delete_source,
                                        "overwrite": false,
                                    })),
                                ),
                            ),
                        )
                    })
                    .collect(),
            }
        };
        let mut items = vec![item(
            if x.session_id.is_some() {
                "Take over"
            } else {
                "Launch"
            },
            launch_profile(p),
        )];
        if x.session_id.is_none() && !x.tabs.is_empty() {
            items.push(item(
                "Launch without the default tabs",
                launch_profile_blank(p),
            ));
        }
        items.push(item("Details", details(&x.name, p)));
        items.push(item("Rename…", rename_profile(p)));
        items.push(item(
            "Proxy for new sessions…",
            Action::Menu {
                title: "Proxy".into(),
                items: self.proxy_items(&p.node_name, x.proxy.as_deref(), |pr| {
                    set_profile_proxy(self, p, pr)
                }),
            },
        ));
        items.push(item(
            "Appearance…",
            Action::Menu {
                title: "Appearance".into(),
                items: vec![
                    item(
                        "Dark",
                        patch("Appearance: dark", json!({ "color_scheme": "dark" })),
                    ),
                    item(
                        "Light",
                        patch("Appearance: light", json!({ "color_scheme": "light" })),
                    ),
                ],
            },
        ));
        items.push(item(
            format!("Default tabs ({})…", x.tabs.len()),
            ask(
                "Default tabs",
                "Addresses its sessions open with, separated by spaces (empty: none)",
                &x.tabs.join(" "),
                patch("Default tabs set", json!({ "tabs": INPUT })),
            ),
        ));
        items.push(item(
            "Duplicate…",
            ask(
                "Duplicate profile",
                "Name of the copy",
                "",
                Action::Api(
                    Call::new(
                        "Duplicating",
                        Method::POST,
                        &p.node_name,
                        format!("{base}/clone"),
                    )
                    .body(json!({ "target_name": INPUT })),
                ),
            ),
        ));
        if !others.is_empty() {
            items.push(item("Copy to another node…", transfer(false)));
            items.push(item("Move to another node…", transfer(true)));
        }
        items.push(item("Backups & data…", Action::DataMenu(p.clone())));
        items.push(item("Delete", delete_profile(p)));
        Action::Menu {
            title: x.name.clone(),
            items,
        }
    }

    fn node_menu(&self, name: &str) -> Action {
        let is_local = name == self.local_node();
        let only = self.filter.as_deref() == Some(name);
        let mut items = vec![
            item(
                if only {
                    "Show every node".to_string()
                } else {
                    format!("Show only {name}")
                },
                Action::Filter(if only { None } else { Some(name.to_string()) }),
            ),
            item(
                "New session here…",
                Action::Form(FormKind::Session, Some(name.to_string())),
            ),
            item(
                "New profile here…",
                Action::Form(FormKind::Profile, Some(name.to_string())),
            ),
        ];
        if let Some(ov) = &self.ov {
            let settings = if is_local {
                serde_json::to_value(&ov.local_node.settings).ok()
            } else {
                ov.peers
                    .iter()
                    .find(|p| p.name == name)
                    .and_then(|p| serde_json::to_value(&p.settings).ok())
            };
            if let Some(v) = settings {
                items.push(item("Settings", details(&format!("{name} settings"), &v)));
            }
        }
        if !self.node_proxies(name).is_empty() {
            let current = self.node_settings(name).and_then(|s| s.default_proxy);
            items.push(item(
                "Default proxy…",
                Action::Menu {
                    title: format!("Default proxy of {name}"),
                    items: self.proxy_items(name, current.as_deref(), |p| {
                        Action::Api(
                            Call::new(
                                format!("{name}'s default proxy"),
                                Method::PATCH,
                                name,
                                "/settings",
                            )
                            .body(json!({ "default_proxy": p.unwrap_or("") })),
                        )
                    }),
                },
            ));
        }
        items.push(item("Apply recording retention now", vacuum(name)));
        if !is_local {
            items.push(item("Remove peer", remove_peer(name)));
        }
        Action::Menu {
            title: name.to_string(),
            items,
        }
    }

    fn node_at_selection(&mut self) -> Option<String> {
        let i = self.selected_index()?;
        let ov = self.ov.as_ref()?;
        if i == 0 {
            Some(ov.local_node.name.clone())
        } else {
            ov.peers.get(i - 1).map(|p| p.name.clone())
        }
    }

    // ------------------------------------------------------------ screenshots

    /// Ask for the selected session's screenshot when it is missing or a while old.
    fn fetch_shot(&mut self) {
        if self.picker.is_none()
            || self.tab != Tab::Sessions
            || self
                .modal
                .as_ref()
                .is_some_and(|m| !matches!(m, Modal::Text { .. }))
        {
            return;
        }
        let Some(s) = self.session() else { return };
        let id = s.session.id.to_string();
        if self
            .shot_asked
            .get(&id)
            .is_some_and(|t| t.elapsed() < SHOT_EVERY)
        {
            return;
        }
        self.shot_asked.insert(id.clone(), Instant::now());
        let req = self.request(
            Method::GET,
            &self.api(&s.node_name, &format!("/sessions/{id}/screenshot")),
        );
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let jpeg = match req.send().await {
                Ok(r) if r.status() == reqwest::StatusCode::OK => r.bytes().await.ok(),
                _ => None,
            };
            let shot = match jpeg {
                Some(b) => tokio::task::spawn_blocking(move || image::load_from_memory(&b).ok())
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let _ = tx.send(Msg::Shot(id, shot));
        });
    }

    /// Fetch the pictures of the profiles (once each, and again when one changes).
    fn fetch_icons(&mut self) {
        if self.picker.is_none() {
            return;
        }
        let Some(ov) = self.ov.as_ref() else { return };
        let wanted: Vec<(String, String, String)> = ov
            .profiles
            .iter()
            .filter_map(|p| {
                let key = icon_key(p)?;
                (!self.icon_asked.contains(&key))
                    .then(|| (key, p.node_name.clone(), profile_base(p)))
            })
            .collect();
        for (key, node, base) in wanted {
            self.icon_asked.insert(key.clone());
            let req = self.request(Method::GET, &self.api(&node, &format!("{base}/icon")));
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let bytes = match req.send().await {
                    Ok(r) if r.status() == reqwest::StatusCode::OK => r.bytes().await.ok(),
                    _ => None,
                };
                // Raster pictures only; an SVG stays as the initials in the browser.
                let icon = match bytes {
                    Some(b) => {
                        tokio::task::spawn_blocking(move || image::load_from_memory(&b).ok())
                            .await
                            .ok()
                            .flatten()
                    }
                    None => None,
                };
                let _ = tx.send(Msg::Icon(key, icon));
            });
        }
    }

    // --------------------------------------------------------------- running

    fn run(&mut self, action: Action) {
        match action {
            Action::Api(call) => self.call(call),
            Action::ApiAll(label, calls) => self.call_all(label, calls),
            Action::Open(url) => self.open(&url),
            Action::OpenBrowserTab(browser, url) => self.open_browser_tab(browser, &url),
            Action::Play(node, id, here) => self.play(&node, &id, here),
            Action::Say(text) => self.set_status(text, false),
            Action::Menu { title, items } => {
                self.modal = Some(Modal::Menu {
                    title,
                    items,
                    sel: 0,
                });
            }
            Action::Input {
                title,
                prompt,
                initial,
                then,
            } => {
                self.modal = Some(Modal::Input {
                    title,
                    prompt,
                    buf: initial,
                    then,
                });
            }
            Action::Confirm { question, then } => {
                self.modal = Some(Modal::Confirm { question, then });
            }
            Action::Text { title, lines, shot } => {
                self.modal = Some(Modal::Text {
                    title,
                    lines,
                    scroll: 0,
                    shot,
                });
            }
            Action::Form(kind, node) => self.open_form(kind, node),
            Action::StopAll(targets) => self.stop_all(targets),
            Action::Filter(node) => {
                self.filter = node;
                self.clamp_selection();
            }
            Action::DataMenu(p) => self.data_menu(p),
        }
    }

    fn open_browser_tab(&mut self, browser: BrowserTarget, url: &str) {
        let Some(tab) = browser.node.opener() else {
            self.set_status("The browser owner must allow opening shared tabs", true);
            return;
        };
        let view = self.browser_view(&browser);
        let path = self.api(
            &browser.daemon,
            &format!("/extension/nodes/{}/command", browser.node.id),
        );
        let req = self
            .request(Method::POST, &path)
            .json(&json!({ "tab": tab, "action": "open-tab", "params": { "url": url } }));
        let tx = self.tx.clone();
        self.pending += 1;
        self.set_status(format!("Opening a tab in {}…", browser.node.name), false);
        tokio::spawn(async move {
            let result = async {
                let response = req.send().await.map_err(|e| e.to_string())?;
                let status = response.status();
                let reply: Value = response.json().await.map_err(|e| e.to_string())?;
                reply["result"]["tab"]
                    .as_u64()
                    .filter(|_| status.is_success())
                    .map(|tab| format!("{view}&page={tab}"))
                    .ok_or_else(|| {
                        reply["error"]
                            .as_str()
                            .unwrap_or("The browser did not open a tab")
                            .to_string()
                    })
            }
            .await;
            let _ = tx.send(Msg::BrowserTab(result));
        });
    }

    fn call(&mut self, call: Call) {
        let url = self.api(&call.node, &call.path);
        let mut req = self.request(call.method.clone(), &url);
        if call.long {
            req = req.timeout(Duration::from_secs(3600));
        }
        if let Some(ref body) = call.body {
            req = req.json(body);
        }
        self.set_status(format!("{}…", call.label), false);
        self.pending += 1;
        let tx = self.tx.clone();
        let refresh = call.save_to.is_none();
        tokio::spawn(async move {
            let result = run_call(req, call.upload, call.save_to).await;
            let _ = tx.send(Msg::Done {
                label: call.label,
                result,
                refresh,
            });
        });
    }

    /// Fetch a recording as a video file and play it: here, in the terminal (mpv
    /// drawing into it; the event loop hands the terminal over), or in a window of
    /// `$BROWSER_DAEMON_PLAYER` (mpv by default; a command with arguments is fine).
    /// Either way the player gets a file: the daemon's secret stays out of its
    /// command line.
    fn play(&mut self, node: &str, id: &str, here: bool) {
        if here {
            let url = self.api(node, &format!("/recordings/{id}/export.webm"));
            let req = self
                .request(Method::GET, &url)
                .timeout(Duration::from_secs(3600));
            self.set_status("Fetching the recording…", false);
            self.pending += 1;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let msg = match download_video(req).await {
                    Ok(file) => Msg::PlayHere(file),
                    Err(e) => Msg::Done {
                        label: "Fetching the recording".into(),
                        result: Err(e),
                        refresh: false,
                    },
                };
                let _ = tx.send(msg);
            });
            return;
        }
        let player = std::env::var("BROWSER_DAEMON_PLAYER").unwrap_or_else(|_| "mpv".into());
        let mut words = player.split_whitespace().map(String::from);
        let Some(program) = words.next() else {
            self.set_status("BROWSER_DAEMON_PLAYER is empty", true);
            return;
        };
        let args: Vec<String> = words.collect();
        let url = self.api(node, &format!("/recordings/{id}/export.webm"));
        let req = self
            .request(Method::GET, &url)
            .timeout(Duration::from_secs(3600));
        let label = "Fetching the recording".to_string();
        self.set_status(format!("{label}…"), false);
        self.pending += 1;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let done = |result| Msg::Done {
                label: label.clone(),
                result,
                refresh: false,
            };
            let file = match download_video(req).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(done(Err(e)));
                    return;
                }
            };
            let child = std::process::Command::new(&program)
                .args(&args)
                .arg(file.path())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => {
                    let _ = tx.send(done(Ok(format!("Playing in {program}"))));
                    // The file goes away once the player does.
                    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
                }
                Err(e) => {
                    let _ = tx.send(done(Err(format!("{program}: {e}"))));
                }
            }
            drop(file);
        });
    }

    fn call_all(&mut self, label: String, calls: Vec<Call>) {
        self.set_status(format!("{label}…"), false);
        self.pending += 1;
        let reqs: Vec<_> = calls
            .iter()
            .map(|c| {
                let url = self.api(&c.node, &c.path);
                let req = self.request(c.method.clone(), &url);
                match c.body {
                    Some(ref body) => req.json(body),
                    None => req,
                }
            })
            .collect();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let results = futures_util::future::join_all(reqs.into_iter().map(|r| r.send())).await;
            let failed = results
                .iter()
                .filter(|r| !matches!(r, Ok(res) if res.status().is_success()))
                .count();
            let result = if failed == 0 {
                Ok(String::new())
            } else {
                Err(format!("{failed} of {} nodes failed", results.len()))
            };
            let _ = tx.send(Msg::Done {
                label,
                result,
                refresh: true,
            });
        });
    }

    fn stop_all(&mut self, targets: Vec<(String, String)>) {
        if targets.is_empty() {
            self.set_status("No sessions", false);
            return;
        }
        let label = format!("Stopping {} sessions", targets.len());
        self.set_status(format!("{label}…"), false);
        self.pending += 1;
        let reqs: Vec<_> = targets
            .iter()
            .map(|(n, id)| {
                let url = self.api(n, &format!("/sessions/{id}"));
                self.request(Method::DELETE, &url)
            })
            .collect();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let results = futures_util::future::join_all(reqs.into_iter().map(|r| r.send())).await;
            let failed = results
                .iter()
                .filter(|r| !matches!(r, Ok(res) if res.status().is_success()))
                .count();
            let result = if failed == 0 {
                Ok(format!("Stopped {} sessions", results.len()))
            } else {
                Err(format!(
                    "{failed} of {} could not be stopped",
                    results.len()
                ))
            };
            let _ = tx.send(Msg::Done {
                label,
                result,
                refresh: true,
            });
        });
    }

    /// A profile's backups, with what can be done to its data (fetched in the background).
    fn data_menu(&mut self, p: MeshProfile) {
        let base = profile_base(&p);
        let url = self.api(&p.node_name, &format!("{base}/backups"));
        let req = self.request(Method::GET, &url);
        self.set_status("Loading backups…", false);
        self.pending += 1;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = match req.send().await {
                Ok(r) if r.status().is_success() => Ok(r.json().await.unwrap_or_default()),
                Ok(r) => Err(format!("HTTP {}", r.status())),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(Msg::Backups(p, res));
        });
    }

    fn show_data_menu(&mut self, p: MeshProfile, list: Vec<Value>) {
        let base = profile_base(&p);
        let node = p.node_name.clone();
        let mut items = vec![item(
            "Back up now…",
            ask(
                "Back up",
                "Label (optional)",
                "",
                Action::Api(
                    Call::new("Backing up", Method::POST, &node, format!("{base}/backups"))
                        .body(json!({ "label": INPUT })),
                ),
            ),
        )];
        for b in &list {
            let id = b["id"].as_str().unwrap_or_default().to_string();
            let label = format!(
                "{id}  {}  {}  {}{}",
                ago(b["created_at_secs"].as_u64()),
                bytes(b["size_bytes"].as_u64().unwrap_or(0)),
                if b["automatic"].as_bool().unwrap_or(false) {
                    "auto"
                } else {
                    "manual"
                },
                b["label"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .map(|l| format!("  {l}"))
                    .unwrap_or_default(),
            );
            let bpath = format!("{base}/backups/{}", urlencode(&id));
            items.push(item(
                label,
                Action::Menu {
                    title: format!("Backup {id}"),
                    items: vec![
                        item(
                            "Restore (keeps a safety backup of the current data)",
                            confirm_then(
                                format!("Replace the data of {} with backup {id}?", p.profile.name),
                                Action::Api(
                                    Call::new(
                                        "Restoring",
                                        Method::POST,
                                        &node,
                                        format!("{bpath}/restore"),
                                    )
                                    .long(),
                                ),
                            ),
                        ),
                        item(
                            "Delete backup",
                            Action::Api(Call::new("Deleting backup", Method::DELETE, &node, bpath)),
                        ),
                    ],
                },
            ));
        }
        let force = if p.profile.is_active {
            "&force=true"
        } else {
            ""
        };
        items.push(item(
            "Clear data…",
            confirm_then(
                format!(
                    "Wipe cookies and logins of {}? A safety backup is kept.",
                    p.profile.name
                ),
                Action::Api(Call::new(
                    "Clearing",
                    Method::POST,
                    &node,
                    format!("{base}/clear?backup=true{force}"),
                )),
            ),
        ));
        items.push(item(
            "Export archive…",
            ask(
                "Export",
                "Save as",
                &format!("{}.tar.gz", p.profile.name),
                Action::Api(
                    Call::new("Exporting", Method::GET, &node, format!("{base}/export"))
                        .save_to(INPUT),
                ),
            ),
        ));
        items.push(item(
            "Import archive…",
            confirm_then(
                format!("Replace the data of {} with an archive?", p.profile.name),
                ask(
                    "Import",
                    "Archive file",
                    "",
                    Action::Api(
                        Call::new(
                            "Importing",
                            Method::POST,
                            &node,
                            format!(
                                "/profiles/{}/import?overwrite=true",
                                urlencode(&p.profile.name)
                            ),
                        )
                        .upload(INPUT),
                    ),
                ),
            ),
        ));
        self.modal = Some(Modal::Menu {
            title: format!("{} — backups & data", p.profile.name),
            items,
            sel: 0,
        });
    }

    // ----------------------------------------------------------------- forms

    fn node_choice(&self, kind: FormKind, preset: Option<&str>) -> Field {
        let mut nodes: Vec<_> = self
            .node_names()
            .into_iter()
            .map(|n| (n.clone(), n))
            .collect();
        if matches!(kind, FormKind::Session) {
            nodes.extend(self.browsers.iter().filter(|b| b.node.online).map(|b| {
                (
                    b.key(),
                    format!("{} (extension via {})", b.node.name, b.daemon),
                )
            }));
        }
        let chosen = preset
            .or(self.filter.as_deref())
            .and_then(|n| nodes.iter().position(|(key, _)| key == n))
            .unwrap_or(0);
        let mut f = Field::choice("Node", nodes);
        if let FieldKind::Choice { sel, .. } = &mut f.kind {
            *sel = chosen;
        }
        f
    }

    fn proxy_field(&self, node: &str) -> Field {
        let mut options = vec![
            (String::new(), "(default)".to_string()),
            ("none".to_string(), "direct".to_string()),
        ];
        options.extend(self.node_proxies(node).into_iter().map(|p| (p.id, p.name)));
        Field::choice("Proxy", options)
    }

    fn open_form(&mut self, kind: FormKind, node: Option<String>) {
        if self.ov.is_none() {
            return;
        }
        let node_field = self.node_choice(kind, node.as_deref());
        let node = node_field.value();
        let engines = || {
            Field::choice(
                "Engine",
                vec![
                    ("chromium".into(), "Chromium".into()),
                    ("firefox".into(), "Firefox".into()),
                ],
            )
        };
        let scheme = || {
            Field::choice(
                "Appearance",
                vec![
                    (String::new(), "(default)".into()),
                    ("dark".into(), "dark".into()),
                    ("light".into(), "light".into()),
                ],
            )
        };
        let tri = |label: &'static str| {
            Field::choice(
                label,
                vec![
                    (String::new(), "(default)".into()),
                    ("true".into(), "on".into()),
                    ("false".into(), "off".into()),
                ],
            )
        };
        let fields = match kind {
            FormKind::Session => {
                let mut profiles = vec![(String::new(), "(throwaway)".to_string())];
                profiles.extend(self.profiles().into_iter().map(|p| {
                    (
                        format!("{}\u{1f}{}", p.node_name, profile_key(p)),
                        format!("{} ({})", p.profile.name, p.node_name),
                    )
                }));
                vec![
                    node_field,
                    engines(),
                    Field::choice("Profile", profiles),
                    Field::text("Name"),
                    self.proxy_field(&node),
                    scheme(),
                    tri("Record"),
                    tri("Ad blocker"),
                    Field::text("Idle (min)"),
                    Field::choice(
                        "Default tabs",
                        vec![
                            ("true".into(), "open the profile's".into()),
                            ("false".into(), "don't".into()),
                        ],
                    ),
                    Field::text("URL or search"),
                ]
            }
            FormKind::Profile => vec![
                node_field,
                Field::text("Name"),
                engines(),
                self.proxy_field(&node),
                scheme(),
                Field::text("Default tabs"),
            ],
        };
        self.modal = Some(Modal::Form(Form {
            kind,
            fields,
            sel: 0,
        }));
    }

    /// The node changed: its proxies are the ones to choose from.
    fn form_sync(&self, f: &mut Form) {
        let proxy_at = match f.kind {
            FormKind::Session => 4,
            FormKind::Profile => 3,
        };
        let node = f.fields[0].value();
        f.fields[proxy_at] = self.proxy_field(&node);
    }

    fn browser_form(f: &Form) -> bool {
        matches!(f.kind, FormKind::Session) && f.fields[0].value().starts_with("browser:")
    }

    fn form_indices(f: &Form) -> Vec<usize> {
        if Self::browser_form(f) {
            vec![0, SESSION_TARGET_FIELD]
        } else if matches!(f.kind, FormKind::Session) {
            (0..SESSION_TARGET_FIELD).collect()
        } else {
            (0..f.fields.len()).collect()
        }
    }

    fn form_action(&self, f: &Form) -> Action {
        let v = |i: usize| f.fields[i].value();
        let flag = |s: String| match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        match f.kind {
            FormKind::Session => {
                let mut node = v(0);
                if Self::browser_form(f) {
                    let Some(browser) = self
                        .browsers
                        .iter()
                        .find(|b| b.node.online && b.key() == node)
                    else {
                        return Action::Say("The browser is offline".into());
                    };
                    return match crate::extension::tab_target(&v(SESSION_TARGET_FIELD)) {
                        Some(url) => Action::OpenBrowserTab(browser.clone(), url),
                        None => Action::Open(self.browser_view(browser)),
                    };
                }

                let mut body = json!({ "persistent": true, "engine": v(1) });
                if let Some((n, key)) = v(2).split_once('\u{1f}') {
                    node = n.to_string();
                    body["profile"] = json!(key);
                    // The profile has its own engine.
                    if let Some(p) = self
                        .profiles()
                        .into_iter()
                        .find(|p| p.node_name == n && profile_key(p) == key)
                    {
                        body["engine"] = json!(p.profile.engine);
                    }
                }
                if !v(3).is_empty() {
                    body["name"] = json!(v(3));
                }
                if !v(4).is_empty() {
                    body["proxy"] = json!(v(4));
                }
                if !v(5).is_empty() {
                    body["color_scheme"] = json!(v(5));
                }
                if let Some(b) = flag(v(6)) {
                    body["record"] = json!(b);
                }
                if let Some(b) = flag(v(7)) {
                    body["adblock"] = json!(b);
                }
                if let Ok(m) = v(8).parse::<u64>() {
                    body["idle_timeout_minutes"] = json!(m);
                }
                if v(9) == "false" {
                    body["default_tabs"] = json!(false);
                }
                Action::Api(
                    Call::new(
                        format!("Starting a session on {node}"),
                        Method::POST,
                        &node,
                        "/sessions",
                    )
                    .body(body),
                )
            }
            FormKind::Profile => {
                let node = v(0);
                if v(1).is_empty() {
                    return Action::Say("A profile needs a name".into());
                }
                let mut body = json!({ "name": v(1), "engine": v(2) });
                if !v(3).is_empty() {
                    body["proxy"] = json!(v(3));
                }
                if !v(4).is_empty() {
                    body["color_scheme"] = json!(v(4));
                }
                if !v(5).is_empty() {
                    body["tabs"] = json!(v(5));
                }
                Action::Api(
                    Call::new(
                        format!("Creating {} on {node}", v(1)),
                        Method::POST,
                        &node,
                        "/profiles",
                    )
                    .body(body),
                )
            }
        }
    }

    // ----------------------------------------------------------------- modals

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match modal {
            Modal::Menu {
                title,
                items,
                mut sel,
            } => {
                let last = items.len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => return,
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::PageDown => sel = (sel + 8).min(last),
                    KeyCode::PageUp => sel = sel.saturating_sub(8),
                    KeyCode::Home | KeyCode::Char('g') => sel = 0,
                    KeyCode::End | KeyCode::Char('G') => sel = last,
                    KeyCode::Enter => {
                        if let Some((_, a)) = items.get(sel) {
                            let a = a.clone();
                            self.run(a);
                            return;
                        }
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        if let Some((_, a)) = items.get(c as usize - '1' as usize) {
                            let a = a.clone();
                            self.run(a);
                            return;
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Menu { title, items, sel });
            }
            Modal::Input {
                title,
                prompt,
                mut buf,
                then,
            } => {
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let a = then.with_input(buf.trim());
                        self.run(a);
                        return;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.modal = Some(Modal::Input {
                    title,
                    prompt,
                    buf,
                    then,
                });
            }
            Modal::Confirm { then, .. } => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.run(*then);
                } else {
                    self.set_status("Cancelled", false);
                }
            }
            Modal::Text {
                title,
                lines,
                mut scroll,
                shot,
            } => {
                let last = lines.len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 10).min(last),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => scroll = last,
                    _ => {}
                }
                self.modal = Some(Modal::Text {
                    title,
                    lines,
                    scroll,
                    shot,
                });
            }
            Modal::Form(mut f) => {
                let fields = Self::form_indices(&f);
                let n = fields.len();
                let pos = fields.iter().position(|i| *i == f.sel).unwrap_or(0);
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let a = self.form_action(&f);
                        self.run(a);
                        return;
                    }
                    KeyCode::Up | KeyCode::BackTab => f.sel = fields[(pos + n - 1) % n],
                    KeyCode::Down | KeyCode::Tab => f.sel = fields[(pos + 1) % n],
                    code => {
                        let at = f.sel;
                        let mut node_changed = false;
                        match &mut f.fields[at].kind {
                            FieldKind::Choice { options, sel } => {
                                let len = options.len().max(1);
                                match code {
                                    KeyCode::Right | KeyCode::Char(' ') | KeyCode::Char('l') => {
                                        *sel = (*sel + 1) % len;
                                        node_changed = at == 0;
                                    }
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        *sel = (*sel + len - 1) % len;
                                        node_changed = at == 0;
                                    }
                                    _ => {}
                                }
                            }
                            FieldKind::Text(buf) => match code {
                                KeyCode::Backspace => {
                                    buf.pop();
                                }
                                KeyCode::Char('u') if ctrl => buf.clear(),
                                KeyCode::Char(c) if !ctrl => buf.push(c),
                                _ => {}
                            },
                        }
                        if node_changed {
                            self.form_sync(&mut f);
                        }
                    }
                }
                self.modal = Some(Modal::Form(f));
            }
        }
    }

    fn open(&mut self, url: &str) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reap it when it ends (the opener may stay for as long as the browser does).
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                self.set_status(format!("Opened {url}"), false);
            }
            Err(_) => self.set_status(url.to_string(), false),
        }
    }
}

// ------------------------------------------------------- actions of one row

fn handoff_url(node_url: &str, session: impl std::fmt::Display) -> String {
    format!(
        "{}/handoff/?session={session}",
        node_url.trim_end_matches('/')
    )
}

fn play_url(r: &MeshRecording) -> String {
    format!(
        "{}/?play={}",
        r.node_url.trim_end_matches('/'),
        r.recording.id
    )
}

fn ws_url(s: &MeshSession) -> String {
    let engine = if s.session.engine == crate::BrowserEngine::Firefox {
        "firefox"
    } else {
        "chromium"
    };
    format!(
        "{}/{engine}?session_id={}",
        s.node_url.trim_end_matches('/').replacen("http", "ws", 1),
        s.session.id
    )
}

fn idle_label(secs: u64) -> String {
    if secs == 0 {
        "never".into()
    } else {
        dur(secs)
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

fn session_call(s: &MeshSession, label: &str, verb: &str) -> Action {
    Action::Api(Call::new(
        label,
        Method::POST,
        &s.node_name,
        format!("/sessions/{}/{verb}", s.session.id),
    ))
}

fn stop_session(s: &MeshSession) -> Action {
    confirm_then(
        format!("Stop {} on {}?", s.session.name, s.node_name),
        Action::Api(Call::new(
            "Stopping session",
            Method::DELETE,
            &s.node_name,
            format!("/sessions/{}", s.session.id),
        )),
    )
}

fn record_toggle(s: &MeshSession) -> Action {
    if s.session.is_recording {
        session_call(s, "Stopping recording", "stop-record")
    } else {
        session_call(s, "Starting recording", "record")
    }
}

fn new_tab(s: &MeshSession) -> Action {
    ask(
        "New tab",
        "URL",
        "https://",
        Action::Api(Call::new(
            "Opening tab",
            Method::PUT,
            &s.node_name,
            format!("/json/new?url={INPUT}&session_id={}", s.session.id),
        )),
    )
}

fn rename_session(s: &MeshSession) -> Action {
    ask(
        "Rename session",
        "Name (empty: a generated one)",
        &s.session.name,
        Action::Api(
            Call::new(
                "Renaming",
                Method::POST,
                &s.node_name,
                format!("/sessions/{}/name", s.session.id),
            )
            .body(json!({ "name": INPUT })),
        ),
    )
}

fn idle_timeout(s: &MeshSession) -> Action {
    ask(
        "Auto-stop",
        "Minutes of inactivity before it stops (0: never)",
        &(s.session.idle_timeout_seconds / 60).to_string(),
        Action::Api(Call::new(
            "Auto-stop set",
            Method::POST,
            &s.node_name,
            format!("/sessions/{}/idle-timeout?minutes={INPUT}", s.session.id),
        )),
    )
}

fn set_session_proxy(app: &App, s: &MeshSession, proxy: Option<&str>) -> Action {
    let label = match proxy {
        Some(id) => format!(
            "Session now goes through {}",
            app.proxy_name(&s.node_name, id)
        ),
        None => "Session now connects directly".to_string(),
    };
    Action::Api(
        Call::new(
            label,
            Method::POST,
            &s.node_name,
            format!("/sessions/{}/proxy", s.session.id),
        )
        .body(json!({ "proxy": proxy.unwrap_or("none") })),
    )
}

/// Identifies a profile's picture (a new one has a new revision).
fn icon_key(p: &MeshProfile) -> Option<String> {
    Some(format!(
        "{}/{}/{}",
        p.node_name,
        profile_key(p),
        p.profile.icon_rev?
    ))
}

/// Draw the pictures of the rows in view, over the blank first column.
fn draw_row_icons(f: &mut Frame, app: &mut App, body: Rect, state: usize, keys: &[Option<String>]) {
    let offset = app.tables[state].offset();
    // The block's top border and the header come first.
    let first = body.y + 2;
    for (n, key) in keys.iter().enumerate().skip(offset) {
        let y = first + (n - offset) as u16;
        if y >= body.y + body.height {
            break;
        }
        if let Some(icon) = key.as_ref().and_then(|k| app.icons.get_mut(k)) {
            let area = Rect {
                x: body.x,
                y,
                width: 2,
                height: 1,
            };
            f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Fit(None)),
                area,
                icon,
            );
        }
    }
}

fn profile_key(p: &MeshProfile) -> String {
    p.profile
        .id
        .map_or_else(|| p.profile.name.clone(), |id| id.to_string())
}

fn profile_base(p: &MeshProfile) -> String {
    format!("/profiles/{}", urlencode(&profile_key(p)))
}

fn set_profile_proxy(app: &App, p: &MeshProfile, proxy: Option<&str>) -> Action {
    let label = match proxy {
        Some(id) => format!(
            "{} now uses {}",
            p.profile.name,
            app.proxy_name(&p.node_name, id)
        ),
        None => format!("{} now connects directly", p.profile.name),
    };
    Action::Api(
        Call::new(label, Method::PATCH, &p.node_name, profile_base(p))
            .body(json!({ "proxy": proxy.unwrap_or("none") })),
    )
}

fn launch_profile(p: &MeshProfile) -> Action {
    match p.profile.session_id {
        Some(sid) => Action::Open(handoff_url(&p.node_url, sid)),
        None => Action::Api(
            Call::new(
                format!("Launching {}", p.profile.name),
                Method::POST,
                &p.node_name,
                "/sessions",
            )
            .body(json!({
                "profile": profile_key(p),
                "engine": p.profile.engine,
                "persistent": true,
            })),
        ),
    }
}

fn launch_profile_blank(p: &MeshProfile) -> Action {
    Action::Api(
        Call::new(
            format!("Launching {} without its tabs", p.profile.name),
            Method::POST,
            &p.node_name,
            "/sessions",
        )
        .body(json!({
            "profile": profile_key(p),
            "engine": p.profile.engine,
            "persistent": true,
            "default_tabs": false,
        })),
    )
}

fn rename_profile(p: &MeshProfile) -> Action {
    ask(
        "Rename profile",
        "New name",
        &p.profile.name,
        Action::Api(
            Call::new("Renaming", Method::PATCH, &p.node_name, profile_base(p))
                .body(json!({ "name": INPUT })),
        ),
    )
}

fn delete_profile(p: &MeshProfile) -> Action {
    let force = if p.profile.is_active {
        "?force=true"
    } else {
        ""
    };
    confirm_then(
        format!(
            "Delete profile {} on {}? Cookies and logins are wiped.",
            p.profile.name, p.node_name
        ),
        Action::Api(Call::new(
            format!("Deleting {}", p.profile.name),
            Method::DELETE,
            &p.node_name,
            format!("{}{force}", profile_base(p)),
        )),
    )
}

fn recording_menu(r: &MeshRecording) -> Action {
    let title = r
        .recording
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Recording".into());
    Action::Menu {
        title,
        items: vec![
            item(
                "Play here, in the terminal (mpv)",
                Action::Play(r.node_name.clone(), r.recording.id.to_string(), true),
            ),
            item(
                "Play in a player window",
                Action::Play(r.node_name.clone(), r.recording.id.to_string(), false),
            ),
            item("Play in the browser", Action::Open(play_url(r))),
            item(
                if r.recording.keep {
                    "Unfavorite (the cleanup may remove it)"
                } else {
                    "Favorite (the cleanup never removes it)"
                },
                keep_recording(r),
            ),
            item("Details", details("Recording", r)),
            item("Export frames…", export_recording(r)),
            item("Apply retention now", vacuum(&r.node_name)),
            item("Delete", delete_recording(r)),
        ],
    }
}

/// Favorite a recording (the cleanup never removes it), or unfavorite it.
fn keep_recording(r: &MeshRecording) -> Action {
    let keep = !r.recording.keep;
    Action::Api(
        Call::new(
            if keep {
                "Adding to favorites"
            } else {
                "Removing from favorites"
            },
            Method::POST,
            &r.node_name,
            format!("/recordings/{}/keep", r.recording.id),
        )
        .body(json!({ "keep": keep })),
    )
}

fn export_recording(r: &MeshRecording) -> Action {
    ask(
        "Export frames",
        "Save as (JSON lines)",
        &format!("{}.jsonl", r.recording.id),
        Action::Api(
            Call::new(
                "Exporting",
                Method::GET,
                &r.node_name,
                format!("/recordings/{}/frames", r.recording.id),
            )
            .save_to(INPUT),
        ),
    )
}

fn delete_recording(r: &MeshRecording) -> Action {
    confirm_then(
        format!(
            "Delete recording {}?{}",
            r.recording.title.as_deref().unwrap_or("(untitled)"),
            if r.recording.keep {
                " It is a favorite."
            } else {
                ""
            }
        ),
        Action::Api(Call::new(
            "Deleting recording",
            Method::DELETE,
            &r.node_name,
            format!("/recordings/{}", r.recording.id),
        )),
    )
}

fn vacuum(node: &str) -> Action {
    Action::Api(Call::new(
        format!("Applying the retention limits on {node}"),
        Method::POST,
        node,
        "/recordings/vacuum",
    ))
}

fn add_peer() -> Action {
    ask(
        "Add a peer",
        "Base URL of the node",
        "https://",
        Action::Api(
            // Peers are added on the asked node itself.
            Call::new("Adding peer", Method::POST, "", "/peers").body(json!({ "url": INPUT })),
        ),
    )
}

fn remove_peer(node: &str) -> Action {
    confirm_then(
        format!("Remove peer {node}? Its sessions keep running."),
        Action::Api(Call::new(
            format!("Removing {node}"),
            Method::DELETE,
            "",
            format!("/peers/{}", urlencode(node)),
        )),
    )
}

/// Pretty JSON of `value`, line by line, in a scrollable popup.
fn details(title: &str, value: &impl serde::Serialize) -> Action {
    details_with_shot(title, value, None)
}

fn details_with_shot(title: &str, value: &impl serde::Serialize, shot: Option<String>) -> Action {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    Action::Text {
        title: title.to_string(),
        lines: text.lines().map(str::to_string).collect(),
        shot,
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn dur(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

fn ago(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return "–".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(secs) {
        d if d < 45 => "just now".into(),
        d => format!("{} ago", dur(d).split(' ').next().unwrap_or_default()),
    }
}

fn header(cells: &[&str]) -> Row<'static> {
    Row::new(cells.iter().map(|c| Cell::from(c.to_uppercase())))
        .style(
            Style::new()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Rgb(125, 133, 190)),
        )
        .bottom_margin(0)
}

mod draw;
use draw::draw;

/// What to authenticate with: the cluster secret from `file` (an explicit file
/// that cannot be read is an error), else from the NixOS module's file when
/// readable, else the `bd_auth` token the dashboard at `base` hands to whoever
/// can open it. `None`: the daemon wants nothing.
pub(crate) async fn find_secret(
    http: &reqwest::Client,
    base: &str,
    file: Option<&PathBuf>,
) -> Result<Option<String>, String> {
    // The NixOS module exports the file in the environment of every user, most of
    // whom cannot read it: only a file named on the command line must be readable.
    let from_env = |p: &PathBuf| {
        std::env::var_os("BROWSER_DAEMON_SECRET_FILE")
            .is_some_and(|e| std::path::Path::new(&e) == p.as_path())
    };
    if let Some(path) = file {
        match std::fs::read_to_string(path) {
            Ok(secret) => {
                let secret = secret.trim();
                if secret.is_empty() {
                    return Err(format!("The secret file {} is empty", path.display()));
                }
                return Ok(Some(secret.to_string()));
            }
            Err(e) if from_env(path) => {
                tracing::debug!("Secret file {} unreadable: {e}", path.display());
            }
            Err(e) => {
                return Err(format!(
                    "Cannot read the secret file {}: {e}",
                    path.display()
                ));
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(DEFAULT_SECRET_FILE) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(Some(s));
        }
    }
    // The page sets the sign-in cookie; the daemon takes its value as a token.
    let Ok(res) = http
        .get(format!("{base}/"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
    else {
        return Ok(None);
    };
    let cookies: Vec<&str> = res
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    Ok(auth_cookie_token(&cookies))
}

/// The `bd_auth` token among `Set-Cookie` values.
fn auth_cookie_token(set_cookies: &[&str]) -> Option<String> {
    set_cookies.iter().find_map(|c| {
        let (name, rest) = c.split_once('=')?;
        let value = rest.split(';').next()?.trim();
        (name.trim() == "bd_auth" && !value.is_empty()).then(|| value.to_string())
    })
}

pub async fn run(args: crate::ctl::Endpoint) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let base = args.url.trim_end_matches('/').to_string();
    let secret = find_secret(&http, &base, args.secret_file.as_ref()).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App {
        tx,
        pending: 0,
        refreshing: false,
        picker: None,
        play_here: None,
        shots: HashMap::new(),
        shot_asked: HashMap::new(),
        icons: HashMap::new(),
        icon_asked: Default::default(),
        tab_spans: Vec::new(),
        body: Rect::default(),
        popup: None,
        screen: Rect::default(),
        last_click: None,
        refresh_again: false,
        tick: 0,
        http,
        base,
        secret,
        ov: None,
        browsers: Vec::new(),
        browsers_pending: false,
        browsers_refresh: None,
        tab: Tab::Sessions,
        tables: Default::default(),
        status: String::new(),
        error: false,
        refresh_error: false,
        modal: None,
        filter: None,
        help: false,
        last_refresh: None,
        live: false,
    };
    app.refresh();
    tokio::spawn(push_overviews(
        app.base.clone(),
        app.secret.clone(),
        app.tx.clone(),
    ));

    let mut terminal = ratatui::init();
    // After raw mode is on: the terminal answers the query on stdin.
    app.picker = image_picker();
    // Late answers to the query would arrive as key presses (an `i` opens the details).
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_millis(400)
        && event::poll(Duration::from_millis(120)).unwrap_or(false)
    {
        let _ = event::read();
    }
    let mouse = mouse_wanted();
    if mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = event_loop(&mut terminal, &mut app, &mut rx);
    if mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

fn mouse_wanted() -> bool {
    !matches!(
        std::env::var("BROWSER_DAEMON_TUI_MOUSE").as_deref(),
        Ok("off" | "0" | "no")
    )
}

/// Play recordings in this terminal (the default) rather than in a player window;
/// `BROWSER_DAEMON_TUI_INLINE_PLAYER=off` turns it around (`P` plays the other way).
fn inline_player() -> bool {
    !matches!(
        std::env::var("BROWSER_DAEMON_TUI_INLINE_PLAYER").as_deref(),
        Ok("off" | "0" | "no" | "false")
    )
}

/// mpv's video output for this terminal: `$BROWSER_DAEMON_TUI_PLAYER_VO`, else kitty
/// graphics (not through tmux, which mpv cannot pass them through), sixel (tmux
/// draws sixel itself), else truecolor text.
fn mpv_vo(protocol: Option<ProtocolType>, tmux: bool) -> String {
    if let Ok(vo) = std::env::var("BROWSER_DAEMON_TUI_PLAYER_VO") {
        if !vo.trim().is_empty() {
            return vo.trim().to_owned();
        }
    }
    match protocol {
        Some(ProtocolType::Kitty) if !tmux => "kitty",
        Some(ProtocolType::Sixel) => "sixel",
        _ => "tct",
    }
    .to_owned()
}

/// Whether this mpv lists `vo` among its video outputs (`mpv --vo=help`).
fn mpv_has_vo(vo: &str) -> bool {
    std::process::Command::new("mpv")
        .arg("--vo=help")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split_whitespace().next() == Some(vo))
        })
        .unwrap_or(false)
}

/// Hand the terminal to mpv for a downloaded recording, then take it back.
fn play_here(terminal: &mut DefaultTerminal, app: &mut App, file: &tempfile::NamedTempFile) {
    let mut vo = mpv_vo(
        app.picker.as_ref().map(Picker::protocol_type),
        std::env::var_os("TMUX").is_some(),
    );
    // Not every mpv build has every output (sixel needs libsixel).
    if vo != "tct" && !mpv_has_vo(&vo) {
        vo = "tct".into();
    }
    let mouse = mouse_wanted();
    if mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    let status = std::process::Command::new("mpv")
        .arg(format!("--vo={vo}"))
        .args(["--really-quiet", "--keep-open=no", "--force-window=no"])
        .arg(file.path())
        .status();
    *terminal = ratatui::init();
    if mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    }
    let _ = terminal.clear();
    // Images drawn before are gone from the screen; draw them again.
    app.shots.clear();
    app.shot_asked.clear();
    app.icons.clear();
    app.icon_asked.clear();
    match status {
        Ok(s) if s.success() => app.set_status("", false),
        Ok(s) => app.set_status(format!("mpv ({vo}) exited with {s}"), true),
        Err(e) => app.set_status(
            format!("mpv: {e} (BROWSER_DAEMON_TUI_INLINE_PLAYER=off plays in a window)"),
            true,
        ),
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
        }
        if let Some(file) = app.play_here.take() {
            play_here(terminal, app, &file);
        }
        app.tick = app.tick.wrapping_add(1);
        app.refresh_browsers();
        app.fetch_shot();
        app.fetch_icons();
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press && !app.key(k) => return Ok(()),
                Event::Mouse(m) => app.mouse(m),
                _ => {}
            }
        } else if !app.live && app.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH) {
            app.refresh();
        }
    }
}

/// The overview's WebSocket URL for a daemon's base URL.
fn overview_ws_url(base: &str) -> String {
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/api/ws/overview")
}

/// Follow the overviews the daemon pushes, reconnecting with a growing pause; the
/// event loop polls while this is not connected.
async fn push_overviews(
    base: String,
    secret: Option<String>,
    tx: tokio::sync::mpsc::UnboundedSender<Msg>,
) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    let mut pause = Duration::from_secs(1);
    loop {
        let Ok(mut req) = overview_ws_url(&base).into_client_request() else {
            return;
        };
        if let Some(value) = secret
            .as_deref()
            .and_then(|s| reqwest::header::HeaderValue::from_str(s).ok())
        {
            req.headers_mut().insert("X-Browser-Cluster-Secret", value);
        }
        let connect = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(req),
        );
        if let Ok(Ok((mut ws, _))) = connect.await {
            pause = Duration::from_secs(1);
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(ov) = serde_json::from_str::<MeshOverview>(text.as_str()) else {
                    continue;
                };
                // Live only once an overview came through, not on the handshake.
                if tx.send(Msg::Live(true)).is_err() || tx.send(Msg::Pushed(ov)).is_err() {
                    return;
                }
            }
        }
        if tx.send(Msg::Live(false)).is_err() {
            return;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

/// The recording's video, downloaded to a temporary file.
async fn download_video(req: reqwest::RequestBuilder) -> Result<tempfile::NamedTempFile, String> {
    use futures_util::StreamExt;
    use std::io::Write;
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let mut file = tempfile::Builder::new()
        .prefix("browser-daemon-recording-")
        .suffix(".webm")
        .tempfile()
        .map_err(|e| e.to_string())?;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    Ok(file)
}

/// Fetch the overview; an error reads as the status line does.
async fn fetch_overview(req: reqwest::RequestBuilder, base: &str) -> Result<MeshOverview, String> {
    match req.send().await {
        Ok(r) if r.status().is_success() => r
            .json::<MeshOverview>()
            .await
            .map_err(|e| format!("Unexpected answer: {e}")),
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
            Err("Unauthorized: pass --secret-file (or set BROWSER_DAEMON_SECRET_FILE)".to_string())
        }
        Ok(r) => Err(format!("{base}: HTTP {}", r.status())),
        Err(e) => Err(format!(
            "No browser-daemon at {base} ({}): pass --url, set BROWSER_DAEMON_URL or write \
             it to /etc/browser-daemon/url",
            e.without_url()
        )),
    }
}

/// Send an action's request; the answer's own error message on failure. A body
/// to upload is read from `upload`, an answer to save is streamed to `save_to`.
async fn run_call(
    mut req: reqwest::RequestBuilder,
    upload: Option<String>,
    save_to: Option<String>,
) -> Result<String, String> {
    if let Some(file) = upload {
        match tokio::fs::read(expand_home(&file)).await {
            Ok(data) => req = req.body(data),
            Err(e) => return Err(format!("{file}: {e}")),
        }
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or_default();
        return Err(body["error"].as_str().map_or_else(
            || {
                if text.is_empty() {
                    status.to_string()
                } else {
                    text.clone()
                }
            },
            str::to_string,
        ));
    }
    if let Some(file) = save_to {
        let size = crate::ctl::save_stream(res, &expand_home(&file), false)
            .await
            .map_err(|e| format!("{file}: {e}"))?;
        return Ok(format!("Saved {file} ({})", bytes(size)));
    }
    Ok(String::new())
}

#[cfg(test)]
mod node_color_tests {
    use super::*;

    #[test]
    fn node_colours_match_the_web_ui() {
        assert_eq!(node_color("fnuc"), Color::Rgb(229, 210, 118));
        assert_eq!(node_color("rofl-13"), Color::Rgb(118, 223, 229));
        assert_eq!(node_color("ROFL-14"), Color::Rgb(229, 118, 190));
    }
}

#[cfg(test)]
mod secret_tests {
    #[test]
    fn the_overview_socket_follows_the_scheme() {
        assert_eq!(
            super::overview_ws_url("https://b.test"),
            "wss://b.test/api/ws/overview"
        );
        assert_eq!(
            super::overview_ws_url("http://127.0.0.1:3001"),
            "ws://127.0.0.1:3001/api/ws/overview"
        );
    }

    use super::*;

    #[test]
    fn the_token_comes_from_the_auth_cookie() {
        assert_eq!(
            auth_cookie_token(&["other=1; Path=/", "bd_auth=123.abcd; Max-Age=5; HttpOnly"]),
            Some("123.abcd".to_string())
        );
        assert_eq!(auth_cookie_token(&["bd_auth=; Max-Age=0"]), None);
        assert_eq!(auth_cookie_token(&["x_bd_auth=1"]), None);
        assert_eq!(auth_cookie_token(&[]), None);
    }

    #[tokio::test]
    async fn an_explicit_unreadable_secret_file_is_an_error() {
        let http = reqwest::Client::new();
        let missing = PathBuf::from("/nonexistent/browser-daemon-secret");
        let err = find_secret(&http, "http://127.0.0.1:1", Some(&missing)).await;
        assert!(err.unwrap_err().contains("Cannot read the secret file"));
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::write(&empty, " \n").unwrap();
        assert!(find_secret(&http, "http://127.0.0.1:1", Some(&empty))
            .await
            .is_err());
        let ok = dir.path().join("ok");
        std::fs::write(&ok, "s3cret\n").unwrap();
        assert_eq!(
            find_secret(&http, "http://127.0.0.1:1", Some(&ok)).await,
            Ok(Some("s3cret".to_string()))
        );
    }
}

#[cfg(test)]
mod tmux_image_tests {
    use super::*;

    #[test]
    fn tmux_viewer_is_the_latest_client_on_the_window() {
        let clients = "100\t@1\t12\t24\tkitty(0.50.0)\tRGB,sync\n\
                       200\t@1\t15\t30\t\tRGB,sixel,sync\n\
                       300\t@2\t9\t18\tkitty(0.50.0)\tRGB\n";
        let viewer = tmux_viewer(clients, "@1").unwrap();
        assert_eq!(viewer.cell, (15, 30));
        assert_eq!(tmux_protocol(&viewer), Some(ProtocolType::Sixel));
        assert!(tmux_viewer(clients, "@9").is_none());
    }

    #[test]
    fn tmux_protocol_follows_the_client_terminal() {
        let viewer = |termtype: &str, features: &str| TmuxViewer {
            termtype: termtype.into(),
            features: features.into(),
            cell: (10, 20),
        };
        assert_eq!(
            tmux_protocol(&viewer("kitty(0.50.0)", "RGB")),
            Some(ProtocolType::Kitty)
        );
        assert_eq!(
            tmux_protocol(&viewer("ghostty 1.2", "")),
            Some(ProtocolType::Kitty)
        );
        assert_eq!(
            tmux_protocol(&viewer("WezTerm 2024", "")),
            Some(ProtocolType::Iterm2)
        );
        assert_eq!(
            tmux_protocol(&viewer("", "RGB,sixel")),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(tmux_protocol(&viewer("", "RGB,sync")), None);
    }
}

#[cfg(test)]
mod player_tests {
    use super::*;

    #[test]
    fn mpv_draws_with_what_the_terminal_can_show() {
        assert_eq!(mpv_vo(Some(ProtocolType::Kitty), false), "kitty");
        assert_eq!(mpv_vo(Some(ProtocolType::Kitty), true), "tct");
        assert_eq!(mpv_vo(Some(ProtocolType::Sixel), true), "sixel");
        assert_eq!(mpv_vo(Some(ProtocolType::Iterm2), false), "tct");
        assert_eq!(mpv_vo(None, false), "tct");
    }
}
