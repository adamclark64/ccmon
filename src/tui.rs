use crate::{decode_project, format_age, Entry, Kind};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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
use std::path::Path;
use std::time::{Duration, Instant};

pub fn run(entries: Vec<Entry>, max_age_hours: u64) -> Result<Option<Entry>> {
    if entries.is_empty() {
        return Ok(None);
    }
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(entries, max_age_hours);
    let result = event_loop(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result.map(|picked| if picked { app.selected_entry().cloned() } else { None })
}

struct App {
    entries: Vec<Entry>,
    visible: Vec<usize>,
    state: ListState,
    filter: String,
    max_age_hours: u64,
    preview: Vec<Line<'static>>,
    preview_for: Option<(usize, Instant)>,
}

impl App {
    fn new(entries: Vec<Entry>, max_age_hours: u64) -> Self {
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
            max_age_hours,
            preview: Vec::new(),
            preview_for: None,
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let row = self.state.selected()?;
        let idx = *self.visible.get(row)?;
        self.entries.get(idx)
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if needle.is_empty() {
                    return true;
                }
                let proj = e
                    .cwd
                    .as_deref()
                    .and_then(|c| Path::new(c).file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| decode_project(&e.project));
                e.summary.to_lowercase().contains(&needle)
                    || proj.to_lowercase().contains(&needle)
                    || e.agent_type
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        if self.visible.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
        self.preview.clear();
        self.preview_for = None;
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

fn draw(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    draw_title(f, root[0], app);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(root[1]);
    draw_list(f, cols[0], app);
    draw_preview(f, cols[1], app);
    draw_footer(f, root[2], app);
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let total = app.entries.len();
    let shown = app.visible.len();
    let mut spans = vec![
        Span::styled("ccmon", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
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
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, area: Rect, _app: &App) {
    let hint = Line::from(vec![
        dim("↑↓"), Span::raw(" move  "),
        dim("enter"), Span::raw(" attach  "),
        dim("type"), Span::raw(" filter  "),
        dim("⌫"), Span::raw(" del  "),
        dim("esc/q"), Span::raw(" quit"),
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
        ("●", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
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
        .and_then(|c| Path::new(c).file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| decode_project(&e.project));
    let title = trunc(&e.summary, 80);
    Line::from(vec![
        Span::styled(format!("{state_glyph} "), state_style),
        Span::styled(format!("{age:>4} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{kind_label:<12} "), kind_style),
        Span::styled(
            format!("{proj:<22} "),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(title, Style::default().fg(Color::Gray)),
    ])
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

fn draw_preview(f: &mut Frame, area: Rect, app: &App) {
    let title = match app.selected_entry() {
        Some(e) => {
            let proj = e
                .cwd
                .as_deref()
                .and_then(|c| Path::new(c).file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| decode_project(&e.project));
            format!(" {} · {} ", proj, format_age(e.mtime))
        }
        None => " preview ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let body = if app.preview.is_empty() {
        vec![Line::from(Span::styled(
            "(no transcript yet)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.preview.clone()
    };
    let para = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
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
    let stale = app
        .preview_for
        .map(|(i, t)| i != idx || t.elapsed() > Duration::from_millis(750))
        .unwrap_or(true);
    if !stale {
        return;
    }
    let path = app.entries[idx].path.clone();
    app.preview = load_preview(&path, 120);
    app.preview_for = Some((idx, Instant::now()));
}

fn load_preview(path: &Path, tail: usize) -> Vec<Line<'static>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<String> = BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .collect();
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
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
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
                                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
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
    let max = 200;
    if out.len() > max {
        out.drain(0..out.len() - max);
    }
    out
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<bool> {
    loop {
        ensure_preview(app);
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Esc, _) => return Ok(false),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(false),
                    (KeyCode::Char('q'), m) if !m.contains(KeyModifiers::CONTROL) && app.filter.is_empty() => {
                        return Ok(false)
                    }
                    (KeyCode::Enter, _) if app.selected_entry().is_some() => return Ok(true),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        app.move_selection(1)
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        app.move_selection(-1)
                    }
                    (KeyCode::PageDown, _) => app.move_selection(10),
                    (KeyCode::PageUp, _) => app.move_selection(-10),
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
