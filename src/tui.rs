// Interactive terminal dashboard. It is another control-channel client: every change goes
// through the same requests as the CLI. Periodic refreshes only talk to a running manager,
// so the dashboard never holds the manager lock while the manager is starting.
use crate::{config::Paths, ipc, manager, protocol::Request};
use anyhow::{Result, ensure};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const STATUS_INTERVAL: Duration = Duration::from_secs(1);
const LOGS_INTERVAL: Duration = Duration::from_secs(2);
const PAGE: usize = 20;

pub async fn run(paths: Paths) -> Result<()> {
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "runnerctl tui needs an interactive terminal; use runnerctl status instead"
    );
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &paths).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, paths: &Paths) -> Result<()> {
    let (outcome_tx, mut outcomes) = mpsc::unbounded_channel();
    let (event_tx, mut events) = mpsc::unbounded_channel();
    // Terminal reads block, so they run on their own thread for the life of the process.
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    let mut app = App::new(paths.home.display().to_string());
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tasks = app.start();
    loop {
        for task in tasks.drain(..) {
            spawn(paths, &outcome_tx, task);
        }
        terminal.draw(|f| draw(f, &app))?;
        if app.quit {
            return Ok(());
        }
        tasks = tokio::select! {
            Some(event) = events.recv() => match event {
                Event::Key(key) => app.on_key(key),
                _ => Vec::new(),
            },
            Some(outcome) = outcomes.recv() => app.apply(outcome),
            _ = tick.tick() => app.on_tick(Instant::now()),
        };
    }
}

enum Task {
    /// `fallback` lets the request open saved state when the manager is stopped, like the CLI.
    Status {
        fallback: bool,
    },
    Change {
        request: Box<Request>,
        label: String,
    },
    Logs {
        pool: String,
        id: String,
        fallback: bool,
    },
    Doctor,
}
/// `Ok(None)` means the manager is not running and the request was not sent.
type Reply = Result<Option<Value>, String>;
enum Outcome {
    Status(Reply),
    Change { label: String, reply: Reply },
    Logs { id: String, reply: Reply },
    Doctor(Reply),
}

fn spawn(paths: &Paths, tx: &mpsc::UnboundedSender<Outcome>, task: Task) {
    let paths = paths.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let outcome = match task {
            Task::Status { fallback } => {
                Outcome::Status(execute(&paths, Request::Status { pool: None }, fallback).await)
            }
            Task::Change { request, label } => Outcome::Change {
                label,
                reply: execute(&paths, *request, true).await,
            },
            Task::Logs { pool, id, fallback } => {
                let request = Request::Logs {
                    pool,
                    id: id.clone(),
                };
                Outcome::Logs {
                    id,
                    reply: execute(&paths, request, fallback).await,
                }
            }
            Task::Doctor => Outcome::Doctor(execute(&paths, Request::Doctor, true).await),
        };
        let _ = tx.send(outcome);
    });
}

async fn execute(paths: &Paths, request: Request, fallback: bool) -> Reply {
    let result = if fallback {
        crate::dispatch(paths, request).await
    } else if ipc::connect(paths).await.is_ok() {
        manager::send(paths, &request).await
    } else {
        return Ok(None);
    };
    result.map(Some).map_err(|e| clean_line(&format!("{e:#}")))
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct PoolView {
    name: String,
    target: String,
    organization: String,
    intent: String,
    replicas: u32,
    active: u64,
    idle: u64,
    busy: u64,
    starting: u64,
    unknown: u64,
    draining: u64,
    error: Option<String>,
    config: PoolConfig,
    runners: Vec<RunnerView>,
}
impl PoolView {
    fn scope(&self) -> &str {
        if self.target.is_empty() {
            &self.organization
        } else {
            &self.target
        }
    }
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct PoolConfig {
    image: String,
    labels: Vec<String>,
    runner_group: String,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct RunnerView {
    id: String,
    phase: String,
    retiring: bool,
    drain_reason: Option<String>,
}
impl RunnerView {
    fn note(&self) -> &'static str {
        if !self.retiring {
            return "";
        }
        match self.drain_reason.as_deref() {
            Some("waiting_for_next_job") => "draining: waiting for next job",
            Some("job_in_progress") => "draining: finishing current job",
            _ => "draining: awaiting cleanup",
        }
    }
}

struct Check {
    ok: bool,
    name: String,
    detail: String,
}
fn checks(data: &Value) -> Vec<Check> {
    let join = |v: &Value| {
        v.as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default()
    };
    data["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let detail = match c["error"].as_str() {
                Some(error) => error.to_owned(),
                None => [join(&c["reasons"]), join(&c["warnings"])]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or_default(),
            };
            Check {
                ok: c["ok"] == true,
                name: c["check"].as_str().unwrap_or_default().to_owned(),
                detail: clean_line(&detail),
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Pools,
    Runners,
}
enum Overlay {
    Help,
    Confirm {
        prompt: String,
        request: Request,
        label: String,
    },
    Logs {
        pool: String,
        id: String,
        lines: Vec<String>,
        /// Lines scrolled up from the end; zero follows new output.
        back: usize,
        note: Option<String>,
        fetched: Option<Instant>,
    },
    Doctor(Option<Result<Vec<Check>, String>>),
}
struct Message {
    error: bool,
    text: String,
}

struct App {
    home: String,
    pools: Vec<PoolView>,
    revision: u64,
    /// None until the first status reply.
    manager: Option<bool>,
    load_error: Option<String>,
    selected: usize,
    runner: usize,
    focus: Focus,
    overlay: Option<Overlay>,
    message: Option<Message>,
    status_busy: bool,
    status_at: Option<Instant>,
    logs_busy: bool,
    changing: bool,
    quit: bool,
}

impl App {
    fn new(home: String) -> Self {
        Self {
            home,
            pools: Vec::new(),
            revision: 0,
            manager: None,
            load_error: None,
            selected: 0,
            runner: 0,
            focus: Focus::Pools,
            overlay: None,
            message: None,
            status_busy: false,
            status_at: None,
            logs_busy: false,
            changing: false,
            quit: false,
        }
    }
    fn start(&mut self) -> Vec<Task> {
        self.status_busy = true;
        vec![Task::Status { fallback: true }]
    }
    fn pool(&self) -> Option<&PoolView> {
        self.pools.get(self.selected)
    }
    fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            error: false,
            text: text.into(),
        });
    }
    fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            error: true,
            text: text.into(),
        });
    }

    fn on_tick(&mut self, now: Instant) -> Vec<Task> {
        let mut tasks = Vec::new();
        if !self.status_busy
            && self
                .status_at
                .is_none_or(|at| now.duration_since(at) >= STATUS_INTERVAL)
        {
            self.status_busy = true;
            tasks.push(Task::Status { fallback: false });
        }
        if let Some(Overlay::Logs {
            pool, id, fetched, ..
        }) = &self.overlay
        {
            if !self.logs_busy && fetched.is_some_and(|at| now.duration_since(at) >= LOGS_INTERVAL)
            {
                self.logs_busy = true;
                tasks.push(Task::Logs {
                    pool: pool.clone(),
                    id: id.clone(),
                    fallback: false,
                });
            }
        }
        tasks
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Task> {
        // Windows also reports key releases.
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if let Some(overlay) = self.overlay.take() {
            return self.overlay_key(overlay, key);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.focus = match self.focus {
                    Focus::Pools => Focus::Runners,
                    Focus::Runners => Focus::Pools,
                }
            }
            KeyCode::Char('r') => {
                self.status_busy = true;
                return vec![Task::Status {
                    fallback: !self.changing,
                }];
            }
            KeyCode::Char('s') => {
                if let Some(name) = self.pool().map(|p| p.name.clone()) {
                    return self.change(
                        Request::Start {
                            pool: Some(name.clone()),
                            all: false,
                        },
                        format!("start {name}"),
                    );
                }
            }
            KeyCode::Char(c @ ('x' | 'X')) => {
                if let Some(name) = self.pool().map(|p| p.name.clone()) {
                    let force = c == 'X';
                    let prompt = if force {
                        format!("Force stop pool '{name}'? Running jobs may be interrupted.")
                    } else {
                        format!(
                            "Stop pool '{name}'? Replacements stop, busy jobs finish, and idle runners can accept one final job."
                        )
                    };
                    self.overlay = Some(Overlay::Confirm {
                        prompt,
                        request: Request::Stop {
                            pool: Some(name.clone()),
                            all: false,
                            force,
                        },
                        label: format!("{} {name}", if force { "force stop" } else { "stop" }),
                    });
                }
            }
            KeyCode::Char('R') => {
                if let Some(name) = self.pool().map(|p| p.name.clone()) {
                    self.overlay = Some(Overlay::Confirm {
                        prompt: format!(
                            "Restart pool '{name}'? Every runner is replaced after its current or next job; stopped pools start."
                        ),
                        request: Request::Restart {
                            pool: Some(name.clone()),
                            all: false,
                            force: false,
                        },
                        label: format!("restart {name}"),
                    });
                }
            }
            KeyCode::Char('+' | '=') => return self.scale(true),
            KeyCode::Char('-') => return self.scale(false),
            KeyCode::Enter | KeyCode::Char('l') => return self.open_logs(),
            KeyCode::Char('d') => {
                self.overlay = Some(Overlay::Doctor(None));
                return vec![Task::Doctor];
            }
            _ => {}
        }
        Vec::new()
    }

    fn overlay_key(&mut self, overlay: Overlay, key: KeyEvent) -> Vec<Task> {
        let close = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));
        match overlay {
            Overlay::Help | Overlay::Doctor(_) => {}
            Overlay::Confirm { request, label, .. }
                if matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) =>
            {
                return self.change(request, label);
            }
            Overlay::Confirm { .. } if close || key.code == KeyCode::Char('n') => {}
            Overlay::Logs { .. } if close || key.code == KeyCode::Char('l') => {}
            Overlay::Logs {
                pool,
                id,
                lines,
                back,
                note,
                fetched,
            } => {
                let back = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => back + 1,
                    KeyCode::Down | KeyCode::Char('j') => back.saturating_sub(1),
                    KeyCode::PageUp => back + PAGE,
                    KeyCode::PageDown => back.saturating_sub(PAGE),
                    KeyCode::Home | KeyCode::Char('g') => lines.len(),
                    KeyCode::End | KeyCode::Char('G') => 0,
                    _ => back,
                };
                self.overlay = Some(Overlay::Logs {
                    back: back.min(lines.len()),
                    pool,
                    id,
                    lines,
                    note,
                    fetched,
                });
            }
            other => self.overlay = Some(other),
        }
        Vec::new()
    }

    fn move_selection(&mut self, delta: isize) {
        let step = |index: usize, len: usize| {
            index
                .saturating_add_signed(delta)
                .min(len.saturating_sub(1))
        };
        match self.focus {
            Focus::Pools => {
                self.selected = step(self.selected, self.pools.len());
                self.runner = 0;
            }
            Focus::Runners => {
                let len = self.pool().map_or(0, |p| p.runners.len());
                self.runner = step(self.runner, len);
            }
        }
    }

    fn change(&mut self, request: Request, label: String) -> Vec<Task> {
        if self.changing {
            self.error("Wait for the previous change to finish.");
            return Vec::new();
        }
        self.changing = true;
        self.info(format!("Sending {label}..."));
        vec![Task::Change {
            request: Box::new(request),
            label,
        }]
    }

    fn scale(&mut self, up: bool) -> Vec<Task> {
        if self.changing {
            self.error("Wait for the previous change to finish.");
            return Vec::new();
        }
        let Some(pool) = self.pools.get_mut(self.selected) else {
            return Vec::new();
        };
        let replicas = if up {
            pool.replicas.saturating_add(1)
        } else if pool.replicas == 0 {
            return Vec::new();
        } else {
            pool.replicas - 1
        };
        // Show the new target now so repeated presses build on it; the next status corrects it.
        pool.replicas = replicas;
        let name = pool.name.clone();
        self.change(
            Request::Scale {
                pool: name.clone(),
                replicas,
            },
            format!("scale {name} to {replicas}"),
        )
    }

    fn open_logs(&mut self) -> Vec<Task> {
        let Some(pool) = self.pool() else {
            return Vec::new();
        };
        let Some(runner) = pool.runners.get(self.runner) else {
            self.error("The selected pool has no active runners.");
            return Vec::new();
        };
        let (pool, id) = (pool.name.clone(), runner.id.clone());
        self.overlay = Some(Overlay::Logs {
            pool: pool.clone(),
            id: id.clone(),
            lines: Vec::new(),
            back: 0,
            note: Some("Loading...".into()),
            fetched: None,
        });
        self.logs_busy = true;
        vec![Task::Logs {
            pool,
            id,
            fallback: true,
        }]
    }

    fn apply(&mut self, outcome: Outcome) -> Vec<Task> {
        match outcome {
            Outcome::Status(reply) => {
                self.status_busy = false;
                self.status_at = Some(Instant::now());
                match reply {
                    Ok(Some(data)) => self.load(&data),
                    Ok(None) => self.manager = Some(false),
                    Err(error) => self.load_error = Some(error),
                }
            }
            Outcome::Change { label, reply } => {
                self.changing = false;
                match reply {
                    Ok(data) => {
                        let data = data.unwrap_or_default();
                        let mut text = data["message"]
                            .as_str()
                            .map_or_else(|| format!("Done: {label}."), clean_line);
                        if data["manager_running"] == false {
                            text.push_str(" Manager is stopped; the change is saved and applies when it starts.");
                        }
                        self.info(text);
                    }
                    Err(error) => self.error(format!("{label} failed: {error}")),
                }
                self.status_busy = true;
                return vec![Task::Status { fallback: true }];
            }
            Outcome::Logs { id, reply } => {
                self.logs_busy = false;
                if let Some(Overlay::Logs {
                    id: open,
                    lines,
                    back,
                    note,
                    fetched,
                    ..
                }) = &mut self.overlay
                {
                    if *open == id {
                        *fetched = Some(Instant::now());
                        match reply {
                            Ok(Some(data)) => {
                                let next = clean_lines(data["logs"].as_str().unwrap_or_default());
                                // Keep a scrolled-up view on the same lines while output grows.
                                if *back > 0 {
                                    *back += next.len().saturating_sub(lines.len());
                                }
                                *back = (*back).min(next.len());
                                *lines = next;
                                *note = None;
                            }
                            Ok(None) => {
                                *note = Some("Manager is stopped; logs are not refreshing.".into())
                            }
                            Err(error) => *note = Some(error),
                        }
                    }
                }
            }
            Outcome::Doctor(reply) => {
                if let Some(Overlay::Doctor(result)) = &mut self.overlay {
                    *result = Some(match reply {
                        Ok(data) => Ok(checks(&data.unwrap_or_default())),
                        Err(error) => Err(error),
                    });
                }
            }
        }
        Vec::new()
    }

    fn load(&mut self, data: &Value) {
        let pools = match Vec::<PoolView>::deserialize(&data["pools"]) {
            Ok(pools) => pools,
            Err(error) => {
                self.load_error = Some(format!("unexpected status reply: {error}"));
                return;
            }
        };
        let previous = self.pool().map(|p| p.name.clone());
        self.selected = previous
            .and_then(|name| pools.iter().position(|p| p.name == name))
            .unwrap_or(self.selected)
            .min(pools.len().saturating_sub(1));
        self.pools = pools;
        self.runner = self
            .runner
            .min(self.pool().map_or(0, |p| p.runners.len()).saturating_sub(1));
        self.revision = data["revision"].as_u64().unwrap_or_default();
        self.manager = Some(data["manager_running"] != false);
        self.load_error = None;
    }
}

fn clean_lines(text: &str) -> Vec<String> {
    text.lines().map(clean_line).collect()
}
/// Keeps the text a terminal would end up showing, without sequences that could move the
/// dashboard's cursor or change its colors.
fn clean_line(line: &str) -> String {
    let line = line
        .rsplit('\r')
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn draw(f: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(f.area());
    draw_header(f, app, header);
    let pool_height = (app.pools.len().max(1) as u16 + 3).min((body.height / 2).max(4));
    let [pools, detail] =
        Layout::vertical([Constraint::Length(pool_height), Constraint::Min(0)]).areas(body);
    draw_pools(f, app, pools);
    draw_detail(f, app, detail);
    draw_footer(f, app, footer);
    match &app.overlay {
        None => {}
        Some(Overlay::Help) => draw_help(f),
        Some(Overlay::Confirm { prompt, .. }) => draw_confirm(f, prompt),
        Some(Overlay::Logs {
            pool,
            id,
            lines,
            back,
            note,
            ..
        }) => draw_logs(f, pool, id, lines, *back, note.as_deref()),
        Some(Overlay::Doctor(result)) => draw_doctor(f, result.as_ref()),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let manager = match app.manager {
        None => Span::raw("connecting..."),
        Some(true) => "manager running".green(),
        Some(false) => "manager stopped (showing saved state)".yellow(),
    };
    let mut spans = vec![
        format!(" runnerctl {} ", env!("CARGO_PKG_VERSION")).bold(),
        Span::raw(" "),
        manager,
        format!("  revision {}", app.revision).dark_gray(),
        format!("  {}", app.home).dark_gray(),
    ];
    if app.status_busy {
        spans.push("  refreshing".dark_gray());
    }
    f.render_widget(Line::from(spans), area);
}

fn intent_style(intent: &str) -> Style {
    match intent {
        "running" => Style::new().fg(Color::Green),
        "deleting" => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::DarkGray),
    }
}
fn phase_style(phase: &str) -> Style {
    match phase {
        "idle" => Style::new().fg(Color::Green),
        "busy" => Style::new().fg(Color::Yellow),
        "creating" | "starting" => Style::new().fg(Color::Cyan),
        "unknown" => Style::new().fg(Color::Red),
        _ => Style::new(),
    }
}
fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    }
}
fn width(values: impl Iterator<Item = usize>, header: &str) -> u16 {
    values.max().unwrap_or(0).max(header.len()).min(40) as u16
}

fn draw_pools(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" Pools ");
    if app.pools.is_empty() {
        let text = match (&app.load_error, app.manager) {
            (Some(error), _) => Text::from(error.as_str()).red(),
            (None, None) => Text::raw("Loading..."),
            (None, Some(_)) => Text::raw(
                "No pools yet. Add one with: runnerctl pool add NAME --org ORG --auth PROFILE --labels LABEL",
            ),
        };
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(block),
            area,
        );
        return;
    }
    let numbers = [
        "TARGET", "ACTIVE", "IDLE", "BUSY", "STARTING", "UNKNOWN", "DRAINING",
    ];
    let header = Row::new(
        ["POOL", "SCOPE", "INTENT"]
            .into_iter()
            .map(Cell::from)
            .chain(
                numbers
                    .iter()
                    .map(|h| Cell::from(Line::from(*h).right_aligned())),
            ),
    )
    .bold();
    let rows = app.pools.iter().map(|p| {
        let counts = [
            u64::from(p.replicas),
            p.active,
            p.idle,
            p.busy,
            p.starting,
            p.unknown,
            p.draining,
        ];
        let row = Row::new(
            [
                Cell::from(p.name.as_str()),
                Cell::from(p.scope()),
                Cell::from(p.intent.as_str()).style(intent_style(&p.intent)),
            ]
            .into_iter()
            .chain(
                counts
                    .iter()
                    .map(|n| Cell::from(Line::from(n.to_string()).right_aligned())),
            ),
        );
        if p.error.is_some() { row.red() } else { row }
    });
    let mut widths = vec![
        Constraint::Length(width(app.pools.iter().map(|p| p.name.len()), "POOL")),
        Constraint::Length(width(app.pools.iter().map(|p| p.scope().len()), "SCOPE")),
        Constraint::Length(8),
    ];
    widths.extend(numbers.iter().map(|h| Constraint::Length(h.len() as u16)));
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(highlight(app.focus == Focus::Pools))
        .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(pool) = app.pool() else {
        f.render_widget(Block::bordered().title(" Runners "), area);
        return;
    };
    let block = Block::bordered().title(format!(" {} ", pool.name));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let field = |name: &'static str, value: String| {
        Line::from(vec![format!("{name:<8}").dark_gray(), Span::raw(value)])
    };
    let mut info = vec![
        field("scope", pool.scope().to_owned()),
        field("image", pool.config.image.clone()),
        field("labels", pool.config.labels.join(", ")),
        field("group", pool.config.runner_group.clone()),
    ];
    if let Some(error) = &pool.error {
        info.push(Line::from(vec![
            format!("{:<8}", "error").dark_gray(),
            clean_line(error).red(),
        ]));
    }
    if pool.draining > 0 {
        info.push(
            "Draining: idle runners can accept one final job; busy jobs finish before cleanup."
                .yellow()
                .into(),
        );
    }
    let info_height = info.len() as u16 + 1;
    let [info_area, runners_area] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(0)]).areas(inner);
    f.render_widget(Paragraph::new(info), info_area);
    if pool.runners.is_empty() {
        f.render_widget("No active runners.".dark_gray(), runners_area);
        return;
    }
    let rows = pool.runners.iter().map(|r| {
        Row::new([
            Cell::from(r.id.as_str()),
            Cell::from(r.phase.as_str()).style(phase_style(&r.phase)),
            Cell::from(r.note()).yellow(),
        ])
    });
    let widths = [
        Constraint::Length(width(pool.runners.iter().map(|r| r.id.len()), "RUNNER")),
        Constraint::Length(9),
        Constraint::Min(0),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(["RUNNER", "PHASE", "NOTE"]).bold())
        .row_highlight_style(highlight(app.focus == Focus::Runners))
        .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(Some(app.runner));
    f.render_stateful_widget(table, runners_area, &mut state);
}

const HINTS: &str = "↑↓ select  Tab pools/runners  s start  x stop  X force stop  +/- scale  l logs  d doctor  r refresh  ? help  q quit";

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let message = match (&app.message, &app.load_error) {
        (Some(Message { error: true, text }), _) => Line::from(text.as_str()).red(),
        (Some(Message { text, .. }), _) => Line::from(text.as_str()),
        (None, Some(error)) if !app.pools.is_empty() => {
            Line::from(format!("Refresh failed: {error}")).red()
        }
        _ => Line::default(),
    };
    let [message_area, hints] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(area);
    f.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: true }),
        message_area,
    );
    f.render_widget(Line::from(HINTS).dark_gray(), hints);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn draw_help(f: &mut Frame) {
    let keys = [
        ("↑/↓ j/k", "Move the selection"),
        ("Tab", "Switch between pools and runners"),
        ("s", "Start the selected pool"),
        ("x", "Stop the selected pool (busy jobs finish)"),
        ("X", "Force stop (running jobs may be interrupted)"),
        ("R", "Restart: replace every runner (busy jobs finish)"),
        ("+ / -", "Change the pool's target runner count"),
        ("l / Enter", "Show logs for the selected runner"),
        ("d", "Run doctor checks"),
        ("r", "Refresh now"),
        ("q / Esc", "Quit, or close a window"),
    ];
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, action)| Line::from(vec![format!("{key:<12}").bold(), Span::raw(*action)]))
        .collect();
    let area = centered(f.area(), 60, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        area,
    );
}

fn draw_confirm(f: &mut Frame, prompt: &str) {
    let area = centered(f.area(), 64, 6);
    f.render_widget(Clear, area);
    let text = vec![
        Line::from(prompt),
        Line::default(),
        Line::from(vec![
            "y".bold(),
            " confirm   ".into(),
            "n".bold(),
            " cancel".into(),
        ]),
    ];
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" Confirm ")),
        area,
    );
}

fn draw_logs(
    f: &mut Frame,
    pool: &str,
    id: &str,
    lines: &[String],
    back: usize,
    note: Option<&str>,
) {
    let area = f.area().inner(ratatui::layout::Margin::new(1, 1));
    f.render_widget(Clear, area);
    let mode = if back == 0 { "following" } else { "paused" };
    let block = Block::bordered()
        .title(format!(" {pool} / {id} "))
        .title_bottom(format!(" {mode}  ↑↓ PgUp/PgDn g/G scroll  Esc close "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut height = usize::from(inner.height);
    let mut text = Vec::new();
    if let Some(note) = note {
        text.push(Line::from(note).yellow());
        height = height.saturating_sub(1);
    }
    let end = lines.len().saturating_sub(back);
    let start = end.saturating_sub(height);
    text.extend(lines[start..end].iter().map(|l| Line::from(l.as_str())));
    f.render_widget(Paragraph::new(text), inner);
}

fn draw_doctor(f: &mut Frame, result: Option<&Result<Vec<Check>, String>>) {
    let lines: Vec<Line> = match result {
        None => vec![Line::from("Running checks...")],
        Some(Err(error)) => vec![Line::from(error.as_str()).red()],
        Some(Ok(checks)) => checks
            .iter()
            .map(|c| {
                let mut spans = vec![
                    if c.ok { " OK  ".green() } else { "FAIL ".red() },
                    Span::raw(c.name.clone()),
                ];
                if !c.detail.is_empty() {
                    spans.push(format!("  {}", c.detail).dark_gray());
                }
                Line::from(spans)
            })
            .collect(),
    };
    let area = centered(f.area(), 100, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" Doctor ")
                .title_bottom(" Esc close "),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn status(names: &[&str]) -> Outcome {
        let pools: Vec<Value> = names
            .iter()
            .map(|name| {
                json!({"name":name,"target":"example","intent":"running","replicas":2,
                "active":2,"idle":1,"busy":1,"starting":0,"unknown":0,"draining":0,"error":null,
                "config":{"image":"img:1","labels":["ci"],"runner_group":"Default"},
                "runners":[{"id":format!("{name}-a"),"phase":"idle","retiring":false},
                           {"id":format!("{name}-b"),"phase":"busy","retiring":true,"drain_reason":"job_in_progress"}]})
            })
            .collect();
        Outcome::Status(Ok(Some(json!({"revision":3,"pools":pools}))))
    }
    fn loaded(names: &[&str]) -> App {
        let mut app = App::new("/home".into());
        app.start();
        app.apply(status(names));
        app
    }
    fn changed(tasks: &[Task]) -> &Request {
        match tasks {
            [Task::Change { request, .. }] => request,
            _ => panic!("expected one change"),
        }
    }
    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn clean_line_removes_terminal_control_sequences() {
        assert_eq!(clean_line("\x1b[31mred\x1b[0m text"), "red text");
        assert_eq!(clean_line("10%\r50%\r100%"), "100%");
        assert_eq!(clean_line("done\r"), "done");
        assert_eq!(clean_line("\x1b]0;title\x07a\tb\x08"), "a    b");
    }

    #[test]
    fn refresh_keeps_selection_by_name_and_tracks_manager() {
        let mut app = loaded(&["a", "b", "c"]);
        assert_eq!(app.manager, Some(true));
        app.on_key(key('j'));
        app.on_key(key('j'));
        assert_eq!(app.pool().unwrap().name, "c");
        app.apply(status(&["c", "a"]));
        assert_eq!(app.pool().unwrap().name, "c");
        app.apply(Outcome::Status(Ok(None)));
        assert_eq!(app.manager, Some(false));
        assert_eq!(app.pools.len(), 2);
    }

    #[test]
    fn periodic_refresh_never_falls_back_to_saved_state() {
        let mut app = loaded(&["a"]);
        let later = Instant::now() + STATUS_INTERVAL;
        let tasks = app.on_tick(later);
        assert!(matches!(
            tasks.as_slice(),
            [Task::Status { fallback: false }]
        ));
        assert!(app.on_tick(later).is_empty(), "one refresh at a time");
    }

    #[test]
    fn stop_requires_confirmation() {
        let mut app = loaded(&["a"]);
        assert!(app.on_key(key('x')).is_empty());
        assert!(app.on_key(key('n')).is_empty());
        assert!(app.overlay.is_none());
        app.on_key(key('X'));
        let tasks = app.on_key(key('y'));
        assert!(matches!(
            changed(&tasks),
            Request::Stop { pool: Some(p), all: false, force: true } if p == "a"
        ));
    }

    #[test]
    fn restart_requires_confirmation() {
        let mut app = loaded(&["a"]);
        assert!(app.on_key(key('R')).is_empty());
        let tasks = app.on_key(key('y'));
        assert!(matches!(
            changed(&tasks),
            Request::Restart { pool: Some(p), all: false, force: false } if p == "a"
        ));
    }

    #[test]
    fn scale_updates_target_and_waits_for_previous_change() {
        let mut app = loaded(&["a"]);
        let tasks = app.on_key(key('+'));
        assert!(matches!(
            changed(&tasks),
            Request::Scale { replicas: 3, .. }
        ));
        assert!(app.on_key(key('+')).is_empty());
        assert_eq!(app.pools[0].replicas, 3);
        let tasks = app.apply(Outcome::Change {
            label: "scale".into(),
            reply: Ok(Some(json!({"message":"Capacity updated."}))),
        });
        assert!(matches!(
            tasks.as_slice(),
            [Task::Status { fallback: true }]
        ));
        assert!(!app.message.as_ref().unwrap().error);
        assert_eq!(app.on_key(key('-')).len(), 1);
    }

    #[test]
    fn logs_follow_output_unless_scrolled_up() {
        let mut app = loaded(&["a"]);
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.on_key(key('j'));
        let tasks = app.on_key(key('l'));
        assert!(matches!(tasks.as_slice(), [Task::Logs { id, fallback: true, .. }] if id == "a-b"));
        let logs = |text: &str| Outcome::Logs {
            id: "a-b".into(),
            reply: Ok(Some(json!({ "logs": text }))),
        };
        app.apply(logs("1\n2\n3\n"));
        app.on_key(key('k'));
        app.apply(logs("1\n2\n3\n4\n5\n"));
        let Some(Overlay::Logs { back, lines, .. }) = &app.overlay else {
            panic!("logs closed")
        };
        assert_eq!((*back, lines.len()), (3, 5));
        app.on_key(key('G'));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert!(!app.quit);
    }

    #[test]
    fn renders_pools_runners_and_overlays() {
        let mut app = loaded(&["validate", "build"]);
        let text = screen(&app);
        for expected in [
            "validate",
            "build",
            "running",
            "validate-a",
            "finishing current job",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        app.on_key(key('d'));
        app.apply(Outcome::Doctor(Ok(Some(json!({"checks":[
            {"check":"docker","ok":true},
            {"check":"github:build","ok":false,"error":"bad credentials"}
        ]})))));
        let text = screen(&app);
        assert!(
            text.contains("FAIL github:build  bad credentials"),
            "{text}"
        );
    }

    #[test]
    fn empty_and_failed_loads_explain_next_step() {
        let mut app = App::new("/home".into());
        app.apply(Outcome::Status(Ok(Some(json!({"revision":0,"pools":[]})))));
        assert!(screen(&app).contains("No pools yet"));
        app.apply(Outcome::Status(Err("run runnerctl init first".into())));
        assert!(screen(&app).contains("run runnerctl init first"));
    }
}
