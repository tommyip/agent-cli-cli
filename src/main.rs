use std::{
    cmp::Reverse,
    collections::{HashMap, VecDeque},
    error::Error,
    fs,
    io::{self, IsTerminal, Stdout},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use chrono::{DateTime, Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use rusqlite::Connection;
use serde_json::Value;
use walkdir::WalkDir;

const TITLE_SEARCH_LIMIT: usize = 1_000;
const MESSAGE_SEARCH_LIMIT: usize = 20_000;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        println!("acc {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("acc is interactive and must be run in a terminal.");
        return Ok(());
    }

    let mut sessions = load_sessions();
    sessions.sort_by_key(|session| Reverse(session.updated_at));

    if sessions.is_empty() {
        println!("No Claude Code or Codex sessions found under ~/.claude or ~/.codex.");
        return Ok(());
    }

    let message_rx = spawn_message_indexer(&sessions);
    let terminal = setup_terminal()?;
    let mut app = App::new(sessions, message_rx);
    let action = run_app(terminal, &mut app);
    restore_terminal()?;

    match action? {
        AppAction::Quit => Ok(()),
        AppAction::Launch(index) => launch_session(&app.sessions[index]),
    }
}

fn print_help() {
    println!(
        "\
acc - interactive session switcher for Claude Code and Codex

Usage:
  acc

Controls:
  tab        focus next input
  shift-tab  focus previous input
  space      cycle provider when provider is focused
  up/down    move selection
  enter      launch selected session
  ctrl-1..9  launch visible row by number
  esc        quit
"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Both,
    Claude,
    Codex,
}

impl Provider {
    fn cycle(self) -> Self {
        match self {
            Provider::Both => Provider::Claude,
            Provider::Claude => Provider::Codex,
            Provider::Codex => Provider::Both,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Provider::Both => "both",
            Provider::Claude => "claude code",
            Provider::Codex => "codex",
        }
    }

    fn matches(self, provider: SessionProvider) -> bool {
        match self {
            Provider::Both => true,
            Provider::Claude => provider == SessionProvider::Claude,
            Provider::Codex => provider == SessionProvider::Codex,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionProvider {
    Claude,
    Codex,
}

impl SessionProvider {
    fn as_str(self) -> &'static str {
        match self {
            SessionProvider::Claude => "claude",
            SessionProvider::Codex => "codex",
        }
    }

    fn style(self) -> Style {
        match self {
            SessionProvider::Claude => Style::default().fg(Color::Magenta),
            SessionProvider::Codex => Style::default().fg(Color::Blue),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Provider,
    Title,
    Location,
    Messages,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Provider => Focus::Title,
            Focus::Title => Focus::Location,
            Focus::Location => Focus::Messages,
            Focus::Messages => Focus::Provider,
        }
    }

    fn previous(self) -> Self {
        match self {
            Focus::Provider => Focus::Messages,
            Focus::Title => Focus::Provider,
            Focus::Location => Focus::Title,
            Focus::Messages => Focus::Location,
        }
    }
}

#[derive(Debug)]
struct Session {
    id: String,
    provider: SessionProvider,
    cwd: PathBuf,
    cwd_display: String,
    title: String,
    title_search: String,
    message_search: String,
    message_turns: Vec<ChatTurn>,
    transcript_path: Option<PathBuf>,
    created_at: Option<i64>,
    updated_at: i64,
    tokens: Option<u64>,
}

#[derive(Debug, Clone)]
struct MatchRow {
    index: usize,
    title_score: u32,
    location_score: u32,
    message_score: u32,
}

#[derive(Debug, Clone)]
struct ChatTurn {
    role: ChatRole,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    Assistant,
}

impl ChatRole {
    fn label(self) -> &'static str {
        match self {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

struct App {
    sessions: Vec<Session>,
    provider: Provider,
    focus: Focus,
    title_query: String,
    location_query: String,
    message_query: String,
    selected: usize,
    rows: Vec<MatchRow>,
    indexed_messages: usize,
    message_rx: Receiver<(usize, Vec<ChatTurn>)>,
    error: Option<String>,
}

enum AppAction {
    Quit,
    Launch(usize),
}

impl App {
    fn new(sessions: Vec<Session>, message_rx: Receiver<(usize, Vec<ChatTurn>)>) -> Self {
        let mut app = Self {
            sessions,
            provider: Provider::Both,
            focus: Focus::Title,
            title_query: String::new(),
            location_query: String::new(),
            message_query: String::new(),
            selected: 0,
            rows: Vec::new(),
            indexed_messages: 0,
            message_rx,
            error: None,
        };
        app.recompute_rows();
        app
    }

    fn drain_message_updates(&mut self, max_updates: usize) -> bool {
        let mut changed = false;
        for _ in 0..max_updates {
            let Ok((index, turns)) = self.message_rx.try_recv() else {
                break;
            };
            if let Some(session) = self.sessions.get_mut(index) {
                session.message_search = limited_search_text(
                    turns
                        .iter()
                        .map(|turn| turn.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    MESSAGE_SEARCH_LIMIT,
                );
                session.message_turns = turns;
                self.indexed_messages += 1;
                changed = true;
            }
        }
        changed
    }

    fn recompute_rows(&mut self) {
        let title_query = self.title_query.trim();
        let location_query = self.location_query.trim();
        let message_query = self.message_query.trim();
        let title_active = !title_query.is_empty();
        let location_active = !location_query.is_empty();
        let message_active = !message_query.is_empty();
        let mut rows = Vec::new();

        for index in 0..self.sessions.len() {
            let session = &self.sessions[index];
            if !self.provider.matches(session.provider) {
                continue;
            }

            let mut title_score = 0;
            let mut location_score = 0;
            let mut message_score = 0;

            if title_active {
                let Some(search_score) = fuzzy_score(&session.title_search, title_query) else {
                    continue;
                };
                title_score = fuzzy_score(&session.title, title_query)
                    .map(|score| score + 10_000)
                    .unwrap_or(search_score);
            }

            if location_active {
                let Some(score) = fuzzy_score(&session.cwd_display, location_query) else {
                    continue;
                };
                location_score = score;
            }

            if message_active {
                let Some(score) = fuzzy_score(&session.message_search, message_query) else {
                    continue;
                };
                message_score = score;
            }

            rows.push(MatchRow {
                index,
                title_score,
                location_score,
                message_score,
            });
        }

        rows.sort_by_key(|row| {
            (
                Reverse(row.title_score),
                Reverse(row.location_score),
                Reverse(row.message_score),
                Reverse(self.sessions[row.index].updated_at),
            )
        });
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.rows.get(self.selected).map(|row| row.index)
    }

    fn visible_index_for_digit(&self, digit: char) -> Option<usize> {
        let number = digit.to_digit(10)? as usize;
        if number == 0 {
            return None;
        }
        self.rows.get(number - 1).map(|row| row.index)
    }

    fn try_launch(&mut self, index: usize) -> Option<AppAction> {
        let session = &self.sessions[index];
        if !session.cwd.is_dir() {
            self.error = Some(format!("Missing cwd: {}", session.cwd_display));
            return None;
        }
        Some(AppAction::Launch(index))
    }
}

fn fuzzy_score(haystack: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }

    let haystack = haystack.as_bytes();
    let query = query.as_bytes();
    let mut query_pos = 0;
    let mut score = 0u32;
    let mut last_match: Option<usize> = None;

    for (index, &byte) in haystack.iter().enumerate() {
        if query_pos == query.len() {
            break;
        }

        if ascii_lower(byte) != ascii_lower(query[query_pos]) {
            continue;
        }

        score += 10;
        if last_match.is_some_and(|previous| previous + 1 == index) {
            score += 15;
        }
        if index == 0 || is_boundary(haystack[index - 1]) {
            score += 8;
        }
        if index < 32 {
            score += 4;
        }

        last_match = Some(index);
        query_pos += 1;
    }

    if query_pos == query.len() {
        Some(score)
    } else {
        None
    }
}

fn ascii_lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

fn is_boundary(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'/' | b':' | b'-' | b'_' | b'.' | b'#'
    )
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn run_app(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<AppAction, Box<dyn Error>> {
    app.drain_message_updates(512);
    terminal.draw(|frame| render(frame, app))?;
    loop {
        if !app.message_query.trim().is_empty() && app.drain_message_updates(8) {
            app.recompute_rows();
            terminal.draw(|frame| render(frame, app))?;
        } else {
            app.drain_message_updates(16);
        }
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != event::KeyEventKind::Press {
            continue;
        }
        if let Some(action) = handle_key(app, key) {
            return Ok(action);
        }
        terminal.draw(|frame| render(frame, app))?;
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    app.error = None;

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(AppAction::Quit)
        }
        KeyCode::Esc => Some(AppAction::Quit),
        KeyCode::Tab => {
            app.focus = app.focus.next();
            None
        }
        KeyCode::BackTab => {
            app.focus = app.focus.previous();
            None
        }
        KeyCode::Up => {
            app.selected = app.selected.saturating_sub(1);
            None
        }
        KeyCode::Down => {
            if app.selected + 1 < app.rows.len() {
                app.selected += 1;
            }
            None
        }
        KeyCode::Enter => app.selected_index().and_then(|index| app.try_launch(index)),
        KeyCode::Char(ch)
            if ch.is_ascii_digit()
                && ch != '0'
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.visible_index_for_digit(ch)
                .and_then(|index| app.try_launch(index))
        }
        KeyCode::Char(' ') if app.focus == Focus::Provider => {
            app.provider = app.provider.cycle();
            app.selected = 0;
            app.drain_message_updates(128);
            app.recompute_rows();
            None
        }
        KeyCode::Backspace => {
            match app.focus {
                Focus::Provider => {}
                Focus::Title => {
                    app.title_query.pop();
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
                Focus::Location => {
                    app.location_query.pop();
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
                Focus::Messages => {
                    app.message_query.pop();
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
            }
            None
        }
        KeyCode::Char(ch) => {
            match app.focus {
                Focus::Provider => {}
                Focus::Title => {
                    app.title_query.push(ch);
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
                Focus::Location => {
                    app.location_query.push(ch);
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
                Focus::Messages => {
                    app.message_query.push(ch);
                    app.selected = 0;
                    app.drain_message_updates(128);
                    app.recompute_rows();
                }
            }
            None
        }
        _ => None,
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_inputs(frame, app, root[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(root[1]);

    render_table(frame, app, body[0]);
    render_preview(frame, app, body[1]);
    render_status(frame, app, root[2]);

    if let Some(error) = &app.error {
        render_error(frame, error);
    }
}

fn render_inputs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::Gray);

    let provider_style = if app.focus == Focus::Provider {
        focused
    } else {
        normal
    };
    let title_style = if app.focus == Focus::Title {
        focused
    } else {
        normal
    };
    let location_style = if app.focus == Focus::Location {
        focused
    } else {
        normal
    };
    let message_style = if app.focus == Focus::Messages {
        focused
    } else {
        normal
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("Provider: ", provider_style),
            Span::styled(app.provider.as_str(), provider_style),
        ]),
        Line::from(vec![
            Span::styled("Title:    ", title_style),
            Span::styled(
                if app.title_query.is_empty() {
                    "..."
                } else {
                    &app.title_query
                },
                title_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("Location: ", location_style),
            Span::styled(
                if app.location_query.is_empty() {
                    "..."
                } else {
                    &app.location_query
                },
                location_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("Messages: ", message_style),
            Span::styled(
                if app.message_query.is_empty() {
                    "..."
                } else {
                    &app.message_query
                },
                message_style,
            ),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title("acc").borders(Borders::ALL)),
        area,
    );
}

fn render_table(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let table_width = area.width.saturating_sub(2);
    let fixed_width = 3 + 7 + 10 + 8;
    let title_width = ((table_width.saturating_sub(fixed_width)) * 45 / 100).max(16);
    let location_width = table_width
        .saturating_sub(fixed_width)
        .saturating_sub(title_width)
        .max(16);

    let rows = app.rows.iter().enumerate().map(|(visible, row)| {
        let session = &app.sessions[row.index];
        let key = if visible < 9 {
            (visible + 1).to_string()
        } else {
            String::new()
        };
        Row::new(vec![
            Cell::from(key).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from(session.provider.as_str()).style(session.provider.style()),
            Cell::from(highlight_title_cell(
                &session.title,
                app.title_query.trim(),
                title_width as usize,
            )),
            Cell::from(highlight_location_cell(
                &session.cwd_display,
                app.location_query.trim(),
                location_width as usize,
            )),
            Cell::from(format_tokens(session.tokens)).style(token_style(session.tokens)),
            Cell::from(format_age(session.updated_at)).style(Style::default().fg(Color::Gray)),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Length(title_width),
            Constraint::Length(location_width),
            Constraint::Length(8),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec![
            "key", "agent", "title", "location", "tokens", "updated",
        ])
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(36, 36, 36))
            .add_modifier(Modifier::BOLD),
    )
    .block(Block::default().title("sessions").borders(Borders::ALL));

    let mut state = TableState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_preview(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(index) = app.selected_index() {
        let session = &app.sessions[index];
        let title_query = app.title_query.trim();
        let location_query = app.location_query.trim();
        let message_query = app.message_query.trim();
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        if title_query.is_empty() {
            lines.push(Line::from(Span::styled(session.title.clone(), title_style)));
        } else {
            lines.push(highlight_preview_query(
                &session.title,
                title_query,
                title_style,
            ));
        }
        lines.push(Line::from(""));
        lines.push(metadata_line(
            "agent",
            session.provider.as_str(),
            session.provider.style(),
        ));
        lines.push(metadata_line(
            "id",
            &session.id,
            Style::default().fg(Color::Gray),
        ));
        lines.push(metadata_line_highlighted(
            "cwd",
            &session.cwd_display,
            Style::default().fg(Color::Gray),
            location_query,
        ));
        lines.push(metadata_line(
            "updated",
            &format_datetime(session.updated_at),
            Style::default().fg(Color::Gray),
        ));
        if let Some(created_at) = session.created_at {
            lines.push(metadata_line(
                "created",
                &format_datetime(created_at),
                Style::default().fg(Color::Gray),
            ));
        }
        if let Some(tokens) = session.tokens {
            lines.push(metadata_line(
                "tokens",
                &format_tokens(Some(tokens)),
                token_style(Some(tokens)),
            ));
        }

        if !message_query.is_empty() {
            lines.push(Line::from(""));
            lines.push(section_line("message context"));
            if let Some(context) = message_match_context(&session.message_turns, message_query) {
                lines.extend(context);
            } else if session.message_search.is_empty() {
                lines.push(Line::from("indexing transcript..."));
            } else {
                lines.push(Line::from("(no context available)"));
            }
        } else {
            lines.push(Line::from(""));
            lines.push(section_line("recent messages"));
            lines.extend(message_tail(&session.message_turns, message_query));
        }
    } else {
        lines.push(Line::from("No matching sessions."));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title("preview").borders(Borders::ALL)),
        area,
    );
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let status = format!(
        "{} sessions | {} indexed | tab focus | space provider | enter launch | ctrl-1..9 quick launch | esc quit",
        app.rows.len(),
        app.indexed_messages
    );
    frame.render_widget(Paragraph::new(status), area);
}

fn render_error(frame: &mut Frame<'_>, message: &str) {
    let area = centered_rect(60, 20, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message.to_owned())
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title("cannot launch")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            ),
        area,
    );
}

fn metadata_line(label: &'static str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_owned(), value_style),
    ])
}

fn metadata_line_highlighted(
    label: &'static str,
    value: &str,
    value_style: Style,
    query: &str,
) -> Line<'static> {
    if query.is_empty() {
        return metadata_line(label, value, value_style);
    }

    let mut spans = vec![Span::styled(
        format!("{label}: "),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend(highlight_preview_query_spans(value, query, value_style));
    Line::from(spans)
}

fn section_line(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        label,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn token_style(tokens: Option<u64>) -> Style {
    match tokens {
        Some(value) if value >= 1_000_000 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn message_match_context(turns: &[ChatTurn], query: &str) -> Option<Vec<Line<'static>>> {
    let (best_index, _) = turns
        .iter()
        .enumerate()
        .filter_map(|(index, turn)| fuzzy_score(&turn.text, query).map(|score| (index, score)))
        .max_by_key(|(_, score)| *score)?;

    let start = best_index.saturating_sub(1);
    let end = (best_index + 2).min(turns.len());
    let mut lines = Vec::new();
    for (index, turn) in turns.iter().enumerate().take(end).skip(start).rev() {
        let prefix = if index == best_index { "> " } else { "  " };
        let base_style = if index == best_index {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        if index == best_index {
            lines.push(highlight_message_line(prefix, turn, 700, query, base_style));
        } else {
            lines.push(highlight_message_line(prefix, turn, 500, query, base_style));
        }
    }
    Some(lines)
}

fn message_tail(turns: &[ChatTurn], query: &str) -> Vec<Line<'static>> {
    let Some(last_user_index) = turns.iter().rposition(|turn| turn.role == ChatRole::User) else {
        return vec![Line::from("indexing transcript...")];
    };

    let start = last_user_index.saturating_sub(3);
    let end = (last_user_index + 1).min(turns.len());
    turns
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .rev()
        .map(|(index, turn)| {
            let prefix = if index == last_user_index { "> " } else { "  " };
            let style = if index == last_user_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            highlight_message_line(prefix, turn, 500, query, style)
        })
        .collect()
}

fn highlight_message_line(
    prefix: &str,
    turn: &ChatTurn,
    max_chars: usize,
    query: &str,
    base_style: Style,
) -> Line<'static> {
    let text = format!(
        "{prefix}{}: {}",
        turn.role.label(),
        compact_line(&turn.text, max_chars)
    );
    if query.is_empty() {
        return Line::from(Span::styled(text, base_style));
    }
    highlight_preview_query(&text, query, base_style)
}

fn highlight_preview_query(text: &str, query: &str, base_style: Style) -> Line<'static> {
    Line::from(highlight_preview_query_spans(text, query, base_style))
}

fn highlight_preview_query_spans(text: &str, query: &str, base_style: Style) -> Vec<Span<'static>> {
    highlight_query_spans(
        text,
        query,
        base_style,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

fn highlight_table_query(text: &str, query: &str) -> Line<'static> {
    highlight_query_with_style(
        text,
        query,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )
}

fn highlight_query_with_style(text: &str, query: &str, match_style: Style) -> Line<'static> {
    Line::from(highlight_query_spans(
        text,
        query,
        Style::default(),
        match_style,
    ))
}

fn highlight_query_spans(
    text: &str,
    query: &str,
    base_style: Style,
    match_style: Style,
) -> Vec<Span<'static>> {
    let mut query_chars = query.chars().filter(|ch| !ch.is_whitespace());
    let mut current = query_chars.next().map(|ch| ch.to_ascii_lowercase());
    let mut spans = Vec::new();
    let mut plain = String::new();

    for ch in text.chars() {
        let is_match = current.is_some_and(|target| ch.to_ascii_lowercase() == target);
        if is_match {
            if !plain.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut plain), base_style));
            }
            spans.push(Span::styled(ch.to_string(), match_style));
            current = query_chars.next().map(|ch| ch.to_ascii_lowercase());
        } else {
            plain.push(ch);
        }
    }

    if !plain.is_empty() {
        spans.push(Span::styled(plain, base_style));
    }
    spans
}

fn highlight_title_cell(title: &str, query: &str, max_chars: usize) -> Line<'static> {
    let title = ellipsize_tail(title, max_chars);
    if query.is_empty() {
        return Line::from(title);
    }
    highlight_table_query(&title, query)
}

fn highlight_location_cell(path: &str, query: &str, max_chars: usize) -> Line<'static> {
    let path = ellipsize_path(path, max_chars);
    if query.is_empty() {
        return Line::from(Span::styled(path, Style::default().fg(Color::Gray)));
    }
    highlight_table_query(&path, query)
}

fn compact_line(text: &str, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    ellipsize_tail(&text, max_chars)
}

fn ellipsize_tail(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let keep = max_chars - 1;
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn ellipsize_path(path: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if path.chars().count() <= max_chars {
        return path.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }

    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut suffix = String::new();
    for part in parts.iter().rev() {
        let candidate = if suffix.is_empty() {
            (*part).to_owned()
        } else {
            format!("{part}/{suffix}")
        };
        if candidate.chars().count() + 2 > max_chars {
            break;
        }
        suffix = candidate;
    }

    if suffix.is_empty() {
        ellipsize_tail(path, max_chars)
    } else {
        format!("…/{suffix}")
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn load_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();
    sessions.extend(load_codex_sessions());
    sessions.extend(load_claude_sessions());
    sessions
}

fn spawn_message_indexer(sessions: &[Session]) -> Receiver<(usize, Vec<ChatTurn>)> {
    let (tx, rx) = mpsc::channel();
    let paths: Vec<(usize, PathBuf)> = sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            session
                .transcript_path
                .as_ref()
                .map(|path| (index, path.clone()))
        })
        .collect();

    thread::spawn(move || {
        for (index, path) in paths {
            let turns = extract_transcript_turns(&path);
            if tx.send((index, turns)).is_err() {
                break;
            }
        }
    });

    rx
}

fn load_codex_sessions() -> Vec<Session> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let db_path = home.join(".codex/state_5.sqlite");
    if !db_path.exists() {
        return Vec::new();
    }

    let Ok(conn) = Connection::open(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "select id, title, cwd, rollout_path, created_at, updated_at, tokens_used, first_user_message
         from threads
         where archived = 0
         order by updated_at desc",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let cwd: String = row.get(2)?;
        let rollout_path: String = row.get(3)?;
        let created_at: Option<i64> = row.get(4)?;
        let updated_at: i64 = row.get(5)?;
        let tokens: Option<i64> = row.get(6)?;
        let first_user_message: String = row.get(7).unwrap_or_default();
        Ok((
            id,
            title,
            cwd,
            rollout_path,
            created_at,
            updated_at,
            tokens.and_then(|value| u64::try_from(value).ok()),
            first_user_message,
        ))
    }) else {
        return Vec::new();
    };

    let mut sessions = Vec::new();
    for row in rows.flatten() {
        let (id, title, cwd, rollout_path, created_at, updated_at, tokens, first_user_message) =
            row;
        let cwd = PathBuf::from(cwd);
        let title = compact_title(&title, &first_user_message);
        let title_search =
            limited_search_text(format!("{title}\n{first_user_message}"), TITLE_SEARCH_LIMIT);
        let transcript_path = PathBuf::from(rollout_path);
        sessions.push(Session {
            id,
            provider: SessionProvider::Codex,
            cwd: cwd.clone(),
            cwd_display: display_path(&cwd),
            title,
            title_search,
            message_search: String::new(),
            message_turns: Vec::new(),
            transcript_path: Some(transcript_path),
            created_at,
            updated_at,
            tokens,
        });
    }
    sessions
}

fn load_claude_sessions() -> Vec<Session> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude/projects");
    if !projects_dir.exists() {
        return Vec::new();
    }

    let index = load_claude_indexes(&projects_dir);
    let mut sessions = Vec::new();

    for entry in WalkDir::new(projects_dir)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
    {
        let path = entry.path().to_path_buf();
        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            continue;
        };

        let meta = index.get(&id).cloned().unwrap_or_default();
        let parsed = parse_claude_head(&path);
        let file_updated_at = file_modified_unix(&path);
        let cwd = meta
            .project_path
            .or(parsed.cwd)
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("")).to_path_buf());
        let updated_at = meta
            .modified
            .or(file_updated_at)
            .or(parsed.updated_at)
            .unwrap_or_default();
        let created_at = meta.created.or(parsed.created_at);
        let raw_title = meta
            .summary
            .or(parsed.slug)
            .or(meta.first_prompt.clone())
            .or(parsed.first_prompt.clone())
            .unwrap_or_else(|| id.clone());
        let first_prompt = meta
            .first_prompt
            .or(parsed.first_prompt)
            .unwrap_or_default();
        let title = compact_title(&raw_title, &first_prompt);
        let title_search =
            limited_search_text(format!("{title}\n{first_prompt}"), TITLE_SEARCH_LIMIT);

        sessions.push(Session {
            id,
            provider: SessionProvider::Claude,
            cwd: cwd.clone(),
            cwd_display: display_path(&cwd),
            title,
            title_search,
            message_search: String::new(),
            message_turns: Vec::new(),
            transcript_path: Some(path),
            created_at,
            updated_at,
            tokens: parsed.tokens,
        });
    }

    sessions
}

#[derive(Clone, Default)]
struct ClaudeIndexEntry {
    summary: Option<String>,
    first_prompt: Option<String>,
    project_path: Option<PathBuf>,
    created: Option<i64>,
    modified: Option<i64>,
}

fn load_claude_indexes(projects_dir: &Path) -> HashMap<String, ClaudeIndexEntry> {
    let mut index = HashMap::new();
    for entry in WalkDir::new(projects_dir)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "sessions-index.json")
    {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(entries) = value.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for item in entries {
            let Some(id) = item.get("sessionId").and_then(Value::as_str) else {
                continue;
            };
            index.insert(
                id.to_owned(),
                ClaudeIndexEntry {
                    summary: item
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    first_prompt: item
                        .get("firstPrompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    project_path: item
                        .get("projectPath")
                        .and_then(Value::as_str)
                        .map(PathBuf::from),
                    created: item
                        .get("created")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp),
                    modified: item
                        .get("modified")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp),
                },
            );
        }
    }
    index
}

#[derive(Default)]
struct ClaudeHead {
    cwd: Option<PathBuf>,
    first_prompt: Option<String>,
    slug: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    tokens: Option<u64>,
}

fn parse_claude_head(path: &Path) -> ClaudeHead {
    let Ok(text) = fs::read_to_string(path) else {
        return ClaudeHead::default();
    };
    let mut head = ClaudeHead::default();
    let mut tokens = 0;

    for line in text.lines().take(200) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if head.cwd.is_none() {
            head.cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        if head.slug.is_none() {
            head.slug = value.get("slug").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(ts) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        {
            head.created_at.get_or_insert(ts);
            head.updated_at = Some(ts);
        }
        tokens += sum_token_fields(&value);

        if head.first_prompt.is_none()
            && value.get("type").and_then(Value::as_str) == Some("user")
            && !value
                .get("isMeta")
                .and_then(Value::as_bool)
                .unwrap_or_default()
        {
            head.first_prompt =
                extract_message_content(&value).map(|text| text.chars().take(500).collect());
        }
    }

    if tokens > 0 {
        head.tokens = Some(tokens);
    }
    head
}

fn extract_transcript_turns(path: &Path) -> Vec<ChatTurn> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut turns = VecDeque::new();
    let mut total_len = 0usize;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if should_skip_transcript_line(&value) {
            continue;
        }
        let Some((role, parts)) = extract_chat_turn_text(&value) else {
            continue;
        };
        for part in parts {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            let turn = compact_line(trimmed, 1_000);
            total_len += turn.len() + 1;
            turns.push_back(ChatTurn { role, text: turn });
            while total_len > MESSAGE_SEARCH_LIMIT {
                let Some(removed) = turns.pop_front() else {
                    break;
                };
                total_len = total_len.saturating_sub(removed.text.len() + 1);
            }
        }
    }
    turns.into_iter().collect()
}

fn extract_chat_turn_text(value: &Value) -> Option<(ChatRole, Vec<String>)> {
    let role = value
        .pointer("/message/role")
        .or_else(|| value.pointer("/payload/role"))
        .and_then(Value::as_str);
    let role = match role {
        Some("user") => ChatRole::User,
        Some("assistant") => ChatRole::Assistant,
        _ => return None,
    };

    let content = value
        .pointer("/message/content")
        .or_else(|| value.pointer("/payload/content"))
        .or_else(|| value.pointer("/payload/message/content"));
    let Some(content) = content else {
        return None;
    };

    Some((role, extract_text_content_items(content)))
}

fn extract_text_content_items(content: &Value) -> Vec<String> {
    match content {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(Value::as_str);
                match item_type {
                    Some("text" | "input_text" | "output_text") | None => item
                        .get("text")
                        .or_else(|| item.get("content"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn should_skip_transcript_line(value: &Value) -> bool {
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "session_meta"
                | "progress"
                | "file-history-snapshot"
                | "event_msg"
                | "turn_context"
                | "function_call"
                | "function_call_output"
        )
    ) {
        return true;
    }
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or_default()
    {
        return true;
    }
    if value.pointer("/payload/role").and_then(Value::as_str) == Some("developer") {
        return true;
    }
    false
}

fn extract_message_content(value: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(message) = value.get("message") {
        collect_text_fields(message, &mut parts);
    } else {
        collect_text_fields(value, &mut parts);
    }
    let text = parts.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn collect_text_fields(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            out.push(text.clone());
        }
        Value::Array(items) => {
            for item in items {
                collect_text_fields(item, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "base_instructions" | "environment_context" | "turn_context"
                ) {
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "text" | "content" | "message" | "input_text" | "output_text"
                ) {
                    collect_text_fields(value, out);
                } else if value.is_object() || value.is_array() {
                    collect_text_fields(value, out);
                }
            }
        }
        _ => {}
    }
}

fn sum_token_fields(value: &Value) -> u64 {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                let here = if key.contains("tokens") {
                    value.as_u64().unwrap_or_default()
                } else {
                    0
                };
                here + sum_token_fields(value)
            })
            .sum(),
        Value::Array(items) => items.iter().map(sum_token_fields).sum(),
        _ => 0,
    }
}

fn launch_session(session: &Session) -> Result<(), Box<dyn Error>> {
    let mut command = match session.provider {
        SessionProvider::Claude => {
            let mut command = Command::new("claude");
            command.arg("--resume").arg(&session.id);
            command
        }
        SessionProvider::Codex => {
            let mut command = Command::new("codex");
            command.arg("resume").arg(&session.id);
            command
        }
    };

    let status = command.current_dir(&session.cwd).status()?;
    if !status.success() {
        eprintln!("Session command exited with status: {status}");
    }
    Ok(())
}

fn compact_title(title: &str, fallback: &str) -> String {
    let source = if is_low_information_title(title) {
        fallback
    } else {
        title
    };
    let one_line = source.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 120 {
        format!("{}...", one_line.chars().take(117).collect::<String>())
    } else if is_low_information_title(&one_line) {
        "(untitled)".to_owned()
    } else {
        one_line
    }
}

fn is_low_information_title(title: &str) -> bool {
    let title = title.trim();
    title.is_empty() || title == "."
}

fn limited_search_text(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    text.chars().take(limit).collect()
}

fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            if stripped.as_os_str().is_empty() {
                return "~".to_owned();
            }
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

fn file_modified_unix(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let datetime: DateTime<Utc> = modified.into();
    Some(datetime.timestamp())
}

fn parse_timestamp(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.timestamp())
        .ok()
}

fn format_datetime(timestamp: i64) -> String {
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M").to_string(),
        None => "unknown".to_owned(),
    }
}

fn format_age(timestamp: i64) -> String {
    let age = Utc::now().timestamp().saturating_sub(timestamp).max(0);
    if age < 60 {
        "now".to_owned()
    } else if age < 60 * 60 {
        format!("{}m ago", age / 60)
    } else if age < 60 * 60 * 24 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86_400)
    }
}

fn format_tokens(tokens: Option<u64>) -> String {
    let Some(tokens) = tokens else {
        return "-".to_owned();
    };
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}
