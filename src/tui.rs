use crate::{decode_project, discover, format_age, Entry, Kind};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::fs::File;
use std::io::{stdout, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the agent list is re-scanned from disk while the TUI is open, so
/// newly-started agents appear without restarting ccmon.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub fn run(entries: Vec<Entry>, root: PathBuf, max_age_hours: u64) -> Result<Option<Entry>> {
    if entries.is_empty() {
        return Ok(None);
    }
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(entries, root, max_age_hours);
    let result = event_loop(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    result.map(|picked| {
        if picked {
            app.selected_entry().cloned()
        } else {
            None
        }
    })
}

struct App {
    entries: Vec<Entry>,
    visible: Vec<usize>,
    state: ListState,
    filter: String,
    root: PathBuf,
    last_refresh: Instant,
    max_age_hours: u64,
    preview: Vec<Line<'static>>,
    preview_for: Option<(usize, Instant)>,
    preview_scroll: u16,
    preview_stuck_bottom: bool,
    preview_viewport_h: u16,
    list_area: Rect,
    preview_area: Rect,
}

impl App {
    fn new(entries: Vec<Entry>, root: PathBuf, max_age_hours: u64) -> Self {
        let visible: Vec<usize> = (0..entries.len()).collect();
        let mut state = ListState::default();
        if !visible.is_empty() {
            state.select(Some(0));
        }
        Self {
            entries,
            visible,
            state,
            filter: String::new(),
            root,
            last_refresh: Instant::now(),
            max_age_hours,
            preview: Vec::new(),
            preview_for: None,
            preview_scroll: 0,
            preview_stuck_bottom: true,
            preview_viewport_h: 0,
            list_area: Rect::default(),
            preview_area: Rect::default(),
        }
    }

    fn scroll_preview(&mut self, delta: i32) {
        let viewport = self.preview_viewport_h.max(1) as i32;
        let total = self.preview.len() as i32;
        let max_offset = (total - viewport).max(0);
        let next = (self.preview_scroll as i32 + delta).clamp(0, max_offset);
        self.preview_scroll = next as u16;
        self.preview_stuck_bottom = next >= max_offset;
    }

    fn stick_preview_to_bottom(&mut self) {
        let viewport = self.preview_viewport_h.max(1) as i32;
        let total = self.preview.len() as i32;
        let max_offset = (total - viewport).max(0);
        self.preview_scroll = max_offset as u16;
        self.preview_stuck_bottom = true;
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let row = self.state.selected()?;
        let idx = *self.visible.get(row)?;
        self.entries.get(idx)
    }

    /// Indices into `entries` that match the current filter, preserving order.
    fn compute_visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if needle.is_empty() {
                    return true;
                }
                let proj = e
                    .cwd
                    .as_deref()
                    .and_then(|c| {
                        Path::new(c)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                    })
                    .unwrap_or_else(|| decode_project(&e.project));
                let matches = |s: &Option<String>| {
                    s.as_deref()
                        .map(|t| t.to_lowercase().contains(&needle))
                        .unwrap_or(false)
                };
                e.summary.to_lowercase().contains(&needle)
                    || proj.to_lowercase().contains(&needle)
                    || matches(&e.agent_type)
                    || matches(&e.agent_name)
                    || matches(&e.team_name)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn refilter(&mut self) {
        self.visible = self.compute_visible();
        if self.visible.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
        self.preview.clear();
        self.preview_for = None;
        self.preview_scroll = 0;
        self.preview_stuck_bottom = true;
    }

    /// Re-scan `~/.claude/projects` and merge the result in, keeping the user's
    /// place: the same agent stays selected, the filter is reapplied, and the
    /// preview's scroll position is preserved. Called on an interval so agents
    /// started after ccmon launched still show up.
    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let Ok(entries) = discover(&self.root, self.max_age_hours) else {
            return;
        };
        // Remember what's selected (by stable identity) so we can re-find it
        // after the list is rebuilt and possibly reordered.
        let selected = self.selected_entry().map(|e| (e.kind, e.id.clone()));
        self.entries = entries;
        self.visible = self.compute_visible();
        let new_row = selected.and_then(|(kind, id)| {
            self.visible
                .iter()
                .position(|&i| self.entries[i].kind == kind && self.entries[i].id == id)
        });
        match new_row {
            Some(row) => {
                self.state.select(Some(row));
                // Point preview_for at the selection's new index so ensure_preview
                // doesn't treat the refresh as a selection change and reset scroll.
                if let Some(&idx) = self.visible.get(row) {
                    if let Some((_, t)) = self.preview_for {
                        self.preview_for = Some((idx, t));
                    }
                }
            }
            None => {
                // Previously-selected agent is gone (or filtered out): fall back
                // to the first row and let the preview reload.
                self.state.select(if self.visible.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.preview_for = None;
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        let max = self.visible.len() as isize;
        let next = (cur + delta).rem_euclid(max);
        self.state.select(Some(next as usize));
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    draw_title(f, root[0], app);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(root[1]);
    app.list_area = cols[0];
    app.preview_area = cols[1];
    draw_list(f, cols[0], app);
    draw_preview(f, cols[1], app);
    draw_footer(f, root[2], app);
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let total = app.entries.len();
    let shown = app.visible.len();
    let mut spans = vec![
        Span::styled(
            "ccmon",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{shown}/{total} agents"),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(
            format!("last {}h", app.max_age_hours),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if !app.filter.is_empty() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("/", Style::default().fg(Color::Yellow)));
        spans.push(Span::styled(
            app.filter.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, area: Rect, _app: &App) {
    let hint = Line::from(vec![
        dim("↑↓"),
        Span::raw(" move  "),
        dim("enter"),
        Span::raw(" attach  "),
        dim("PgUp/Dn"),
        Span::raw(" scroll  "),
        dim("End"),
        Span::raw(" follow  "),
        dim("type"),
        Span::raw(" filter  "),
        dim("esc/q"),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(hint), area);
}

fn dim(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::default().fg(Color::DarkGray))
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| ListItem::new(render_row(&app.entries[i])))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(" agents ", Style::default().fg(Color::Cyan))),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 44, 52))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut s = app.state.clone();
    f.render_stateful_widget(list, area, &mut s);
}

fn render_row(e: &Entry) -> Line<'static> {
    let (state_glyph, state_style) = if e.tmux_session.is_some() {
        (
            "●",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else if e.running {
        ("○", Style::default().fg(Color::Yellow))
    } else {
        ("·", Style::default().fg(Color::DarkGray))
    };
    let kind_label = match e.kind {
        Kind::Agent => "agent".to_string(),
        Kind::Subagent => match e.agent_type.as_deref() {
            None | Some("general-purpose") => "sub".to_string(),
            Some(t) => {
                let short = t.rsplit(':').next().unwrap_or(t);
                let short = short.trim_end_matches("-purpose");
                format!("sub:{}", &short[..short.len().min(10)])
            }
        },
    };
    let kind_style = match e.kind {
        Kind::Agent => Style::default().fg(Color::Magenta),
        Kind::Subagent => Style::default().fg(Color::Blue),
    };
    let age = format_age(e.mtime);
    let proj = e
        .cwd
        .as_deref()
        .and_then(|c| {
            Path::new(c)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| decode_project(&e.project));
    let title = trunc(&e.summary, 80);
    let mut spans = vec![
        Span::styled(format!("{state_glyph} "), state_style),
        Span::styled(format!("{age:>4} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{kind_label:<12} "), kind_style),
        Span::styled(
            format!("{proj:<22} "),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // Swarm teammates all share a project/transcript-summary, so lead with the
    // teammate name to make each row identifiable (e.g. "probe-two").
    if let Some(name) = e.agent_name.as_deref() {
        spans.push(Span::styled(
            format!("{name} "),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(title, Style::default().fg(Color::Gray)));
    Line::from(spans)
}

fn trunc(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn draw_preview(f: &mut Frame, area: Rect, app: &mut App) {
    // Record inner viewport height (area minus the 2 border lines) so
    // scroll handlers can clamp + auto-stick correctly.
    app.preview_viewport_h = area.height.saturating_sub(2);
    if app.preview_stuck_bottom {
        app.stick_preview_to_bottom();
    }

    let base_title = match app.selected_entry() {
        Some(e) => {
            let proj = e
                .cwd
                .as_deref()
                .and_then(|c| {
                    Path::new(c)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| decode_project(&e.project));
            format!(" {} · {} ", proj, format_age(e.mtime))
        }
        None => " preview ".to_string(),
    };
    let indicator = if app.preview_stuck_bottom {
        Span::styled(" ▾ live ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" ▴ scrolled ", Style::default().fg(Color::Yellow))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::styled(base_title, Style::default().fg(Color::Cyan)),
            indicator,
        ]));
    let body = if app.preview.is_empty() {
        vec![Line::from(Span::styled(
            "(no transcript yet)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.preview.clone()
    };
    let para = Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll, 0));
    f.render_widget(para, area);
}

fn ensure_preview(app: &mut App) {
    let row = match app.state.selected() {
        Some(r) => r,
        None => {
            app.preview.clear();
            app.preview_for = None;
            return;
        }
    };
    let idx = match app.visible.get(row) {
        Some(i) => *i,
        None => return,
    };
    let selection_changed = app.preview_for.map(|(i, _)| i != idx).unwrap_or(true);
    let stale = app
        .preview_for
        .map(|(_, t)| t.elapsed() > Duration::from_millis(750))
        .unwrap_or(true);
    if !selection_changed && !stale {
        return;
    }
    if selection_changed {
        app.preview_scroll = 0;
        app.preview_stuck_bottom = true;
    }
    let path = app.entries[idx].path.clone();
    app.preview = load_preview(&path, 2000);
    app.preview_for = Some((idx, Instant::now()));
    if app.preview_stuck_bottom {
        app.stick_preview_to_bottom();
    }
}

fn load_preview(path: &Path, tail: usize) -> Vec<Line<'static>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
    let start = lines.len().saturating_sub(tail);
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in &lines[start..] {
        let v: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let tag = if typ == "user" { "u" } else { "a" };
        let tag_style = if typ == "user" {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        };
        let content = v.get("message").and_then(|m| m.get("content"));
        match content {
            Some(serde_json::Value::String(s)) => {
                out.push(Line::from(vec![
                    Span::styled(format!("[{tag}] "), tag_style),
                    Span::raw(trunc(s, 400)),
                ]));
            }
            Some(serde_json::Value::Array(arr)) => {
                for item in arr {
                    let itype = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match itype {
                        "text" => {
                            let t = item.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            if !t.is_empty() {
                                out.push(Line::from(vec![
                                    Span::styled(format!("[{tag}] "), tag_style),
                                    Span::raw(trunc(t, 400)),
                                ]));
                            }
                        }
                        "tool_use" => {
                            let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                            let input = item
                                .get("input")
                                .map(|i| trunc(&i.to_string(), 160))
                                .unwrap_or_default();
                            out.push(Line::from(vec![
                                Span::styled(format!("[{tag}] "), tag_style),
                                Span::styled("→ ", Style::default().fg(Color::Green)),
                                Span::styled(
                                    name.to_string(),
                                    Style::default()
                                        .fg(Color::Green)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!("({input})"),
                                    Style::default().fg(Color::DarkGray),
                                ),
                            ]));
                        }
                        "tool_result" => {
                            let c = item
                                .get("content")
                                .map(|x| trunc(&x.to_string(), 240))
                                .unwrap_or_default();
                            out.push(Line::from(vec![
                                Span::styled(format!("[{tag}] "), tag_style),
                                Span::styled("← ", Style::default().fg(Color::Yellow)),
                                Span::styled(c, Style::default().fg(Color::DarkGray)),
                            ]));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<bool> {
    loop {
        if app.last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh();
        }
        ensure_preview(app);
        terminal.draw(|f| draw(f, app))?;
        let preview_step = (app.preview_viewport_h / 2).max(1) as i32;
        if event::poll(Duration::from_millis(500))? {
            let evt = event::read()?;
            if let Event::Mouse(me) = evt {
                let col = me.column;
                let row = me.row;
                let in_preview = app.preview_area.width > 0
                    && col >= app.preview_area.x
                    && col < app.preview_area.x + app.preview_area.width;
                let in_list = app.list_area.width > 0
                    && col >= app.list_area.x
                    && col < app.list_area.x + app.list_area.width;
                match me.kind {
                    MouseEventKind::ScrollDown if in_preview => app.scroll_preview(3),
                    MouseEventKind::ScrollUp if in_preview => app.scroll_preview(-3),
                    MouseEventKind::ScrollDown if in_list => app.move_selection(1),
                    MouseEventKind::ScrollUp if in_list => app.move_selection(-1),
                    MouseEventKind::Down(MouseButton::Left)
                        if in_list
                            && row > app.list_area.y
                            && row < app.list_area.y + app.list_area.height - 1 =>
                    {
                        let target = (row - app.list_area.y - 1) as usize;
                        if target < app.visible.len() {
                            app.state.select(Some(target));
                            app.preview_scroll = 0;
                            app.preview_stuck_bottom = true;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(k) = evt {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Esc, _) => return Ok(false),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(false),
                    (KeyCode::Char('q'), m)
                        if !m.contains(KeyModifiers::CONTROL) && app.filter.is_empty() =>
                    {
                        return Ok(false)
                    }
                    (KeyCode::Enter, _) if app.selected_entry().is_some() => return Ok(true),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        app.move_selection(1)
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        app.move_selection(-1)
                    }
                    (KeyCode::PageDown, _) => app.scroll_preview(preview_step),
                    (KeyCode::PageUp, _) => app.scroll_preview(-preview_step),
                    (KeyCode::End, _) => app.stick_preview_to_bottom(),
                    (KeyCode::Home, _) => app.scroll_preview(i32::MIN / 2),
                    (KeyCode::Backspace, _) => {
                        app.filter.pop();
                        app.refilter();
                    }
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        app.filter.push(c);
                        app.refilter();
                    }
                    _ => {}
                }
            }
        }
    }
}
