//! Draws the app state.

use std::time::Duration;

use carte_api::{InstanceStatus, Policy, Problem, ProcessState, ProcessStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, Connection};
use super::input::{Confirm, Focus, HELP, Mode};
use super::target;
use super::tree::{self, Node, NodeId, Source, State, Status};

const PREFIX_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Blue,
    Color::Green,
    Color::LightRed,
];

pub fn render(frame: &mut Frame, app: &mut App) {
    let [main, status, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    if app.snapshot.is_some() {
        let [left, right] = if main.width >= 110 {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main)
        };
        render_tree(frame, left, app);
        render_selection(frame, right, app);
    } else {
        render_waiting(frame, main, app);
    }
    frame.render_widget(status_line(app), status);
    frame.render_widget(footer_line(app.focus), footer);

    match &app.mode {
        Mode::Help => render_help(frame, main),
        Mode::Problems => render_problems(frame, main, app),
        Mode::Confirm(Confirm::StopAll) => render_confirm(frame, main, app),
        Mode::Normal | Mode::Filter => {}
    }
}

fn pane(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::new().cyan()
        } else {
            Style::new().dark_gray()
        })
        .title(title)
}

fn render_waiting(frame: &mut Frame, area: Rect, app: &App) {
    let lines = match &app.connection {
        Connection::Lost(message) => vec![
            Line::from("The carte daemon isn't reachable.".bold()),
            Line::default(),
            Line::from(message.clone()),
            Line::default(),
            Line::from("Retrying every second.".dark_gray()),
        ],
        Connection::Connecting | Connection::Connected => {
            vec![Line::from("Connecting to the carte daemon…")]
        }
    };
    let height = lines.len() as u16 + 2;
    let block = pane(Line::from(" carte "), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(lines).centered().wrap(Wrap { trim: true }),
        middle,
    );
}

// Tree

fn render_tree(frame: &mut Frame, area: Rect, app: &mut App) {
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    let mut title = vec![
        Span::raw(" carte "),
        Span::raw(format!("· {} running ", snapshot.running_instances)).dark_gray(),
    ];
    if app.fold.running_only {
        title.push(Span::raw("· running only ").dark_gray());
    }
    if !snapshot.problems.is_empty() {
        let count = snapshot.problems.len();
        let noun = if count == 1 { "problem" } else { "problems" };
        title.push(Span::raw(format!("· {count} {noun} (!) ")).red());
    }
    let block = pane(Line::from(title), app.focus == Focus::Tree);
    app.tree_height = block.inner(area).height.saturating_sub(1) as usize;

    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|row| {
            let node = tree::node(&app.roots, &row.path);
            tree_row(app, node, row)
        })
        .collect();
    let header = Row::new(["", "", "CPU", "MEM"]).dark_gray();
    let selected_style = if app.focus == Focus::Tree {
        Style::new().bg(Color::DarkGray)
    } else {
        Style::new().add_modifier(Modifier::DIM | Modifier::REVERSED)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(16),
            Constraint::Length(5),
            Constraint::Length(5),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(selected_style);
    let mut state =
        TableState::default().with_selected((!app.rows.is_empty()).then_some(app.cursor));
    frame.render_stateful_widget(table, area, &mut state);
    if app.rows.is_empty() {
        let message = if app.fold.filter.is_empty() && !app.fold.running_only {
            "No projects. Add one with `carte project add`."
        } else {
            "Nothing matches."
        };
        let inner = Rect {
            y: area.y + 2,
            height: 1,
            ..pane(Line::default(), false).inner(area)
        };
        frame.render_widget(Paragraph::new(message.dark_gray()).centered(), inner);
    }
}

fn tree_row(app: &App, node: &Node, row: &tree::Row) -> Row<'static> {
    let status = &node.status;
    let aggregate = matches!(node.id(), NodeId::Project { .. } | NodeId::Stack { .. });
    let (symbol, color) = symbol(status.state);
    let mut spans = vec![
        Span::raw("  ".repeat(row.depth)),
        Span::raw(match (row.expandable, row.expanded) {
            (false, _) => "  ",
            (true, true) => "▾ ",
            (true, false) => "▸ ",
        })
        .dark_gray(),
        Span::raw(symbol).fg(color),
        Span::raw(" "),
    ];
    let label = Span::raw(node.label.clone());
    spans.push(match node.ids[0] {
        NodeId::Project { .. } => label.bold(),
        _ if status.state == State::Disabled => label.dark_gray(),
        _ => label,
    });
    spans.extend(badges(status, aggregate));

    let detail = if aggregate {
        Span::raw(if status.running > 0 {
            format!("{}/{}", status.running, status.total)
        } else {
            String::new()
        })
        .dark_gray()
    } else {
        detail(app, node.id(), status)
    };
    let (cpu, memory) = if status.running > 0 {
        (
            format!("{:.0}%", status.cpu_percent),
            format_bytes(status.memory_bytes),
        )
    } else {
        (String::new(), String::new())
    };
    Row::new([
        Cell::from(Line::from(spans)),
        Cell::from(detail),
        Cell::from(Line::from(cpu).right_aligned()),
        Cell::from(Line::from(memory).right_aligned()),
    ])
}

fn symbol(state: State) -> (&'static str, Color) {
    match state {
        State::Ready => ("●", Color::Green),
        State::Starting => ("◐", Color::Yellow),
        State::Stopping => ("◑", Color::Yellow),
        State::Stopped | State::Exited => ("○", Color::DarkGray),
        State::Crashed => ("✖", Color::Red),
        State::Disabled => ("⊘", Color::DarkGray),
    }
}

/// Policies show on the instances and processes they apply to, not on groups.
fn badges(status: &Status, aggregate: bool) -> Vec<Span<'static>> {
    let marked = |word: &str, source: Source| match source {
        Source::Config => word.to_string(),
        Source::Temporary => format!("{word}*"),
    };
    let mut spans = Vec::new();
    if aggregate {
        if status.problems > 0 {
            spans.push(Span::raw(" !").red().bold());
        }
        return spans;
    }
    if status.fallback {
        spans.push(Span::raw(" main").dark_gray());
    }
    if let Some(source) = status.pinned {
        spans.push(Span::raw(format!(" {}", marked("pin", source))).cyan());
    }
    if let Some(source) = status.disabled {
        spans.push(Span::raw(format!(" {}", marked("off", source))).dark_gray());
    }
    if status.shared {
        spans.push(Span::raw(" shared").magenta());
    }
    if status.problems > 0 {
        spans.push(Span::raw(" !").red().bold());
    }
    spans
}

fn detail(app: &App, id: &NodeId, status: &Status) -> Span<'static> {
    let process = matches!(id, NodeId::Process { .. });
    match status.state {
        State::Ready if !status.held_by.is_empty() => {
            let more = match status.held_by.len() {
                1 => String::new(),
                n => format!(" +{}", n - 1),
            };
            Span::raw(format!("held by {}{more}", status.held_by[0])).dark_gray()
        }
        State::Ready => match status.idle_in_secs {
            Some(secs) => Span::raw(format!("idle in {}", duration(app.idle_in(secs)))).dark_gray(),
            None if process => Span::raw("running").green(),
            None => Span::raw("ready").green(),
        },
        State::Starting => Span::raw("starting").yellow(),
        State::Stopping => Span::raw("stopping").yellow(),
        State::Crashed if process => Span::raw("exited").red(),
        State::Crashed => Span::raw("crashed").red(),
        State::Exited => Span::raw("exited").dark_gray(),
        State::Disabled => Span::raw("disabled").dark_gray(),
        State::Stopped => Span::raw(""),
    }
}

fn duration(secs: u64) -> String {
    carte_core::duration::format(Duration::from_secs(secs))
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    match bytes {
        b if b < KB => format!("{b}B"),
        b if b < KB * KB => format!("{}K", b / KB),
        b if b < KB * KB * KB => format!("{}M", b / (KB * KB)),
        b => format!("{:.1}G", b as f64 / (KB * KB * KB) as f64),
    }
}

// Selection: details and logs

fn render_selection(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(node) = app.selected_node() else {
        render_logs(frame, area, app);
        return;
    };
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    let title = format!(" {} ", target::address(snapshot, node.id()).address);
    let lines = details(app, node);
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(4));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(pane(Line::from(title).bold(), false)),
        top,
    );
    render_logs(frame, bottom, app);
}

fn details(app: &App, node: &Node) -> Vec<Line<'static>> {
    let status = &node.status;
    let (symbol, color) = symbol(status.state);
    let state_word = match status.state {
        State::Ready => "running",
        State::Starting => "starting",
        State::Stopping => "stopping",
        State::Stopped => "stopped",
        State::Exited => "exited",
        State::Crashed => "crashed",
        State::Disabled => "disabled",
    };
    let state = Span::raw(format!("{symbol} {state_word}")).fg(color);
    let mut lines = Vec::new();
    let id = node.id();
    match id {
        NodeId::Project { .. } | NodeId::Stack { .. } => {
            let mut first = vec![
                Span::raw(symbol).fg(color),
                Span::raw(format!(
                    " {} of {} instances running",
                    status.running, status.total
                )),
            ];
            if let NodeId::Stack { project, stack } = id
                && let Some(branch) = app
                    .snapshot
                    .iter()
                    .flat_map(|s| &s.projects)
                    .filter(|p| p.name == *project)
                    .flat_map(|p| &p.stacks)
                    .find(|s| s.name == *stack)
                    .and_then(|s| s.branch.clone())
            {
                first.push(Span::raw(format!(" · branch {branch}")).dark_gray());
            }
            lines.push(Line::from(first));
            let crashed: Vec<&str> = crashed_below(node);
            if !crashed.is_empty() {
                lines.push(Line::from(format!("crashed: {}", crashed.join(", ")).red()));
            }
        }
        NodeId::Instance { .. } => {
            if let Some(instance) = app.instance(id) {
                lines.extend(instance_details(app, instance, state, status));
            }
        }
        NodeId::Process { process, .. } => {
            if let Some(instance) = app.instance(id)
                && let Some(proc) = instance.processes.iter().find(|p| p.name == *process)
            {
                lines.extend(process_details(instance, proc, state));
            }
        }
    }
    for problem in problems_for(app, id) {
        lines.push(Line::from(format!("! {}", problem.message)).red());
    }
    lines
}

fn crashed_below(node: &Node) -> Vec<&str> {
    let mut out = Vec::new();
    for child in &node.children {
        if child.status.state == State::Crashed {
            if matches!(child.id(), NodeId::Instance { .. }) {
                out.push(child.label.as_str());
            } else {
                out.extend(crashed_below(child));
            }
        }
    }
    out
}

fn instance_details(
    app: &App,
    instance: &InstanceStatus,
    state: Span<'static>,
    status: &Status,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        state,
        Span::raw(" · "),
        Span::raw(instance.url.clone()).underlined(),
    ])];
    if let Some(public) = &instance.public_url {
        lines.push(Line::from(vec![
            Span::raw("shared at ").magenta(),
            Span::raw(public.clone()).underlined(),
        ]));
    }
    let checkout = if status.fallback {
        "main (no worktree for this stack)".to_string()
    } else {
        instance.checkout.clone()
    };
    let mut checkout = format!("checkout {checkout}");
    if !instance.checkout_path.as_os_str().is_empty() {
        checkout.push_str(&format!(" · {}", instance.checkout_path.display()));
    }
    lines.push(Line::from(checkout.dark_gray()));
    let mut notes = Vec::new();
    for (word, policy) in [("pinned", Policy::Pinned), ("disabled", Policy::Disabled)] {
        let procs: Vec<&ProcessStatus> = instance
            .processes
            .iter()
            .filter(|p| p.policy == policy)
            .collect();
        if procs.is_empty() {
            continue;
        }
        let mut note = word.to_string();
        if procs.len() < instance.processes.len() {
            let names: Vec<&str> = procs.iter().map(|p| p.name.as_str()).collect();
            note = format!("{} {word}", names.join(", "));
        }
        if procs.iter().any(|p| p.temporary) {
            note.push_str(" until the daemon restarts");
        }
        notes.push(note);
    }
    if !status.held_by.is_empty() {
        notes.push(format!("held by {}", status.held_by.join(", ")));
    }
    if let Some(secs) = status.idle_in_secs {
        notes.push(format!("idles out in {}", duration(app.idle_in(secs))));
    }
    if !notes.is_empty() {
        lines.push(Line::from(notes.join(" · ")).dark_gray());
    }
    if let Some(error) = &instance.last_error
        && status.state == State::Crashed
    {
        lines.push(Line::from(error.clone()).red());
    }
    lines
}

fn process_details(
    instance: &InstanceStatus,
    process: &ProcessStatus,
    state: Span<'static>,
) -> Vec<Line<'static>> {
    let mut first = vec![state];
    let mut facts = Vec::new();
    if let Some(pid) = process.pid {
        facts.push(format!("pid {pid}"));
    }
    if let Some(port) = process.port {
        facts.push(format!("port {port}"));
    }
    if process.http {
        facts.push("serves HTTP".into());
    }
    if process.restarts > 0 {
        facts.push(format!("{} restarts", process.restarts));
    }
    for fact in facts {
        first.push(Span::raw(format!(" · {fact}")).dark_gray());
    }
    let mut lines = vec![Line::from(first)];
    if let ProcessState::Exited { code, signal } = process.state {
        let how = match (code, signal) {
            (_, Some(signal)) => format!("killed by signal {signal}"),
            (Some(code), None) => format!("exited with code {code}"),
            (None, None) => "exited".into(),
        };
        let line = Line::from(how);
        lines.push(if code == Some(0) {
            line.dark_gray()
        } else {
            line.red()
        });
    }
    if let Some(error) = &instance.last_error
        && !matches!(process.state, ProcessState::Running)
    {
        lines.push(Line::from(error.clone()).red());
    }
    lines
}

fn problems_for<'a>(app: &'a App, id: &NodeId) -> Vec<&'a Problem> {
    let app_name = match id {
        NodeId::Instance { app, .. } | NodeId::Process { app, .. } => Some(app.as_str()),
        NodeId::Project { .. } | NodeId::Stack { .. } => None,
    };
    app.snapshot
        .iter()
        .flat_map(|s| &s.problems)
        .filter(|p| {
            p.project.as_deref() == Some(id.project())
                && (app_name.is_none() || p.app.as_deref() == app_name)
        })
        .collect()
}

fn render_logs(frame: &mut Frame, area: Rect, app: &mut App) {
    let logs = &app.logs;
    let mut title = vec![Span::raw(" logs ")];
    if !logs.filter.is_empty() {
        title.push(Span::raw(format!("/{} ", logs.filter)).yellow());
    }
    title.push(if logs.scroll > 0 {
        Span::raw(format!("↑{} ", logs.scroll)).yellow()
    } else {
        Span::raw("following ").dark_gray()
    });
    let block = pane(Line::from(title), app.focus == Focus::Logs);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.logs.height = inner.height as usize;
    let logs = &app.logs;

    let window = logs.window(inner.height as usize);
    if window.is_empty() {
        let stopped = app
            .selected_node()
            .is_some_and(|n| matches!(n.status.state, State::Stopped | State::Disabled));
        let message = match (&logs.ended, logs.is_empty()) {
            (Some(reason), _) => format!("No logs: {reason}"),
            (None, false) => "No lines match the filter.".into(),
            (None, true) if logs.key.is_some() && stopped => "Not running; s starts it.".into(),
            (None, true) if logs.key.is_some() => "No output yet.".into(),
            (None, true) => String::new(),
        };
        frame.render_widget(Paragraph::new(message.dark_gray()), inner);
        return;
    }
    let width = window
        .iter()
        .filter_map(|line| logs.prefix.label(line))
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = window
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            if let Some(label) = logs.prefix.label(line) {
                let color = PREFIX_COLORS[logs.color_index(&label) % PREFIX_COLORS.len()];
                spans.push(Span::raw(format!("{label:width$} ")).fg(color));
            }
            spans.extend(highlight(&line.text, &logs.filter));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Marks the first match of the filter in a line.
fn highlight(text: &str, filter: &str) -> Vec<Span<'static>> {
    let found = (!filter.is_empty())
        .then(|| text.to_ascii_lowercase().find(&filter.to_ascii_lowercase()))
        .flatten();
    match found {
        Some(start) if text.is_char_boundary(start + filter.len()) => {
            let end = start + filter.len();
            vec![
                Span::raw(text[..start].to_string()),
                Span::raw(text[start..end].to_string()).black().on_yellow(),
                Span::raw(text[end..].to_string()),
            ]
        }
        _ => vec![Span::raw(text.to_string())],
    }
}

// Status, footer and popups

fn status_line(app: &App) -> Line<'static> {
    if app.mode == Mode::Filter {
        let (pane, filter) = match app.focus {
            Focus::Tree => ("tree", &app.fold.filter),
            Focus::Logs => ("logs", &app.logs.filter),
        };
        return Line::from(vec![
            Span::raw(format!(" filter {pane}: ")).dark_gray(),
            Span::raw(format!("{filter}▏")).yellow(),
            Span::raw("  enter keeps · esc clears").dark_gray(),
        ]);
    }
    if let Some(notice) = &app.notice {
        let text = Span::raw(format!(" {}", notice.text));
        return Line::from(if notice.error {
            text.red()
        } else {
            text.green()
        });
    }
    if let Connection::Lost(_) = app.connection {
        return Line::from(" reconnecting to the daemon…".yellow());
    }
    if !app.fold.filter.is_empty() {
        return Line::from(format!(" filter: {} (esc clears)", app.fold.filter).dark_gray());
    }
    Line::default()
}

fn footer_line(focus: Focus) -> Line<'static> {
    let keys: &[(&str, &str)] = match focus {
        Focus::Tree => &[
            ("?", "help"),
            ("q", "quit"),
            ("s", "start"),
            ("x", "stop"),
            ("r", "restart"),
            ("p", "pin"),
            ("d", "disable"),
            ("/", "filter"),
            ("a", "running only"),
            ("tab", "logs"),
        ],
        Focus::Logs => &[
            ("?", "help"),
            ("q", "quit"),
            ("↑↓", "scroll"),
            ("g", "top"),
            ("G", "follow"),
            ("/", "filter"),
            ("tab", "tree"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::raw(*key).cyan().bold());
        spans.push(Span::raw(format!(" {what}  ")).dark_gray());
    }
    Line::from(spans)
}

fn popup(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(title.len()) as u16
        + 4;
    let height = lines.len() as u16 + 2;
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            pane(Line::from(format!(" {title} ")).bold(), true)
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = HELP
        .iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (i, (section, keys)) in HELP.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(section.to_string()).bold());
        for (key, what) in keys.iter() {
            lines.push(Line::from(vec![
                Span::raw(format!("{key:width$}  ")).cyan(),
                Span::raw(what.to_string()),
            ]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(
        "pin* and off* last until the daemon restarts".dark_gray(),
    ));
    popup(frame, area, "keys", lines);
}

fn render_problems(frame: &mut Frame, area: Rect, app: &App) {
    let problems: Vec<&Problem> = app.snapshot.iter().flat_map(|s| &s.problems).collect();
    let lines = if problems.is_empty() {
        vec![Line::from("No config problems.")]
    } else {
        problems
            .iter()
            .map(|p| {
                let place = [p.project.as_deref(), p.app.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("/");
                Line::from(vec![
                    Span::raw(if place.is_empty() {
                        String::new()
                    } else {
                        format!("{place}: ")
                    })
                    .bold(),
                    Span::raw(p.message.clone()),
                ])
            })
            .collect()
    };
    popup(frame, area, "config problems", lines);
}

fn render_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let running = app.snapshot.as_ref().map_or(0, |s| s.running_instances);
    popup(
        frame,
        area,
        "stop everything",
        vec![
            Line::from(format!(
                "Stop all {running} running instances, pinned ones too?"
            )),
            Line::default(),
            Line::from(vec![
                Span::raw("y").cyan().bold(),
                Span::raw(" stop   "),
                Span::raw("any other key").cyan().bold(),
                Span::raw(" cancel"),
            ]),
        ],
    );
}

