//! `browser-daemon tui`: a terminal view of the mesh (sessions, profiles,
//! recordings, nodes) with the dashboard's actions, talking to the HTTP API.
//!
//! Everything the keys do is an [`Action`]: an API call, a menu, a prompt, a
//! confirmation or a form. The item menu (`m`) offers all of them for the selected row.

use std::path::PathBuf;

use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

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
}

/// An engine on a node, with the defaults its new sessions start with.
struct EngineRow {
    node: String,
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
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), subst(x, input))).collect()),
        other => other.clone(),
    }
}

#[derive(Clone)]
enum Action {
    Api(Call),
    Open(String),
    Say(String),
    Menu { title: String, items: Vec<(String, Action)> },
    /// Ask for a line of text, then do `then` with it in place of `$INPUT`.
    Input { title: String, prompt: String, initial: String, then: Box<Action> },
    Confirm { question: String, then: Box<Action> },
    Text { title: String, lines: Vec<String> },
    /// The form for a new session or profile; the node it starts on.
    Form(FormKind, Option<String>),
    StopAll(Option<String>),
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

fn item(label: impl Into<String>, action: Action) -> (String, Action) {
    (label.into(), action)
}

fn confirm_then(question: impl Into<String>, then: Action) -> Action {
    Action::Confirm { question: question.into(), then: Box::new(then) }
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

struct Field {
    label: &'static str,
    kind: FieldKind,
}

enum FieldKind {
    Text(String),
    /// (value, shown) options and the chosen one.
    Choice { options: Vec<(String, String)>, sel: usize },
}

impl Field {
    fn text(label: &'static str) -> Self {
        Self { label, kind: FieldKind::Text(String::new()) }
    }

    fn choice(label: &'static str, options: Vec<(String, String)>) -> Self {
        Self { label, kind: FieldKind::Choice { options, sel: 0 } }
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
    Menu { title: String, items: Vec<(String, Action)>, sel: usize },
    Input { title: String, prompt: String, buf: String, then: Box<Action> },
    Confirm { question: String, then: Box<Action> },
    Text { title: String, lines: Vec<String>, scroll: usize },
    Form(Form),
}

struct App {
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
    ov: Option<MeshOverview>,
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
        let root = if path.starts_with("/json") { "" } else { "/api" };
        if local {
            format!("{root}{path}")
        } else {
            format!("/api/peers/{}{path}", urlencode(node))
        }
    }

    async fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        let res = self
            .request(reqwest::Method::GET, "/api/mesh/overview")
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => match r.json::<MeshOverview>().await {
                Ok(ov) => {
                    self.ov = Some(ov);
                    // Only the refresh's own error goes away with it; an action's stays.
                    if self.refresh_error {
                        self.refresh_error = false;
                        self.set_status("", false);
                    }
                }
                Err(e) => self.refresh_failed(format!("Unexpected answer: {e}")),
            },
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => self.refresh_failed(
                "Unauthorized: pass --secret-file (or run where the dashboard is reachable)"
                    .to_string(),
            ),
            Ok(r) => self.refresh_failed(format!("{}: HTTP {}", self.base, r.status())),
            Err(e) => self.refresh_failed(format!("{}: {e}", self.base)),
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

    async fn act(&mut self, label: &str, req: reqwest::RequestBuilder) {
        self.set_status(format!("{label}…"), false);
        match req.send().await {
            Ok(r) if r.status().is_success() => self.set_status(format!("{label}: done"), false),
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let msg = body["error"].as_str().map_or_else(
                    || if text.is_empty() { status.to_string() } else { text.clone() },
                    str::to_string,
                );
                self.set_status(format!("{label}: {msg}"), true);
            }
            Err(e) => self.set_status(format!("{label}: {e}"), true),
        }
        self.refresh().await;
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
            Tab::Sessions => self.sessions().len(),
            Tab::Profiles => self.profiles().len(),
            Tab::Recordings => self.recordings().len(),
            Tab::Nodes => 1 + ov.peers.len(),
            Tab::Engines => self.engine_rows().len(),
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
    async fn key(&mut self, k: KeyEvent) -> bool {
        if self.modal.is_some() {
            self.modal_key(k).await;
            return true;
        }
        if self.help {
            self.help = false;
            return true;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
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
                self.refresh().await;
                self.set_status("Refreshed", false);
            }
            KeyCode::Char('f') => self.toggle_filter(),
            KeyCode::Char('m') | KeyCode::Char(' ') => {
                if let Some(a) = self.item_menu() {
                    self.run(a).await;
                }
            }
            KeyCode::Char('M') => {
                let a = self.tab_menu();
                self.run(a).await;
            }
            _ => self.tab_key(k).await,
        }
        true
    }

    fn cycle_tab(&mut self, delta: isize) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.tab = TABS[(i + delta).rem_euclid(TABS.len() as isize) as usize];
    }

    /// The node of the selected row.
    fn selected_node(&mut self) -> Option<String> {
        match self.tab {
            Tab::Sessions => self.session().map(|s| s.node_name),
            Tab::Profiles => self.profile().map(|p| p.node_name),
            Tab::Recordings => self.recording().map(|r| r.node_name),
            Tab::Nodes => self.node_at_selection(),
            Tab::Engines => self.engine_row().map(|r| r.node),
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
    async fn tab_key(&mut self, k: KeyEvent) {
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
            (Tab::Recordings, KeyCode::Enter | KeyCode::Char('o')) => {
                self.recording().map(|r| Action::Open(play_url(&r)))
            }
            (Tab::Recordings, KeyCode::Char('p')) => self.recording().map(|r| keep_recording(&r)),
            (Tab::Recordings, KeyCode::Char('E')) => {
                self.recording().map(|r| export_recording(&r))
            }
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
                    self.engine_patch(&r, &format!("Ad blocker: {}", tri_label(next)), |d| d.adblock = next)
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
                    self.engine_patch(&r, &format!("Appearance: {}", scheme_label(next)), |d| d.color_scheme = next)
                })
            }
            (Tab::Engines, KeyCode::Char('t')) => {
                let row = self.engine_row();
                row.map(|r| {
                    let next = !r.defaults.show_touches;
                    self.engine_patch(&r, &format!("Show touches: {}", if next { "on" } else { "off" }), |d| d.show_touches = next)
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
            self.run(a).await;
        }
    }

    fn details_action(&mut self) -> Option<Action> {
        match self.tab {
            Tab::Sessions => {
                let s = self.session()?;
                Some(details(&s.session.name, &s))
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
                Some(details(&format!("{} on {}", r.engine, r.node), &r.defaults))
            }
        }
    }

    /// One row per node and engine it supports, with the engine's defaults there.
    fn engine_rows(&self) -> Vec<EngineRow> {
        let Some(ov) = self.ov.as_ref() else {
            return Vec::new();
        };
        let mut nodes: Vec<(String, Vec<BrowserEngine>, Option<NodeSettings>)> = vec![(
            ov.local_node.name.clone(),
            ov.local_node.supported_engines.clone(),
            Some(ov.local_node.settings.clone()),
        )];
        nodes.extend(
            ov.peers
                .iter()
                .filter(|p| p.status == "online")
                .map(|p| (p.name.clone(), p.supported_engines.clone(), p.settings.clone())),
        );
        let mut rows = Vec::new();
        for (node, engines, settings) in nodes {
            if !self.keep(&node) {
                continue;
            }
            for engine in engines {
                let defaults = settings
                    .as_ref()
                    .map(|s| s.engines.of(engine).clone())
                    .unwrap_or_default();
                rows.push(EngineRow { node: node.clone(), engine, defaults });
            }
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

    /// Change one engine's defaults on its node: the node takes the engines as a whole.
    fn engine_patch(
        &self,
        row: &EngineRow,
        label: &str,
        change: impl FnOnce(&mut EngineDefaults),
    ) -> Action {
        let Some(settings) = self.node_settings(&row.node) else {
            return Action::Say(format!("{}'s settings are not available", row.node));
        };
        let mut engines = settings.engines.clone();
        change(match row.engine {
            BrowserEngine::Chromium => &mut engines.chromium,
            BrowserEngine::Firefox => &mut engines.firefox,
        });
        Action::Api(
            Call::new(
                format!("{} ({} on {})", label, row.engine, row.node),
                Method::PATCH,
                &row.node,
                "/settings",
            )
            .body(json!({ "engines": serde_json::to_value(&engines).unwrap_or(Value::Null) })),
        )
    }

    fn engine_menu(&self, row: &EngineRow) -> Action {
        let d = &row.defaults;
        let mark = |on: bool| if on { "✓ " } else { "  " };
        let adblock = [None, Some(true), Some(false)].map(|v| {
            item(
                format!("{}{}", mark(d.adblock == v), tri_label(v)),
                self.engine_patch(row, &format!("Ad blocker: {}", tri_label(v)), |e| e.adblock = v),
            )
        });
        let scheme = [None, Some(ColorScheme::Dark), Some(ColorScheme::Light)].map(|v| {
            item(
                format!("{}{}", mark(d.color_scheme == v), scheme_label(v)),
                self.engine_patch(row, &format!("Appearance: {}", scheme_label(v)), |e| e.color_scheme = v),
            )
        });
        Action::Menu {
            title: format!("{} on {}", row.engine, row.node),
            items: vec![
                item(
                    format!("Ad blocker: {}…", tri_label(d.adblock)),
                    Action::Menu { title: "Ad blocker".into(), items: adblock.to_vec() },
                ),
                item(
                    format!("Appearance: {}…", scheme_label(d.color_scheme)),
                    Action::Menu { title: "Appearance".into(), items: scheme.to_vec() },
                ),
                item(
                    format!("Show touches: {}", if d.show_touches { "on" } else { "off" }),
                    self.engine_patch(
                        row,
                        &format!("Show touches: {}", if d.show_touches { "off" } else { "on" }),
                        |e| e.show_touches = !d.show_touches,
                    ),
                ),
                item("Details", details(&format!("{} on {}", row.engine, row.node), d)),
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
        let mut items = vec![item(format!("{}Direct", mark(current.is_none())), make(None))];
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
            return Some(Action::Say(format!("{} has no proxies configured", s.node_name)));
        }
        let choices: Vec<Option<String>> = std::iter::once(None)
            .chain(proxies.into_iter().map(|x| Some(x.id)))
            .collect();
        let cur = choices.iter().position(|c| *c == s.session.proxy).unwrap_or(0);
        let next = choices[(cur + 1) % choices.len()].clone();
        Some(set_session_proxy(self, s, next.as_deref()))
    }

    fn cycle_profile_proxy(&self, p: &MeshProfile) -> Option<Action> {
        let proxies = self.node_proxies(&p.node_name);
        if proxies.is_empty() {
            return Some(Action::Say(format!("{} has no proxies configured", p.node_name)));
        }
        let choices: Vec<Option<String>> = std::iter::once(None)
            .chain(proxies.into_iter().map(|x| Some(x.id)))
            .collect();
        let cur = choices.iter().position(|c| *c == p.profile.proxy).unwrap_or(0);
        let next = choices[(cur + 1) % choices.len()].clone();
        Some(set_profile_proxy(self, p, next.as_deref()))
    }

    fn stop_all_action(&self) -> Action {
        let n = self.sessions().len();
        let scope = self
            .filter
            .as_ref()
            .map(|f| format!(" on {f}"))
            .unwrap_or_default();
        confirm_then(
            format!("Stop {n} session{}{scope}?", if n == 1 { "" } else { "s" }),
            Action::StopAll(self.filter.clone()),
        )
    }

    // ----------------------------------------------------------------- menus

    fn item_menu(&mut self) -> Option<Action> {
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
                    item("New session…", Action::Form(FormKind::Session, self.filter.clone())),
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
                vec![item("Defaults apply to new sessions of the engine: pick a row, then m", Action::Say("Select a row, then press m".into()))],
            ),
        };
        Action::Menu { title: title.into(), items }
    }

    fn session_menu(&self, s: &MeshSession) -> Action {
        let x = &s.session;
        let mut items = vec![
            item("Take over", Action::Open(handoff_url(&s.node_url, x.id))),
            item("New tab…", new_tab(s)),
            item("Rename…", rename_session(s)),
            item(
                format!("Auto-stop when idle: {}…", idle_label(x.idle_timeout_seconds)),
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
        Action::Menu { title: x.name.clone(), items }
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
            if x.session_id.is_some() { "Take over" } else { "Launch" },
            launch_profile(p),
        )];
        if x.session_id.is_none() && !x.tabs.is_empty() {
            items.push(item("Launch without the default tabs", launch_profile_blank(p)));
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
                    item("Dark", patch("Appearance: dark", json!({ "color_scheme": "dark" }))),
                    item("Light", patch("Appearance: light", json!({ "color_scheme": "light" }))),
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
                    Call::new("Duplicating", Method::POST, &p.node_name, format!("{base}/clone"))
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
        Action::Menu { title: x.name.clone(), items }
    }

    fn node_menu(&self, name: &str) -> Action {
        let is_local = name == self.local_node();
        let only = self.filter.as_deref() == Some(name);
        let mut items = vec![
            item(
                if only { "Show every node".to_string() } else { format!("Show only {name}") },
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
        Action::Menu { title: name.to_string(), items }
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

    // --------------------------------------------------------------- running

    async fn run(&mut self, action: Action) {
        match action {
            Action::Api(call) => self.call(call).await,
            Action::Open(url) => self.open(&url),
            Action::Say(text) => self.set_status(text, false),
            Action::Menu { title, items } => {
                self.modal = Some(Modal::Menu { title, items, sel: 0 });
            }
            Action::Input { title, prompt, initial, then } => {
                self.modal = Some(Modal::Input { title, prompt, buf: initial, then });
            }
            Action::Confirm { question, then } => {
                self.modal = Some(Modal::Confirm { question, then });
            }
            Action::Text { title, lines } => {
                self.modal = Some(Modal::Text { title, lines, scroll: 0 });
            }
            Action::Form(kind, node) => self.open_form(kind, node),
            Action::StopAll(node) => self.stop_all(node).await,
            Action::Filter(node) => {
                self.filter = node;
                self.clamp_selection();
            }
            Action::DataMenu(p) => self.data_menu(p).await,
        }
    }

    async fn call(&mut self, call: Call) {
        let url = self.api(&call.node, &call.path);
        let mut req = self.request(call.method.clone(), &url);
        if call.long {
            req = req.timeout(Duration::from_secs(3600));
        }
        if let Some(ref body) = call.body {
            req = req.json(body);
        }
        if let Some(ref file) = call.upload {
            match std::fs::read(expand_home(file)) {
                Ok(data) => req = req.body(data),
                Err(e) => {
                    self.set_status(format!("{file}: {e}"), true);
                    return;
                }
            }
        }
        let Some(ref file) = call.save_to else {
            self.act(&call.label, req).await;
            return;
        };
        self.set_status(format!("{}…", call.label), false);
        match req.send().await {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(data) => match std::fs::write(expand_home(file), &data) {
                    Ok(()) => self.set_status(
                        format!("Saved {file} ({})", bytes(data.len() as u64)),
                        false,
                    ),
                    Err(e) => self.set_status(format!("{file}: {e}"), true),
                },
                Err(e) => self.set_status(format!("{}: {e}", call.label), true),
            },
            Ok(r) => self.set_status(format!("{}: HTTP {}", call.label, r.status()), true),
            Err(e) => self.set_status(format!("{}: {e}", call.label), true),
        }
    }

    async fn stop_all(&mut self, node: Option<String>) {
        let targets: Vec<(String, String)> = self
            .sessions()
            .iter()
            .filter(|s| node.as_deref().is_none_or(|n| s.node_name == n))
            .map(|s| (s.node_name.clone(), s.session.id.to_string()))
            .collect();
        if targets.is_empty() {
            self.set_status("No sessions", false);
            return;
        }
        self.set_status(format!("Stopping {} sessions…", targets.len()), false);
        let results = futures_util::future::join_all(targets.iter().map(|(n, id)| {
            let url = self.api(n, &format!("/sessions/{id}"));
            self.request(Method::DELETE, &url).send()
        }))
        .await;
        let failed = results
            .iter()
            .filter(|r| !matches!(r, Ok(res) if res.status().is_success()))
            .count();
        if failed == 0 {
            self.set_status(format!("Stopped {} sessions", targets.len()), false);
        } else {
            self.set_status(
                format!("{failed} of {} sessions could not be stopped", targets.len()),
                true,
            );
        }
        self.refresh().await;
    }

    /// A profile's backups, with what can be done to its data.
    async fn data_menu(&mut self, p: MeshProfile) {
        let base = profile_base(&p);
        let url = self.api(&p.node_name, &format!("{base}/backups"));
        let list: Vec<Value> = match self.request(Method::GET, &url).send().await {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) => {
                self.set_status(format!("Backups: HTTP {}", r.status()), true);
                return;
            }
            Err(e) => {
                self.set_status(format!("Backups: {e}"), true);
                return;
            }
        };
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
                if b["automatic"].as_bool().unwrap_or(false) { "auto" } else { "manual" },
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
        let force = if p.profile.is_active { "&force=true" } else { "" };
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

    fn node_choice(&self, preset: Option<&str>) -> Field {
        let nodes = self.node_names();
        let chosen = preset
            .or(self.filter.as_deref())
            .and_then(|n| nodes.iter().position(|x| x == n))
            .unwrap_or(0);
        let mut f = Field::choice("Node", nodes.into_iter().map(|n| (n.clone(), n)).collect());
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
        let node_field = self.node_choice(node.as_deref());
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
        self.modal = Some(Modal::Form(Form { kind, fields, sel: 0 }));
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

    async fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match modal {
            Modal::Menu { title, items, mut sel } => {
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
                            self.run(a).await;
                            return;
                        }
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        if let Some((_, a)) = items.get(c as usize - '1' as usize) {
                            let a = a.clone();
                            self.run(a).await;
                            return;
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Menu { title, items, sel });
            }
            Modal::Input { title, prompt, mut buf, then } => {
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let a = then.with_input(buf.trim());
                        self.run(a).await;
                        return;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.modal = Some(Modal::Input { title, prompt, buf, then });
            }
            Modal::Confirm { then, .. } => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.run(*then).await;
                } else {
                    self.set_status("Cancelled", false);
                }
            }
            Modal::Text { title, lines, mut scroll } => {
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
                self.modal = Some(Modal::Text { title, lines, scroll });
            }
            Modal::Form(mut f) => {
                let n = f.fields.len();
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let a = self.form_action(&f);
                        self.run(a).await;
                        return;
                    }
                    KeyCode::Up | KeyCode::BackTab => f.sel = (f.sel + n - 1) % n,
                    KeyCode::Down | KeyCode::Tab => f.sel = (f.sel + 1) % n,
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
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.set_status(format!("Opened {url}"), false),
            Err(_) => self.set_status(url.to_string(), false),
        }
    }
}

// ------------------------------------------------------- actions of one row

fn handoff_url(node_url: &str, session: impl std::fmt::Display) -> String {
    format!("{}/handoff/?session={session}", node_url.trim_end_matches('/'))
}

fn play_url(r: &MeshRecording) -> String {
    format!("{}/?play={}", r.node_url.trim_end_matches('/'), r.recording.id)
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
        Some(id) => format!("Session now goes through {}", app.proxy_name(&s.node_name, id)),
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
        Some(id) => format!("{} now uses {}", p.profile.name, app.proxy_name(&p.node_name, id)),
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
    let force = if p.profile.is_active { "?force=true" } else { "" };
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
            item("Play", Action::Open(play_url(r))),
            item(
                if r.recording.keep { "Stop keeping (the cleanup may remove it)" } else { "Keep (the cleanup never removes it)" },
                keep_recording(r),
            ),
            item("Details", details("Recording", r)),
            item("Export frames…", export_recording(r)),
            item("Apply retention now", vacuum(&r.node_name)),
            item("Delete", delete_recording(r)),
        ],
    }
}

/// Keep a recording (the cleanup never removes it), or stop keeping it.
fn keep_recording(r: &MeshRecording) -> Action {
    let keep = !r.recording.keep;
    Action::Api(
        Call::new(
            if keep { "Keeping the recording" } else { "No longer keeping the recording" },
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
            if r.recording.keep { " It is marked to be kept." } else { "" }
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
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    Action::Text {
        title: title.to_string(),
        lines: text.lines().map(str::to_string).collect(),
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
    Row::new(cells.iter().map(|c| Cell::from(c.to_string())))
        .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Gray))
        .bottom_margin(0)
}

fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, detail, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // Tabs with counts, and the node summary on the right.
    let counts = app.ov.as_ref().map(|o| {
        [
            app.sessions().len(),
            app.profiles().len(),
            app.recordings().len(),
            1 + o.peers.len(),
            app.engine_rows().len(),
        ]
    });
    let titles: Vec<Line> = TABS
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let n = counts.map(|c| format!(" {}", c[i])).unwrap_or_default();
            Line::from(vec![
                Span::raw(format!("{}{}", i + 1, ". ")),
                Span::raw(t.title()),
                Span::raw(n).dim(),
            ])
        })
        .collect();
    let selected = TABS.iter().position(|t| *t == app.tab).unwrap_or(0);
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .highlight_style(Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD))
            .divider(" "),
        top,
    );
    if let Some(ref ov) = app.ov {
        let only = app
            .filter
            .as_ref()
            .map(|f| format!("only {f}  ·  "))
            .unwrap_or_default();
        let summary = format!(
            "{only}{}  ·  {}/{} nodes online ",
            ov.local_node.name, ov.online_nodes, ov.total_nodes
        );
        let w = summary.chars().count() as u16;
        if top.width > w + 40 {
            f.render_widget(
                Paragraph::new(summary).dim(),
                Rect {
                    x: top.x + top.width - w,
                    width: w,
                    ..top
                },
            );
        }
    }

    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().dark_gray());
    let highlight = Style::new()
        .bg(Color::Rgb(60, 50, 120))
        .add_modifier(Modifier::BOLD);
    let Some(ov) = app.ov.clone() else {
        f.render_widget(Paragraph::new("Connecting…").block(block), body);
        draw_footer(f, app, foot);
        return;
    };
    let i = TABS.iter().position(|t| *t == app.tab).unwrap_or(0);
    let sessions: Vec<MeshSession> = app.sessions().into_iter().cloned().collect();
    let profiles: Vec<MeshProfile> = app.profiles().into_iter().cloned().collect();
    let recordings: Vec<MeshRecording> = app.recordings().into_iter().cloned().collect();

    match app.tab {
        Tab::Sessions => {
            let rows = sessions.iter().map(|s| {
                let ss = &s.session;
                let rec = if ss.is_recording {
                    if ss.recording_paused {
                        "paused"
                    } else {
                        "● rec"
                    }
                } else {
                    ""
                };
                Row::new(vec![
                    Cell::from(ss.name.clone()),
                    Cell::from(ss.profile.clone().unwrap_or_else(|| "(throwaway)".into())),
                    Cell::from(s.node_name.clone()),
                    Cell::from(ss.engine.to_string()),
                    Cell::from(
                        ss.proxy
                            .as_deref()
                            .map(|p| app.proxy_name(&s.node_name, p))
                            .unwrap_or_default(),
                    ),
                    Cell::from(ss.tabs_count.to_string()),
                    Cell::from(dur(ss.uptime_seconds)),
                    Cell::from(ss.client_count.to_string()),
                    Cell::from(rec).fg(Color::Red),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Fill(2),
                    Constraint::Fill(3),
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Fill(2),
                    Constraint::Length(5),
                    Constraint::Length(9),
                    Constraint::Length(8),
                    Constraint::Length(7),
                ],
            )
            .header(header(&[
                "Name", "Profile", "Node", "Engine", "Proxy", "Tabs", "Uptime", "Clients", "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let detail_lines = app.tables[i]
                .selected()
                .and_then(|n| sessions.get(n))
                .map(|s| {
                    let mut lines = vec![Line::from(vec![
                        Span::raw("id ").dim(),
                        Span::raw(s.session.id.to_string()),
                        Span::raw("   idle ").dim(),
                        Span::raw(dur(s.session.idle_seconds)),
                        Span::raw(if s.session.adblock {
                            "   ads blocked"
                        } else {
                            ""
                        })
                        .dim(),
                    ])];
                    lines.extend(
                        ov.targets
                            .iter()
                            .filter(|t| {
                                t.node_name == s.node_name
                                    && t.target.session_id == Some(s.session.id)
                                    && t.target.target_type == "page"
                            })
                            .map(|t| {
                                Line::from(vec![
                                    Span::raw("  • "),
                                    Span::raw(t.target.title.clone().unwrap_or_default()).bold(),
                                    Span::raw("  "),
                                    Span::raw(t.target.url.clone()).dim(),
                                ])
                            }),
                    );
                    lines
                });
            draw_detail(f, detail, detail_lines);
        }
        Tab::Profiles => {
            let rows = profiles.iter().map(|p| {
                let pp = &p.profile;
                Row::new(vec![
                    Cell::from(pp.name.clone()),
                    Cell::from(p.node_name.clone()),
                    Cell::from(pp.engine.to_string()),
                    Cell::from(
                        pp.proxy
                            .as_deref()
                            .map(|x| app.proxy_name(&p.node_name, x))
                            .unwrap_or_else(|| "direct".into()),
                    ),
                    Cell::from(bytes(pp.size_bytes)),
                    Cell::from(ago(pp.modified_at_secs)),
                    Cell::from(if pp.is_active { "running" } else { "" }).fg(Color::Green),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Fill(3),
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Fill(2),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(8),
                ],
            )
            .header(header(&[
                "Profile", "Node", "Engine", "Proxy", "Size", "Used", "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let lines = app.tables[i]
                .selected()
                .and_then(|n| profiles.get(n))
                .map(|p| {
                    vec![
                        Line::from(vec![
                            Span::raw("id ").dim(),
                            Span::raw(p.profile.id.map(|i| i.to_string()).unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            Span::raw("storage ").dim(),
                            Span::raw(p.profile.storage_dir.clone().unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            Span::raw("tabs ").dim(),
                            Span::raw(if p.profile.tabs.is_empty() {
                                "(none: a blank tab)".to_string()
                            } else {
                                p.profile.tabs.join("  ")
                            }),
                        ]),
                    ]
                });
            draw_detail(f, detail, lines);
        }
        Tab::Recordings => {
            let rows = recordings.iter().map(|r| {
                let m = &r.recording;
                Row::new(vec![
                    Cell::from(
                        m.title
                            .clone()
                            .filter(|t| !t.is_empty() && t != "about:blank")
                            .or_else(|| m.final_url.clone())
                            .unwrap_or_default(),
                    ),
                    Cell::from(m.profile.clone().unwrap_or_else(|| "(throwaway)".into())),
                    Cell::from(r.node_name.clone()),
                    Cell::from(dur(m.duration_seconds)),
                    Cell::from(bytes(m.size_bytes)),
                    Cell::from(ago(Some(m.started_at_secs))),
                    Cell::from(if m.status == "recording" {
                        "● rec"
                    } else if m.keep {
                        "★ kept"
                    } else {
                        ""
                    })
                    .fg(if m.keep { Color::Yellow } else { Color::Red }),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Fill(3),
                    Constraint::Fill(2),
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(6),
                ],
            )
            .header(header(&[
                "Recording",
                "Profile",
                "Node",
                "Length",
                "Size",
                "Started",
                "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let lines = app.tables[i]
                .selected()
                .and_then(|n| recordings.get(n))
                .map(|r| {
                    vec![
                        Line::from(vec![
                            Span::raw("id ").dim(),
                            Span::raw(r.recording.id.to_string()),
                            Span::raw("   frames ").dim(),
                            Span::raw(r.recording.frame_count.to_string()),
                        ]),
                        Line::from(vec![
                            Span::raw("from ").dim(),
                            Span::raw(r.recording.initial_url.clone().unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            Span::raw("to   ").dim(),
                            Span::raw(r.recording.final_url.clone().unwrap_or_default()),
                        ]),
                    ]
                });
            draw_detail(f, detail, lines);
        }
        Tab::Nodes => {
            let mut rows = vec![Row::new(vec![
                Cell::from(format!("{} (here)", ov.local_node.name)),
                Cell::from("online").fg(Color::Green),
                Cell::from(format!(
                    "{}/{}",
                    ov.local_node.active_sessions, ov.local_node.max_sessions
                )),
                Cell::from(ov.local_node.profiles_count.to_string()),
                Cell::from(ov.local_node.proxies.len().to_string()),
                Cell::from("–"),
                Cell::from(dur(ov.local_node.uptime_seconds)),
            ])];
            rows.extend(ov.peers.iter().map(|p| {
                Row::new(vec![
                    Cell::from(p.name.clone()),
                    Cell::from(p.status.clone()).fg(if p.status == "online" {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                    Cell::from(format!("{}/{}", p.active_sessions, p.max_sessions)),
                    Cell::from(p.profiles_count.to_string()),
                    Cell::from(p.proxies.len().to_string()),
                    Cell::from(
                        p.latency_ms
                            .map(|l| format!("{l} ms"))
                            .unwrap_or_else(|| "–".into()),
                    ),
                    Cell::from(dur(p.uptime_seconds)),
                ])
            }));
            let table = Table::new(
                rows,
                [
                    Constraint::Fill(2),
                    Constraint::Length(12),
                    Constraint::Length(10),
                    Constraint::Length(9),
                    Constraint::Length(8),
                    Constraint::Length(9),
                    Constraint::Length(10),
                ],
            )
            .header(header(&[
                "Node", "Status", "Sessions", "Profiles", "Proxies", "Latency", "Uptime",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let node = app.tables[i].selected().and_then(|n| {
                if n == 0 {
                    Some(ov.local_node.name.clone())
                } else {
                    ov.peers.get(n - 1).map(|p| p.name.clone())
                }
            });
            let lines = node.map(|n| {
                let proxies = app.node_proxies(&n);
                if proxies.is_empty() {
                    vec![Line::from("No proxies").dim()]
                } else {
                    proxies
                        .into_iter()
                        .map(|p| {
                            Line::from(vec![
                                Span::raw("  • "),
                                Span::raw(p.name).bold(),
                                Span::raw(format!("  {}://{}:{}", p.scheme, p.host, p.port)).dim(),
                            ])
                        })
                        .collect()
                }
            });
            draw_detail(f, detail, lines);
        }
        Tab::Engines => {
            let rows_data = app.engine_rows();
            let rows = rows_data.iter().map(|r| {
                Row::new(vec![
                    Cell::from(r.node.clone()),
                    Cell::from(r.engine.to_string()),
                    Cell::from(tri_label(r.defaults.adblock)),
                    Cell::from(scheme_label(r.defaults.color_scheme)),
                    Cell::from(if r.defaults.show_touches { "on" } else { "off" }),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Fill(2),
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(12),
                    Constraint::Length(14),
                ],
            )
            .header(header(&["Node", "Engine", "Ad blocker", "Appearance", "Show touches"]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let lines = app.tables[i].selected().and_then(|n| rows_data.get(n)).map(|r| {
                let proxy = app
                    .node_settings(&r.node)
                    .and_then(|s| s.default_proxy)
                    .map(|p| app.proxy_name(&r.node, &p))
                    .unwrap_or_else(|| "direct".into());
                vec![
                    Line::from(vec![
                        Span::raw("New ").dim(),
                        Span::raw(r.engine.to_string()),
                        Span::raw(" sessions on ").dim(),
                        Span::raw(r.node.clone()),
                        Span::raw(" start with these unless the request or the profile says otherwise.").dim(),
                    ]),
                    Line::from(vec![
                        Span::raw("node default proxy ").dim(),
                        Span::raw(proxy),
                        Span::raw("   (set it from the Nodes tab)").dim(),
                    ]),
                ]
            });
            draw_detail(f, detail, lines);
        }
    }

    draw_footer(f, app, foot);

    if let Some(ref modal) = app.modal {
        draw_modal(f, modal);
    } else if app.help {
        popup(f, "Keys", help_lines());
    }
}

fn draw_detail(f: &mut Frame, area: Rect, lines: Option<Vec<Line<'static>>>) {
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().dark_gray());
    f.render_widget(
        Paragraph::new(lines.unwrap_or_default())
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match app.tab {
        Tab::Sessions => "⏎ take over  s stop  p proxy  R record  t tab  e rename  N new  X stop all  m menu",
        Tab::Profiles => "⏎ launch  p proxy  b backups  e rename  N new  d delete  m menu",
        Tab::Recordings => "⏎ play  p keep  E export  d delete  v vacuum  m menu",
        Tab::Nodes => "n new session  f filter  a add peer  x remove  m menu",
        Tab::Engines => "a ad blocker  c appearance  t touches  m menu  (defaults of new sessions)",
    };
    let line = if app.status.is_empty() {
        Line::from(vec![
            Span::raw(hints).dim(),
            Span::raw("   ? help  q quit").dim(),
        ])
    } else if app.error {
        Line::from(Span::raw(app.status.clone()).fg(Color::Red))
    } else {
        Line::from(Span::raw(app.status.clone()).fg(Color::Cyan))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn help_lines() -> Vec<Line<'static>> {
    [
        ("Tab / ← →  1-5", "switch view"),
        ("↑ ↓  j k  g G", "move"),
        ("m / Space", "menu of the selected row: everything the dashboard offers"),
        ("M", "menu of the view (new session, stop all, ...)"),
        ("f", "show only the selected row's node (again: all nodes)"),
        ("i", "details"),
        ("r", "refresh now"),
        ("", ""),
        ("Sessions", ""),
        ("  ⏎ / o", "open the handoff view in your browser"),
        ("  s", "stop the session"),
        ("  X", "stop every session in view"),
        ("  p", "switch its proxy now (direct, then each proxy)"),
        ("  R", "start or stop recording"),
        ("  t / e / T", "new tab / rename / auto-stop timeout"),
        ("  n / N", "throwaway session on that node / new session form"),
        ("  w", "show the session's WebSocket URL"),
        ("Profiles", ""),
        ("  ⏎ / o", "launch it, or take over its session"),
        ("  p", "cycle its default proxy (from the next launch)"),
        ("  b", "backups & data: back up, restore, clear, export, import"),
        ("  e / N", "rename / new profile form"),
        ("  d", "delete it"),
        ("Recordings", ""),
        ("  ⏎ / o", "play it in the dashboard"),
        ("  p", "keep it (the cleanup never removes it) / stop keeping it"),
        ("  E / v", "export its frames / apply the retention limits"),
        ("  d", "delete it"),
        ("Nodes", ""),
        ("  n / a / x", "session there / add a peer / remove the peer"),
        ("Engines", ""),
        ("  a / c / t", "cycle ad blocker / appearance, toggle show touches (defaults of new sessions)"),
        ("In a menu or form", ""),
        ("  ⏎  Esc", "choose / cancel   (1-9 pick an item)"),
        ("  ← →  Tab", "change a choice / next field"),
        ("", ""),
        ("q / Esc", "quit"),
    ]
    .into_iter()
    .map(|(k, v)| {
        if v.is_empty() {
            Line::from(Span::raw(k).bold())
        } else {
            Line::from(vec![
                Span::raw(format!("{k:<16}")).fg(Color::Magenta),
                Span::raw(v),
            ])
        }
    })
    .collect()
}

/// Lines of `all` around the selected one that fit in `max`.
fn window(all: Vec<Line<'static>>, sel: usize, max: usize) -> Vec<Line<'static>> {
    if all.len() <= max {
        return all;
    }
    let start = sel.saturating_sub(max / 2).min(all.len() - max);
    all.into_iter().skip(start).take(max).collect()
}

fn draw_modal(f: &mut Frame, modal: &Modal) {
    let room = f.area().height.saturating_sub(6) as usize;
    let selected = |text: String| Line::from(Span::raw(format!("▸ {text}")).fg(Color::Magenta).bold());
    let hint = |text: &str| Line::from(Span::raw(text.to_string())).dim();
    match modal {
        Modal::Menu { title, items, sel } => {
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .map(|(i, (label, _))| {
                    let n = if i < 9 { format!("{} ", i + 1) } else { "  ".to_string() };
                    if i == *sel {
                        selected(format!("{n}{label}"))
                    } else {
                        Line::from(format!("  {n}{label}"))
                    }
                })
                .collect();
            let mut lines = window(lines, *sel, room.max(3));
            lines.push(Line::from(""));
            lines.push(hint("⏎ choose   Esc close   1-9 pick"));
            popup(f, title, lines);
        }
        Modal::Input { title, prompt, buf, .. } => popup(
            f,
            title,
            vec![
                Line::from(prompt.clone()),
                Line::from(vec![Span::raw("› "), Span::raw(buf.clone()), Span::raw("▏")]),
                Line::from(""),
                hint("⏎ ok   Esc cancel   ^U clear"),
            ],
        ),
        Modal::Confirm { question, .. } => popup(
            f,
            "Confirm",
            vec![
                Line::from(question.clone()),
                Line::from(""),
                hint("y: yes   any other key: no"),
            ],
        ),
        Modal::Text { title, lines, scroll } => {
            let mut shown: Vec<Line> = lines
                .iter()
                .skip(*scroll)
                .take(room.max(3))
                .map(|l| Line::from(l.clone()))
                .collect();
            shown.push(Line::from(""));
            shown.push(hint("↑↓ scroll   Esc close"));
            popup(f, title, shown);
        }
        Modal::Form(form) => {
            let mut lines: Vec<Line> = form
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let value = match &field.kind {
                        FieldKind::Text(t) => {
                            if i == form.sel { format!("{t}▏") } else { t.clone() }
                        }
                        FieldKind::Choice { options, sel } => format!(
                            "‹ {} ›",
                            options.get(*sel).map(|o| o.1.as_str()).unwrap_or("–")
                        ),
                    };
                    let text = format!("{:<12} {value}", field.label);
                    if i == form.sel { selected(text) } else { Line::from(format!("  {text}")) }
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(hint("↑↓ field   ← → change   ⏎ create   Esc cancel"));
            let title = match form.kind {
                FormKind::Session => "New session",
                FormKind::Profile => "New profile",
            };
            popup(f, title, lines);
        }
    }
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line<'static>>) {
    let area = f.area();
    let w = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(20)
        .clamp(30, area.width.saturating_sub(4) as usize) as u16
        + 4;
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::new().fg(Color::Magenta)),
        ),
        rect,
    );
}

/// What to authenticate with: the cluster secret from `file` (an explicit file
/// that cannot be read is an error), else from the NixOS module's file when
/// readable, else the `bd_auth` token the dashboard at `base` hands to whoever
/// can open it. `None`: the daemon wants nothing.
pub(crate) async fn find_secret(
    http: &reqwest::Client,
    base: &str,
    file: Option<&PathBuf>,
) -> Result<Option<String>, String> {
    if let Some(path) = file {
        let secret = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read the secret file {}: {e}", path.display()))?;
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(format!("The secret file {} is empty", path.display()));
        }
        return Ok(Some(secret.to_string()));
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

pub async fn run(args: crate::ctl::Conn) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let base = args.url.trim_end_matches('/').to_string();
    let secret = find_secret(&http, &base, args.secret_file.as_ref()).await?;
    let mut app = App {
        http,
        base,
        secret,
        ov: None,
        tab: Tab::Sessions,
        tables: Default::default(),
        status: String::new(),
        error: false,
        refresh_error: false,
        modal: None,
        filter: None,
        help: false,
        last_refresh: None,
    };
    app.refresh().await;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && !app.key(k).await {
                    return Ok(());
                }
            }
        } else if app.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH) {
            app.refresh().await;
        }
    }
}

#[cfg(test)]
mod secret_tests {
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
