//! Terminal rendering.

use super::*;

pub(super) fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, detail, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(6),
        Constraint::Length(2),
    ])
    .areas(f.area());
    // Sessions: the list is as tall as it needs, the selected session gets the rest.
    let (body, detail) = if app.tab == Tab::Sessions {
        let main = Rect {
            height: body.height + detail.height,
            ..body
        };
        let rows = app.sessions().len() as u16 + 2;
        let list = rows
            .clamp(4, (main.height * 2 / 5).max(4))
            .min(main.height.saturating_sub(8))
            .max(3);
        let list_area = Rect {
            height: list,
            ..main
        };
        let card = Rect {
            y: main.y + list,
            height: main.height - list,
            ..main
        };
        (list_area, card)
    } else {
        (body, detail)
    };

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
                Span::raw(glyph(t.glyph())),
                Span::raw(t.title()),
                Span::raw(n).dim(),
            ])
        })
        .collect();
    let selected = TABS.iter().position(|t| *t == app.tab).unwrap_or(0);
    // Where each title sits (one space of padding either side, one between), for clicks.
    let mut x = 0u16;
    app.tab_spans = titles
        .iter()
        .map(|t| {
            let span = (x, x + t.width() as u16 + 2);
            x = span.1 + 1;
            span
        })
        .collect();
    app.body = body;
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(Style::new().fg(Color::Rgb(125, 130, 145)))
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
            "{only}{}{}  ·  {}/{} nodes online ",
            glyph('\u{f233}'),
            ov.local_node.name,
            ov.online_nodes,
            ov.total_nodes
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
            let icon_col = Constraint::Length(if app.picker.is_some() { 3 } else { 0 });
            let rows = sessions.iter().map(|s| {
                let ss = &s.session;
                let rec = if ss.is_recording {
                    if ss.recording_paused {
                        "paused"
                    } else {
                        "\u{f111} rec"
                    }
                } else {
                    ""
                };
                Row::new(vec![
                    Cell::from(""),
                    Cell::from(ss.name.clone()),
                    Cell::from(ss.profile.clone().unwrap_or_else(|| "(throwaway)".into())),
                    node_cell(&s.node_name),
                    Cell::from(engine_span(ss.engine)),
                    Cell::from(
                        ss.proxy
                            .as_deref()
                            .map(|p| app.proxy_name(&s.node_name, p))
                            .unwrap_or_default(),
                    ),
                    Cell::from(ss.tabs_count.to_string()),
                    Cell::from(dur(ss.uptime_seconds)),
                    Cell::from(ss.viewer_count.to_string()),
                    Cell::from(ss.client_count.to_string()),
                    Cell::from(crate::ctl::agents(&ss.clients)),
                    Cell::from(rec).fg(Color::Red),
                ])
            });
            let table = Table::new(
                rows,
                [
                    icon_col,
                    Constraint::Fill(2),
                    Constraint::Fill(3),
                    Constraint::Length(9),
                    Constraint::Length(11),
                    Constraint::Fill(2),
                    Constraint::Length(5),
                    Constraint::Length(9),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Fill(3),
                    Constraint::Length(7),
                ],
            )
            .header(header(&[
                "", "Name", "Profile", "Node", "Engine", "Proxy", "Tabs", "Uptime", "Viewers",
                "Clients", "Agent", "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let keys: Vec<Option<String>> = sessions
                .iter()
                .map(|x| {
                    let name = x.session.profile.as_deref()?;
                    ov.profiles
                        .iter()
                        .find(|p| {
                            p.node_name == x.node_name
                                && p.profile.name == name
                                && p.profile.engine == x.session.engine
                        })
                        .and_then(icon_key)
                })
                .collect();
            draw_row_icons(f, app, body, i, &keys);
            if let Some(sel) = app.tables[i].selected().and_then(|n| sessions.get(n)) {
                draw_session(f, app, &ov, sel, detail);
            }
        }
        Tab::Profiles => {
            let icon_col = Constraint::Length(if app.picker.is_some() { 3 } else { 0 });
            let rows = profiles.iter().map(|p| {
                let pp = &p.profile;
                Row::new(vec![
                    Cell::from(""),
                    Cell::from(pp.name.clone()),
                    node_cell(&p.node_name),
                    Cell::from(engine_span(pp.engine)),
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
                    icon_col,
                    Constraint::Fill(3),
                    Constraint::Length(9),
                    Constraint::Length(11),
                    Constraint::Fill(2),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(8),
                ],
            )
            .header(header(&[
                "", "Profile", "Node", "Engine", "Proxy", "Size", "Used", "",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let keys: Vec<Option<String>> = profiles.iter().map(icon_key).collect();
            draw_row_icons(f, app, body, i, &keys);
            let lines = app.tables[i]
                .selected()
                .and_then(|n| profiles.get(n))
                .map(|p| {
                    vec![
                        Line::from(vec![
                            label("id"),
                            Span::raw(p.profile.id.map(|i| i.to_string()).unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            label("storage"),
                            Span::raw(p.profile.storage_dir.clone().unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            label("tabs"),
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
                    node_cell(&r.node_name),
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
                        Line::from(vec![label("id"), Span::raw(r.recording.id.to_string())]),
                        Line::from(vec![
                            label("frames"),
                            Span::raw(r.recording.frame_count.to_string()),
                        ]),
                        Line::from(vec![
                            label("from"),
                            Span::raw(r.recording.initial_url.clone().unwrap_or_default()),
                        ]),
                        Line::from(vec![
                            label("to"),
                            Span::raw(r.recording.final_url.clone().unwrap_or_default()),
                        ]),
                    ]
                });
            draw_detail(f, detail, lines);
        }
        Tab::Nodes => {
            let mut rows = vec![Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::raw(ov.local_node.name.clone()).fg(node_color(&ov.local_node.name)),
                    Span::raw(" (local)").dim(),
                ])),
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
                    node_cell(&p.name),
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
                                Span::raw("  "),
                                Span::raw(glyph('\u{f0ac}')),
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
                    Cell::from(engine_span(r.engine)),
                    Cell::from(tri_label(r.defaults.adblock)),
                    Cell::from(scheme_label(r.defaults.color_scheme)),
                    Cell::from(if r.defaults.show_touches { "on" } else { "off" }),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(12),
                    Constraint::Length(14),
                ],
            )
            .header(header(&[
                "Engine",
                "Ad blocker",
                "Appearance",
                "Show touches",
            ]))
            .row_highlight_style(highlight)
            .block(block);
            f.render_stateful_widget(table, body, &mut app.tables[i]);
            let lines = app.tables[i].selected().and_then(|n| rows_data.get(n)).map(|r| {
                vec![Line::from(vec![
                    Span::raw("New ").dim(),
                    Span::raw(r.engine.to_string()).bold(),
                    Span::raw(" sessions start with these unless the request or the profile says otherwise; a change applies to every online node.").dim(),
                ])]
            });
            draw_detail(f, detail, lines);
        }
    }

    draw_footer(f, app, foot);

    app.screen = f.area();
    app.popup = if let Some(ref modal) = app.modal {
        let rect = draw_modal(f, modal);
        if let Modal::Text { shot: Some(id), .. } = modal {
            let area = Rect {
                x: rect.x + 1,
                y: rect.y + 1,
                width: rect.width.saturating_sub(2),
                height: SHOT_ROWS.min(f.area().height.saturating_sub(6) as usize / 2) as u16,
            };
            match app.shots.get_mut(id) {
                Some(shot) => f.render_stateful_widget(
                    StatefulImage::default().resize(Resize::Fit(None)),
                    area,
                    shot,
                ),
                None if app.picker.is_some() => {
                    f.render_widget(Paragraph::new("Taking a screenshot…").dim(), area)
                }
                None => {}
            }
        }
        Some(rect)
    } else if app.help {
        Some(popup(f, "Keys", help_lines()))
    } else {
        None
    };
}

/// A dim label padded so the values under it line up.
fn label(l: &str) -> Span<'static> {
    Span::raw(format!("{l:<9}")).dim()
}

/// `label  value` pairs of the session card, two to a line when there is room.
fn kv(label: &str, value: Line<'static>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(format!("{label:<9}")).dim()];
    spans.extend(value.spans);
    spans
}

fn pairs(rows: Vec<Vec<Span<'static>>>, wide: bool) -> Vec<Line<'static>> {
    const COL: usize = 34;
    if !wide {
        return rows.into_iter().map(Line::from).collect();
    }
    rows.chunks(2)
        .map(|c| {
            let mut left = c[0].clone();
            let used: usize = left.iter().map(Span::width).sum();
            if let Some(right) = c.get(1) {
                left.push(Span::raw(" ".repeat(COL.saturating_sub(used).max(2))));
                left.extend(right.clone());
            }
            Line::from(left)
        })
        .collect()
}

fn chip(text: String, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::raw(format!(" {text} ")).fg(Color::Black).bg(color),
        Span::raw(" "),
    ]
}

/// The selected session: facts and tabs on the left, its screenshot as large as fits on
/// the right (or above, in a narrow window).
fn draw_session(f: &mut Frame, app: &mut App, ov: &MeshOverview, s: &MeshSession, area: Rect) {
    let ss = &s.session;
    let (engine_glyph, engine_color) = if ss.engine == BrowserEngine::Firefox {
        ('\u{f269}', Color::Rgb(255, 113, 57))
    } else {
        ('\u{f268}', Color::Rgb(66, 133, 244))
    };
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().dark_gray())
        .title(Line::from(vec![
            Span::raw(" "),
            Span::raw(glyph(engine_glyph)).fg(engine_color),
            Span::raw(ss.name.clone()).bold(),
            Span::raw(" "),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 20 {
        return;
    }

    // Where the screenshot goes.
    let (cw, ch) = app.picker.as_ref().map_or_else(cell_px, |p| {
        let f = p.font_size();
        (f.width, f.height)
    });
    let (cw, ch) = (u32::from(cw.max(1)), u32::from(ch.max(1)));
    let shot_area = |room: Rect, max_w: u16| -> Rect {
        // 16:10 at the largest size that fits the room.
        let by_height = u32::from(room.height) * ch * 16 / 10 / cw;
        let cols = by_height.min(u32::from(max_w)).min(u32::from(room.width)) as u16;
        let rows = (u32::from(cols) * cw * 10 / 16 / ch).min(u32::from(room.height)) as u16;
        Rect {
            x: room.x + room.width - cols,
            y: room.y,
            width: cols,
            height: rows,
        }
    };
    let (info, shot) = if app.picker.is_none() {
        (inner, None)
    } else if inner.width >= 100 {
        let shot = shot_area(
            Rect {
                x: inner.x + 1,
                width: inner.width - 1,
                ..inner
            },
            inner.width * 3 / 5,
        );
        let info = Rect {
            width: inner.width.saturating_sub(shot.width + 2),
            ..inner
        };
        (info, Some(shot))
    } else {
        let top = Rect {
            height: inner.height / 2,
            ..inner
        };
        let shot = shot_area(top, inner.width);
        let info = Rect {
            y: inner.y + top.height + 1,
            height: inner.height.saturating_sub(top.height + 1),
            ..inner
        };
        (info, Some(shot))
    };

    // Chips: what is going on with it.
    let mut chips: Vec<Span> = Vec::new();
    if ss.is_recording {
        let (text, color) = if ss.recording_paused {
            ("\u{f04c} paused", Color::Yellow)
        } else {
            ("\u{f111} recording", Color::Red)
        };
        chips.extend(chip(text.into(), color));
    }
    if ss.audible {
        chips.extend(chip("\u{f028} sound".into(), Color::Cyan));
    }
    if ss.adblock {
        chips.extend(chip("\u{f132} ads blocked".into(), Color::Green));
    }
    if ss.persistent && ss.profile.is_some() {
        chips.extend(chip("\u{f0c7} saved".into(), Color::Gray));
    }
    let chips: Vec<Span> = chips
        .into_iter()
        .map(|c| {
            if icons() {
                c
            } else {
                Span::styled(c.content.replace(|ch: char| ch > '\u{2000}', ""), c.style)
            }
        })
        .collect();

    let proxy = ss
        .proxy
        .as_deref()
        .map(|p| app.proxy_name(&s.node_name, p))
        .unwrap_or_else(|| "direct".into());
    let wide = info.width >= 70;
    let facts = vec![
        kv(
            "Profile",
            Line::from(ss.profile.clone().unwrap_or_else(|| "(throwaway)".into())),
        ),
        kv(
            "Node",
            Line::from(Span::raw(s.node_name.clone()).fg(node_color(&s.node_name))),
        ),
        kv("Engine", engine_span(ss.engine)),
        kv("Proxy", Line::from(proxy)),
        kv("Uptime", Line::from(dur(ss.uptime_seconds))),
        kv("Idle", Line::from(dur(ss.idle_seconds))),
        kv(
            "Window",
            Line::from(
                ss.window_size
                    .map(|(w, h)| format!("{w}×{h}"))
                    .unwrap_or_else(|| "–".into()),
            ),
        ),
        kv("Viewers", Line::from(ss.viewer_count.to_string())),
        kv("Clients", Line::from(crate::ctl::agents(&ss.clients))),
    ];
    let mut lines: Vec<Line> = Vec::new();
    if !chips.is_empty() {
        lines.push(Line::from(chips));
        lines.push(Line::from(""));
    }
    lines.extend(pairs(facts, wide));
    lines.push(Line::from(""));
    let tabs: Vec<_> = ov
        .targets
        .iter()
        .filter(|t| {
            t.node_name == s.node_name
                && t.target.session_id == Some(ss.id)
                && t.target.target_type == "page"
        })
        .collect();
    lines.push(Line::from(vec![
        Span::raw(glyph('\u{f2d0}')).dim(),
        Span::raw(format!("Tabs ({})", tabs.len())).bold(),
    ]));
    for t in tabs {
        let title = t.target.title.clone().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(if title.is_empty() {
                "(untitled)".to_string()
            } else {
                title
            }),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(t.target.url.clone()).fg(Color::Blue).dim(),
        ]));
        if let Some(owner) = &t.target.owner {
            lines.push(Line::from(vec![
                Span::raw("  Owner: ").dim(),
                Span::raw(owner.tab_label()).fg(Color::Magenta),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::raw(format!("id {}", ss.id)).dim()));
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            x: info.x + 1,
            width: info.width.saturating_sub(1),
            ..info
        },
    );

    if let Some(area) = shot {
        match app.shots.get_mut(&ss.id.to_string()) {
            Some(shot) => f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Fit(None)),
                area,
                shot,
            ),
            None => f.render_widget(Paragraph::new("Taking a screenshot…").dim(), area),
        }
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
        Tab::Sessions => {
            "⏎ take over  s stop  p proxy  R record  t tab  e rename  N new  X stop all  m menu"
        }
        Tab::Profiles => "⏎ launch  p proxy  b backups  e rename  N new  d delete  m menu",
        Tab::Recordings => "⏎ play (mpv)  o browser  K keep  E export  d delete  v vacuum  m menu",
        Tab::Nodes => "n new session  f filter  a add peer  x remove  m menu",
        Tab::Engines => "a ad blocker  c appearance  t touches  m menu  (defaults of new sessions)",
    };
    let spin = if app.pending > 0 {
        format!("{} ", SPINNER[app.tick % SPINNER.len()])
    } else {
        String::new()
    };
    let status = if app.status.is_empty() {
        Line::from(Span::raw(spin).fg(Color::Cyan))
    } else if app.error {
        Line::from(Span::raw(format!("{spin}{}", app.status)).fg(Color::Red))
    } else {
        Line::from(Span::raw(format!("{spin}{}", app.status)).fg(Color::Cyan))
    };
    let keys = Line::from(vec![
        Span::raw(hints).dim(),
        Span::raw("   ? help  q quit").dim(),
    ]);
    f.render_widget(Paragraph::new(vec![status, keys]), area);
}

fn help_lines() -> Vec<Line<'static>> {
    [
        ("Tab / ← →  1-5", "switch view"),
        ("↑ ↓  j k  g G", "move"),
        (
            "m / Space",
            "menu of the selected row: everything the dashboard offers",
        ),
        ("M", "menu of the view (new session, stop all, ...)"),
        ("f", "show only the selected row's node (again: all nodes)"),
        ("i", "details"),
        (
            "r",
            "refresh now (requests run in the background; the spinner shows while they do)",
        ),
        (
            "mouse",
            "click a tab or row, double-click / right-click: menu, wheel: scroll",
        ),
        ("", ""),
        ("Sessions", ""),
        ("  ⏎ / o", "open the handoff view in your browser"),
        ("  s", "stop the session"),
        ("  X", "stop every session in view"),
        ("  p", "switch its proxy now (direct, then each proxy)"),
        ("  R", "start or stop recording"),
        ("  t / e / T", "new tab / rename / auto-stop timeout"),
        (
            "  n / N",
            "throwaway session on that node / new session form",
        ),
        ("  w", "show the session's WebSocket URL"),
        ("Profiles", ""),
        ("  ⏎ / o", "launch it, or take over its session"),
        ("  p", "cycle its default proxy (from the next launch)"),
        (
            "  b",
            "backups & data: back up, restore, clear, export, import",
        ),
        ("  e / N", "rename / new profile form"),
        ("  d", "delete it"),
        ("Recordings", ""),
        (
            "  ⏎ / p",
            "play it in mpv ($BROWSER_DAEMON_PLAYER), o: in the dashboard",
        ),
        (
            "  K",
            "keep it (the cleanup never removes it) / stop keeping it",
        ),
        ("  E / v", "export its frames / apply the retention limits"),
        ("  d", "delete it"),
        ("Nodes", ""),
        (
            "  n / a / x",
            "session there / add a peer / remove the peer",
        ),
        ("Engines", ""),
        (
            "  a / c / t",
            "cycle ad blocker / appearance, toggle show touches (defaults of new sessions)",
        ),
        ("In a menu or form", ""),
        ("  ⏎  Esc", "choose / cancel   (1-9 pick an item)"),
        ("  ← →  Tab", "change a choice / next field"),
        ("", ""),
        ("q / Ctrl-C", "quit"),
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

/// Draws the modal; the area it covers.
fn draw_modal(f: &mut Frame, modal: &Modal) -> Rect {
    let room = f.area().height.saturating_sub(6) as usize;
    let selected =
        |text: String| Line::from(Span::raw(format!("▸ {text}")).fg(Color::Magenta).bold());
    let hint = |text: &str| Line::from(Span::raw(text.to_string())).dim();
    match modal {
        Modal::Menu { title, items, sel } => {
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .map(|(i, (label, _))| {
                    let n = if i < 9 {
                        format!("{} ", i + 1)
                    } else {
                        "  ".to_string()
                    };
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
            popup(f, title, lines)
        }
        Modal::Input {
            title, prompt, buf, ..
        } => popup(
            f,
            title,
            vec![
                Line::from(prompt.clone()),
                Line::from(vec![
                    Span::raw("› "),
                    Span::raw(buf.clone()),
                    Span::raw("▏"),
                ]),
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
        Modal::Text {
            title,
            lines,
            scroll,
            shot,
        } => {
            // Rows left empty for the screenshot, which draw() paints over them.
            let gap = if shot.is_some() {
                SHOT_ROWS.min(room / 2)
            } else {
                0
            };
            let mut shown: Vec<Line> = std::iter::repeat_n(Line::from(" ".repeat(78)), gap)
                .chain(
                    lines
                        .iter()
                        .skip(*scroll)
                        .take(room.saturating_sub(gap).max(3))
                        .map(|l| Line::from(l.clone())),
                )
                .collect();
            shown.push(Line::from(""));
            shown.push(hint("↑↓ scroll   Esc close"));
            popup(f, title, shown)
        }
        Modal::Form(form) => {
            let mut lines: Vec<Line> = form
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let value = match &field.kind {
                        FieldKind::Text(t) => {
                            if i == form.sel {
                                format!("{t}▏")
                            } else {
                                t.clone()
                            }
                        }
                        FieldKind::Choice { options, sel } => format!(
                            "‹ {} ›",
                            options.get(*sel).map(|o| o.1.as_str()).unwrap_or("–")
                        ),
                    };
                    let text = format!("{:<12} {value}", field.label);
                    if i == form.sel {
                        selected(text)
                    } else {
                        Line::from(format!("  {text}"))
                    }
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(hint("↑↓ field   ← → change   ⏎ create   Esc cancel"));
            let title = match form.kind {
                FormKind::Session => "New session",
                FormKind::Profile => "New profile",
            };
            popup(f, title, lines)
        }
    }
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line<'static>>) -> Rect {
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
    rect
}
