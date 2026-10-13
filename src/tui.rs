//! `browser-daemon tui`: a terminal view of the mesh (sessions, profiles,
//! recordings, nodes) with the everyday actions, talking to the HTTP API.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::{MeshOverview, MeshProfile, MeshRecording, MeshSession};

const REFRESH: Duration = Duration::from_secs(3);
const DEFAULT_SECRET_FILE: &str = "/run/secrets/browser-daemon/cluster-secret";

#[derive(clap::Args, Debug)]
pub struct TuiArgs {
    /// Daemon to talk to; its peers are reachable through it.
    #[arg(
        long,
        default_value = "http://127.0.0.1:3001",
        env = "BROWSER_DAEMON_URL"
    )]
    pub url: String,

    /// File holding the cluster secret (default: the NixOS module's, when readable;
    /// otherwise the one the dashboard hands to its viewers).
    #[arg(long, env = "BROWSER_DAEMON_SECRET_FILE")]
    pub secret_file: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sessions,
    Profiles,
    Recordings,
    Nodes,
}

const TABS: [Tab; 4] = [Tab::Sessions, Tab::Profiles, Tab::Recordings, Tab::Nodes];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Sessions => "Sessions",
            Tab::Profiles => "Profiles",
            Tab::Recordings => "Recordings",
            Tab::Nodes => "Nodes",
        }
    }
}

/// An action waiting for y/n.
enum Pending {
    StopSession(MeshSession),
    DeleteProfile(MeshProfile),
    DeleteRecording(MeshRecording),
}

impl Pending {
    fn question(&self) -> String {
        match self {
            Pending::StopSession(s) => format!(
                "Stop {} on {}?",
                s.session
                    .profile
                    .as_deref()
                    .unwrap_or("this throwaway session"),
                s.node_name
            ),
            Pending::DeleteProfile(p) => format!(
                "Delete profile {} on {}? Cookies and logins are wiped.",
                p.profile.name, p.node_name
            ),
            Pending::DeleteRecording(r) => format!(
                "Delete recording {}?",
                r.recording.title.as_deref().unwrap_or("(untitled)")
            ),
        }
    }
}

struct App {
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
    ov: Option<MeshOverview>,
    tab: Tab,
    tables: [TableState; 4],
    status: String,
    error: bool,
    pending: Option<Pending>,
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
        let local = self.ov.as_ref().is_some_and(|o| o.local_node.name == node);
        if local {
            format!("/api{path}")
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
                    if self.error {
                        self.set_status("", false);
                    }
                }
                Err(e) => self.set_status(format!("Unexpected answer: {e}"), true),
            },
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => self.set_status(
                "Unauthorized: pass --secret-file (or run where the dashboard is reachable)",
                true,
            ),
            Ok(r) => self.set_status(format!("{}: HTTP {}", self.base, r.status()), true),
            Err(e) => self.set_status(format!("{}: {e}", self.base), true),
        }
        self.clamp_selection();
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
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                let msg = body["error"]
                    .as_str()
                    .map_or_else(|| status.to_string(), str::to_string);
                self.set_status(format!("{label}: {msg}"), true);
            }
            Err(e) => self.set_status(format!("{label}: {e}"), true),
        }
        self.refresh().await;
    }

    fn rows(&self) -> usize {
        let Some(ref ov) = self.ov else { return 0 };
        match self.tab {
            Tab::Sessions => ov.sessions.len(),
            Tab::Profiles => ov.profiles.len(),
            Tab::Recordings => ov.recordings.len(),
            Tab::Nodes => 1 + ov.peers.len(),
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
        self.ov.as_ref()?.sessions.get(i).cloned()
    }

    fn profile(&mut self) -> Option<MeshProfile> {
        let i = self.selected_index()?;
        self.ov.as_ref()?.profiles.get(i).cloned()
    }

    fn recording(&mut self) -> Option<MeshRecording> {
        let i = self.selected_index()?;
        self.ov.as_ref()?.recordings.get(i).cloned()
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
        if let Some(pending) = self.pending.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.confirm(pending).await;
            } else {
                self.set_status("Cancelled", false);
            }
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
            KeyCode::Char(c @ '1'..='4') => self.tab = TABS[c as usize - '1' as usize],
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
            _ => self.tab_key(k).await,
        }
        true
    }

    fn cycle_tab(&mut self, delta: isize) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.tab = TABS[(i + delta).rem_euclid(TABS.len() as isize) as usize];
    }

    async fn tab_key(&mut self, k: KeyEvent) {
        match (self.tab, k.code) {
            (Tab::Sessions, KeyCode::Enter | KeyCode::Char('o')) => {
                if let Some(s) = self.session() {
                    let url = format!(
                        "{}/handoff/?session={}",
                        s.node_url.trim_end_matches('/'),
                        s.session.id
                    );
                    self.open(&url);
                }
            }
            (Tab::Sessions, KeyCode::Char('s') | KeyCode::Char('x') | KeyCode::Delete) => {
                if let Some(s) = self.session() {
                    self.pending = Some(Pending::StopSession(s));
                }
            }
            (Tab::Sessions, KeyCode::Char('R')) => {
                if let Some(s) = self.session() {
                    let (path, label) = if s.session.is_recording {
                        ("stop-record", "Stopping recording")
                    } else {
                        ("record", "Starting recording")
                    };
                    let url = self.api(&s.node_name, &format!("/sessions/{}/{path}", s.session.id));
                    let req = self.request(reqwest::Method::POST, &url);
                    self.act(label, req).await;
                }
            }
            (Tab::Sessions, KeyCode::Char('w')) => {
                if let Some(s) = self.session() {
                    let engine = if s.session.engine == crate::BrowserEngine::Firefox {
                        "firefox"
                    } else {
                        "chromium"
                    };
                    let ws = format!(
                        "{}/{engine}?session_id={}",
                        s.node_url.trim_end_matches('/').replacen("http", "ws", 1),
                        s.session.id
                    );
                    self.set_status(ws, false);
                }
            }
            (Tab::Sessions | Tab::Nodes, KeyCode::Char('n')) => {
                let node = match self.tab {
                    Tab::Nodes => self.node_at_selection(),
                    _ => self.session().map(|s| s.node_name),
                }
                .or_else(|| self.ov.as_ref().map(|o| o.local_node.name.clone()));
                if let Some(node) = node {
                    let url = self.api(&node, "/sessions");
                    let req = self
                        .request(reqwest::Method::POST, &url)
                        .json(&serde_json::json!({ "persistent": true }));
                    self.act(&format!("Starting a throwaway session on {node}"), req)
                        .await;
                }
            }
            (Tab::Profiles, KeyCode::Enter | KeyCode::Char('o')) => {
                if let Some(p) = self.profile() {
                    if let Some(ref sid) = p.profile.session_id {
                        let url = format!(
                            "{}/handoff/?session={sid}",
                            p.node_url.trim_end_matches('/')
                        );
                        self.open(&url);
                    } else {
                        let url = self.api(&p.node_name, "/sessions");
                        let key = p
                            .profile
                            .id
                            .map_or_else(|| p.profile.name.clone(), |id| id.to_string());
                        let req =
                            self.request(reqwest::Method::POST, &url)
                                .json(&serde_json::json!({
                                    "profile": key, "engine": p.profile.engine, "persistent": true,
                                }));
                        self.act(&format!("Launching {}", p.profile.name), req)
                            .await;
                    }
                }
            }
            (Tab::Profiles, KeyCode::Char('p')) => {
                if let Some(p) = self.profile() {
                    let choices: Vec<Option<String>> = std::iter::once(None)
                        .chain(
                            self.node_proxies(&p.node_name)
                                .into_iter()
                                .map(|x| Some(x.id)),
                        )
                        .collect();
                    if choices.len() == 1 {
                        self.set_status(format!("{} has no proxies configured", p.node_name), true);
                        return;
                    }
                    let cur = choices
                        .iter()
                        .position(|c| *c == p.profile.proxy)
                        .unwrap_or(0);
                    let next = choices[(cur + 1) % choices.len()].clone();
                    let key = p
                        .profile
                        .id
                        .map_or_else(|| p.profile.name.clone(), |id| id.to_string());
                    let url = self.api(&p.node_name, &format!("/profiles/{}", urlencode(&key)));
                    let label = match next {
                        Some(ref id) => format!(
                            "{} now uses {}",
                            p.profile.name,
                            self.proxy_name(&p.node_name, id)
                        ),
                        None => format!("{} now connects directly", p.profile.name),
                    };
                    let req = self.request(reqwest::Method::PATCH, &url).json(
                        &serde_json::json!({ "proxy": next.unwrap_or_else(|| "none".into()) }),
                    );
                    self.act(&label, req).await;
                }
            }
            (Tab::Profiles, KeyCode::Char('d') | KeyCode::Delete) => {
                if let Some(p) = self.profile() {
                    self.pending = Some(Pending::DeleteProfile(p));
                }
            }
            (Tab::Recordings, KeyCode::Char('d') | KeyCode::Delete) => {
                if let Some(r) = self.recording() {
                    self.pending = Some(Pending::DeleteRecording(r));
                }
            }
            (Tab::Recordings, KeyCode::Enter | KeyCode::Char('o')) => {
                if let Some(r) = self.recording() {
                    let url = format!(
                        "{}/?play={}",
                        r.node_url.trim_end_matches('/'),
                        r.recording.id
                    );
                    self.open(&url);
                }
            }
            _ => {}
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

    async fn confirm(&mut self, pending: Pending) {
        match pending {
            Pending::StopSession(s) => {
                let url = self.api(&s.node_name, &format!("/sessions/{}", s.session.id));
                let req = self.request(reqwest::Method::DELETE, &url);
                self.act("Stopping session", req).await;
            }
            Pending::DeleteProfile(p) => {
                let key = p
                    .profile
                    .id
                    .map_or_else(|| p.profile.name.clone(), |id| id.to_string());
                let mut url = self.api(&p.node_name, &format!("/profiles/{}", urlencode(&key)));
                if p.profile.is_active {
                    url.push_str("?force=true");
                }
                let req = self.request(reqwest::Method::DELETE, &url);
                self.act(&format!("Deleting {}", p.profile.name), req).await;
            }
            Pending::DeleteRecording(r) => {
                let url = self.api(&r.node_name, &format!("/recordings/{}", r.recording.id));
                let req = self.request(reqwest::Method::DELETE, &url);
                self.act("Deleting recording", req).await;
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
            o.sessions.len(),
            o.profiles.len(),
            o.recordings.len(),
            1 + o.peers.len(),
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
        let summary = format!(
            "{}  ·  {}/{} nodes online ",
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

    match app.tab {
        Tab::Sessions => {
            let rows = ov.sessions.iter().map(|s| {
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
                "Profile", "Node", "Engine", "Proxy", "Tabs", "Uptime", "Clients", "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let detail_lines = app.tables[i]
                .selected()
                .and_then(|n| ov.sessions.get(n))
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
            let rows = ov.profiles.iter().map(|p| {
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
                .and_then(|n| ov.profiles.get(n))
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
                    ]
                });
            draw_detail(f, detail, lines);
        }
        Tab::Recordings => {
            let rows = ov.recordings.iter().map(|r| {
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
                    } else {
                        ""
                    })
                    .fg(Color::Red),
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
                .and_then(|n| ov.recordings.get(n))
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
    }

    draw_footer(f, app, foot);

    if let Some(ref pending) = app.pending {
        popup(
            f,
            "Confirm",
            vec![
                Line::from(pending.question()),
                Line::from(""),
                Line::from("y: yes   any other key: no").dim(),
            ],
        );
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
        Tab::Sessions => "⏎ take over  s stop  R record  n new  w ws url",
        Tab::Profiles => "⏎ launch/take over  p proxy  d delete",
        Tab::Recordings => "⏎ play  d delete",
        Tab::Nodes => "n new session there",
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
        ("Tab / ← →  1-4", "switch view"),
        ("↑ ↓  j k  g G", "move"),
        ("r", "refresh now"),
        ("", ""),
        ("Sessions", ""),
        ("  ⏎ / o", "open the handoff view in your browser"),
        ("  s", "stop the session"),
        ("  R", "start or stop recording"),
        ("  n", "start a throwaway session on that node"),
        ("  w", "show the session's WebSocket URL"),
        ("Profiles", ""),
        ("  ⏎ / o", "launch it, or take over its session"),
        (
            "  p",
            "cycle its proxy (direct, then each proxy of its node)",
        ),
        ("  d", "delete it"),
        ("Recordings", ""),
        ("  ⏎ / o", "play it in the dashboard"),
        ("  d", "delete it"),
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

/// The secret: from a file when given or readable, else the one the dashboard
/// at `base` hands to whoever can open it.
async fn find_secret(http: &reqwest::Client, base: &str, file: Option<&PathBuf>) -> Option<String> {
    let path = file
        .cloned()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SECRET_FILE));
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    let html = http
        .get(format!("{base}/"))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let rest = html.split("window.__CLUSTER_SECRET__ = ").nth(1)?;
    let literal = rest.split(';').next()?;
    serde_json::from_str::<Option<String>>(literal)
        .ok()
        .flatten()
}

pub async fn run(args: TuiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let base = args.url.trim_end_matches('/').to_string();
    let secret = find_secret(&http, &base, args.secret_file.as_ref()).await;
    let mut app = App {
        http,
        base,
        secret,
        ov: None,
        tab: Tab::Sessions,
        tables: Default::default(),
        status: String::new(),
        error: false,
        pending: None,
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
