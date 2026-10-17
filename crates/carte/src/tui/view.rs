//! Draws the app state.

use std::time::Duration;

use carte_api::{InstanceStatus, Policy, Problem, ProcessState, ProcessStatus, TunnelState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, Connection};
use super::input::{Confirm, Focus, HELP, Mode};
use super::target;
use super::tree::{self, Node, NodeId, Show, Source, State, Status};

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
        Mode::Confirm(confirm) => render_confirm(frame, main, app, confirm),
        Mode::Normal | Mode::Filter => {}
    }
}

fn pane(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::new().blue()
        } else {
            Style::new().dark_gray()
        })
        .title_style(if focused {
            Style::new().blue()
        } else {
            Style::new().fg(Color::Reset)
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
    match app.fold.show {
        Show::All => {}
        Show::Running => title.push(Span::raw("· running only ").dark_gray()),
        Show::Shared => title.push(Span::raw("· shared only ").dark_gray()),
    }
    if !snapshot.shares.is_empty()
        && let Some(tunnel) = &snapshot.tunnel
    {
        let shared: usize = snapshot.shares.iter().map(|s| s.apps.len()).sum();
        let (word, color) = match tunnel.state {
            TunnelState::Connected => ("online", Color::Magenta),
            TunnelState::Starting => ("connecting", Color::Yellow),
            TunnelState::Failed => ("tunnel down (!)", Color::Red),
            TunnelState::Idle => ("", Color::DarkGray),
        };
        let what = if tunnel.state == TunnelState::Idle {
            format!("· {shared} shared ")
        } else {
            format!("· {shared} shared, {word} ")
        };
        title.push(Span::raw(what).fg(color));
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
    let mut state = TableState::default()
        .with_offset(app.tree_offset)
        .with_selected((!app.rows.is_empty()).then_some(app.cursor));
    frame.render_stateful_widget(table, area, &mut state);
    app.tree_area = area;
    app.tree_offset = state.offset();
    if app.rows.is_empty() {
        let message = if app.fold.filter.is_empty() && app.fold.show == Show::All {
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
    let aggregate = matches!(
        node.id(),
        NodeId::Folder { .. } | NodeId::Project { .. } | NodeId::Stack { .. }
    );
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
        NodeId::Folder { .. } => label.bold(),
        NodeId::Project { .. } => label.bold(),
        _ if status.state == State::Disabled => label.dark_gray(),
        NodeId::Stack { .. } => label.blue().bold(),
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
        spans.push(Span::raw(format!(" {}", marked("pin", source))).blue());
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
    let title = format!(" {} ", target::label(node.id()));
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
        NodeId::Folder { .. } | NodeId::Project { .. } | NodeId::Stack { .. } => {
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
                lines.extend(instance_details(app, id.project(), instance, state, status));
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
    project: &str,
    instance: &InstanceStatus,
    state: Span<'static>,
    status: &Status,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        state,
        Span::raw(" · "),
        Span::raw(instance.url.clone()).underlined(),
    ])];
    lines.extend(share_details(app, project, instance));
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

/// Where a shared instance is public, how it's guarded, and its latest visitors.
fn share_details(app: &App, project: &str, instance: &InstanceStatus) -> Vec<Line<'static>> {
    let Some(public) = &instance.public_url else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(vec![
        Span::raw("shared at ").magenta(),
        Span::raw(public.clone()).underlined(),
    ])];
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    if let Some(tunnel) = &snapshot.tunnel
        && tunnel.state == TunnelState::Failed
    {
        let message = tunnel.message.as_deref().unwrap_or("no reason given");
        lines.push(Line::from(format!("the tunnel is down: {message}")).red());
    }
    let Some(share) = snapshot
        .shares
        .iter()
        .find(|s| s.project == project && s.stack == instance.stack)
    else {
        return lines;
    };
    let mut notes = Vec::new();
    match app
        .credentials
        .get(&(share.project.clone(), share.stack.clone()))
    {
        Some(credentials) => notes.push(format!(
            "user {}, password {}",
            credentials.user, credentials.password
        )),
        None if share.auth => notes.push("behind a password".into()),
        None => notes.push("open to anyone with the address".into()),
    }
    if share.pinned {
        notes.push("kept running".into());
    }
    match share.expires_in_secs {
        Some(secs) => notes.push(format!("ends in {}", duration(app.idle_in(secs)))),
        None => notes.push(format!(
            "shared for {}",
            duration(share.shared_secs + app.now.duration_since(app.snapshot_at).as_secs())
        )),
    }
    lines.push(Line::from(notes.join(" · ")).dark_gray());
    let visits: Vec<_> = share
        .requests
        .iter()
        .filter(|r| r.app == instance.app)
        .collect();
    for request in visits.iter().rev().take(3).rev() {
        let line = Line::from(format!("← {} {}", request.method, request.path));
        lines.push(if request.refused {
            line.red()
        } else {
            line.dark_gray()
        });
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
        NodeId::Folder { .. } | NodeId::Project { .. } | NodeId::Stack { .. } => None,
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
    app.logs_area = area;
    frame.render_widget(block, area);
    app.logs.height = inner.height as usize;
    let logs = &app.logs;

    let window = logs.window_numbered(inner.height as usize);
    if window.is_empty() {
        let stopped = app
            .selected_node()
            .is_some_and(|n| matches!(n.status.state, State::Stopped | State::Disabled));
        let message = match (&logs.ended, logs.is_empty()) {
            (Some(reason), _) => format!("No logs: {reason}"),
            (None, false) => "No lines match the filter.".into(),
            (None, true) if logs.key.is_some() && stopped => "Not running; s starts it.".into(),
            (None, true) if logs.key.is_some() => "No output yet.".into(),
            (None, true)
                if app
                    .selected_node()
                    .is_some_and(|n| matches!(n.id(), NodeId::Folder { .. })) =>
            {
                "A folder has no logs; select a project.".into()
            }
            (None, true) => String::new(),
        };
        frame.render_widget(Paragraph::new(message.dark_gray()), inner);
        return;
    }
    let width = window
        .iter()
        .filter_map(|(_, line)| logs.prefix.label(line))
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = window
        .iter()
        .map(|(seq, line)| {
            let mut spans = Vec::new();
            if let Some(label) = logs.prefix.label(line) {
                let color = PREFIX_COLORS[logs.color_index(&label) % PREFIX_COLORS.len()];
                spans.push(Span::raw(format!("{label:width$} ")).fg(color));
            }
            spans.extend(highlight(&line.text, &logs.filter));
            let line = Line::from(spans);
            if app.selection.is_some_and(|s| s.contains(*seq)) {
                line.reversed()
            } else {
                line
            }
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
            ("a", "show"),
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
        spans.push(Span::raw(*key).blue().bold());
        spans.push(Span::raw(format!(" {what}  ")));
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
                Span::raw(format!("{key:width$}  ")).blue(),
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
    let tunnel = app
        .snapshot
        .as_ref()
        .and_then(|s| s.tunnel.as_ref())
        .filter(|t| t.state == TunnelState::Failed);
    let mut lines = if problems.is_empty() && tunnel.is_none() {
        vec![Line::from("No problems.")]
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
    if let Some(tunnel) = tunnel {
        lines.push(Line::from(vec![
            Span::raw(format!("{} tunnel: ", tunnel.provider)).bold(),
            Span::raw(tunnel.message.clone().unwrap_or_default()),
        ]));
    }
    popup(frame, area, "problems", lines);
}

fn render_confirm(frame: &mut Frame, area: Rect, app: &App, confirm: &Confirm) {
    let (title, question, verb) = match confirm {
        Confirm::StopAll => {
            let running = app.snapshot.as_ref().map_or(0, |s| s.running_instances);
            (
                "stop everything",
                format!("Stop all {running} running instances, pinned ones too?"),
                " stop   ",
            )
        }
        Confirm::ReplaceShare { request, shared } => {
            let provider = app
                .snapshot
                .as_ref()
                .and_then(|s| s.tunnel.as_ref())
                .map_or("the provider", |t| t.provider.as_str());
            (
                "share",
                format!(
                    "{provider} shares one app at a time. Stop sharing {shared} and share {} instead?",
                    request.target.address
                ),
                " switch   ",
            )
        }
    };
    popup(
        frame,
        area,
        title,
        vec![
            Line::from(question),
            Line::default(),
            Line::from(vec![
                Span::raw("y").blue().bold(),
                Span::raw(verb),
                Span::raw("any other key").blue().bold(),
                Span::raw(" cancel"),
            ]),
        ],
    );
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use carte_api::{Hold, InstanceState, LogLine, ProcessState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::tui::app::{Command, Event};
    use crate::tui::fixtures::{process, sample, shared_sample};

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let line: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, now: Instant, code: KeyCode) -> Vec<Command> {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), now)
    }

    fn busy_app() -> App {
        let mut snapshot = sample();
        snapshot.running_instances = 2;
        snapshot.problems.push(Problem {
            project: Some("nova".into()),
            app: Some("nova-frontend".into()),
            message: "branch slug \"billing\" names an app".into(),
        });
        let nova = &mut snapshot.projects[0];
        let billing = &mut nova.stacks[0].instances[0];
        billing.processes[1].policy = Policy::Pinned;
        billing.processes[1].temporary = true;
        billing.holds = vec![Hold::Pinned];
        billing.memory_bytes = 412 * 1024 * 1024;
        billing.cpu_percent = 3.2;
        let frontend = &mut nova.stacks[0].instances[1];
        frontend.state = InstanceState::Crashed;
        frontend.last_error = Some("web exited with code 1 before it was ready".into());
        frontend.processes = vec![process(
            "web",
            ProcessState::Exited {
                code: Some(1),
                signal: None,
            },
        )];
        let feature = &mut nova.stacks[1].instances[1];
        feature.state = InstanceState::Ready;
        feature.idle_in_secs = Some(840);
        feature.processes = vec![process("web", ProcessState::Running)];
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        app
    }

    fn log(app: &mut App, generation: u64, process: &str, text: &str) {
        app.update(
            Event::Log {
                generation,
                line: LogLine {
                    project: "nova".into(),
                    app: "nova-billing".into(),
                    stack: "main".into(),
                    process: process.into(),
                    at_ms: 0,
                    text: text.into(),
                },
            },
            app.now,
        );
    }

    #[test]
    fn renders_the_tree_details_and_merged_logs() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        let generation = app.logs.generation;
        log(
            &mut app,
            generation,
            "web",
            "Puma starting in single mode...",
        );
        log(
            &mut app,
            generation,
            "worker",
            "Started worker with 5 threads",
        );
        log(&mut app, generation, "web", "GET /up 200 in 2ms");
        let screen = draw(&mut app, 120, 18);
        assert_eq!(
            screen,
            "\
╭ carte · 2 running · 1 problem (!) ─────────────────╮╭ nova-billing@main ─────────────────────────────────────────────╮
│                                         CPU   MEM  ││● running · https://nova-billing.test                           │
│▾ ✖ nova !              2/4                 3%  412M││checkout main · /work/nova-billing                              │
│  ▾ ✖ main              1/2                 3%  412M││worker pinned until the daemon restarts                         │
│    ▾ ● billing pin*    ready               3%  412M│╰────────────────────────────────────────────────────────────────╯
│        ● web           running             0%    0B│╭ logs following ────────────────────────────────────────────────╮
│        ● worker pin*   running             0%    0B││web    Puma starting in single mode...                          │
│      ✖ frontend !      crashed                     ││worker Started worker with 5 threads                            │
│  ▾ ● feature-x         1/2                 0%    0B││web    GET /up 200 in 2ms                                       │
│      ○ billing main                                ││                                                                │
│      ● frontend !      idle in 14m         0%    0B││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
╰────────────────────────────────────────────────────╯╰────────────────────────────────────────────────────────────────╯

 ? help  q quit  s start  x stop  r restart  p pin  d disable  / filter  a show  tab logs"
        );
    }

    #[test]
    fn renders_a_share_and_its_visitors() {
        let mut snapshot = shared_sample();
        snapshot.shares[0].requests = ["/", "/login", "/admin", "/admin"]
            .iter()
            .enumerate()
            .map(|(i, path)| carte_api::ExternalRequest {
                at_ms: i as u64,
                app: "nova-billing".into(),
                method: "GET".into(),
                path: path.to_string(),
                refused: i == 3,
            })
            .collect();
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        let screen = draw(&mut app, 120, 18);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(
            lines[0].contains("carte · 0 running · 1 shared, online"),
            "{screen}"
        );
        for wanted in [
            "shared at https://nova-billing--main--nova.demo.dev",
            "behind a password · shared for 2m",
            "← GET /login",
            "← GET /admin",
        ] {
            assert!(screen.contains(wanted), "{wanted:?} in\n{screen}");
        }
        assert!(!screen.contains("← GET / "), "only the last three visits");
        assert!(screen.contains("billing shared"), "{screen}");
    }

    #[test]
    fn the_tree_scrolls_only_past_its_edges() {
        let mut app = busy_app();
        let now = app.now;
        draw(&mut app, 80, 18);
        for _ in 0..6 {
            press(&mut app, now, KeyCode::Down);
            draw(&mut app, 80, 18);
        }
        let offset = app.tree_offset;
        assert!(offset > 0, "the bottom row scrolled into view");
        press(&mut app, now, KeyCode::Up);
        draw(&mut app, 80, 18);
        assert_eq!(
            app.tree_offset, offset,
            "moving up inside the view keeps it"
        );
        for _ in 0..6 {
            press(&mut app, now, KeyCode::Up);
            draw(&mut app, 80, 18);
        }
        assert_eq!(app.tree_offset, 0);
    }

    #[test]
    fn renders_a_crash_and_its_problem() {
        let mut app = busy_app();
        let now = app.now;
        for _ in 0..3 {
            press(&mut app, now, KeyCode::Down);
        }
        let screen = draw(&mut app, 80, 24);
        assert_eq!(
            screen,
            "\
╭ carte · 2 running · 1 problem (!) ───────────────────────────────────────────╮
│                                                                   CPU   MEM  │
│▾ ✖ nova !                                        2/4                 3%  412M│
│  ▾ ✖ main                                        1/2                 3%  412M│
│    ▸ ● billing pin*                              ready               3%  412M│
│      ✖ frontend !                                crashed                     │
│  ▾ ● feature-x                                   1/2                 0%    0B│
│      ○ billing main                                                          │
│      ● frontend !                                idle in 14m         0%    0B│
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ nova-frontend@main ──────────────────────────────────────────────────────────╮
│✖ crashed · https://nova-frontend.test                                        │
│checkout main · /work/nova-frontend                                           │
│web exited with code 1 before it was ready                                    │
│! branch slug \"billing\" names an app                                          │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ logs following ──────────────────────────────────────────────────────────────╮
│No output yet.                                                                │
│                                                                              │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯

 ? help  q quit  s start  x stop  r restart  p pin  d disable  / filter  a show"
        );
    }

    #[test]
    fn renders_the_wait_for_the_daemon() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(
            Event::Disconnected("the carte daemon isn't running".into()),
            now,
        );
        let screen = draw(&mut app, 80, 12);
        assert_eq!(
            screen,
            "\
╭ carte ───────────────────────────────────────────────────────────────────────╮
│                                                                              │
│                       The carte daemon isn't reachable.                      │
│                                                                              │
│                        the carte daemon isn't running                        │
│                                                                              │
│                            Retrying every second.                            │
│                                                                              │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
 reconnecting to the daemon…
 ? help  q quit  s start  x stop  r restart  p pin  d disable  / filter  a show"
        );
    }

    #[test]
    fn renders_help() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('?'));
        let screen = draw(&mut app, 100, 41);
        assert_eq!(
            screen,
            "\
╭ carte · 2 running · 1 problem (!) ───────────────────────────────────────────────────────────────╮
│                                                                                       CPU   MEM  │
│▾ ✖ nova !              ╭ keys ──────────────────────────────────────────╮                3%  412M│
│  ▾ ✖ main              │ Navigate                                       │                3%  412M│
│    ▸ ● billing pin*    │ ↑↓ j k       move                              │y               3%  412M│
│      ✖ frontend !      │ ←→ h l       fold, unfold                      │hed                     │
│  ▾ ● feature-x         │ enter space  fold or unfold                    │                0%    0B│
│      ○ billing main    │ g G          top, bottom                       │                        │
│      ● frontend !      │ PgUp PgDn    page                              │ in 14m         0%    0B│
│                        │ tab          switch between tree and logs      │                        │
│                        │ /            filter the focused pane           │                        │
│                        │ esc          clear the filter                  │                        │
│                        │ a            show all, running or shared       │                        │
│                        │ drag         copy log lines                    │                        │
│                        │ m            mouse on or off                   │                        │
│                        │                                                │                        │
│                        │ Control the selection                          │                        │
│                        │ s            start                             │                        │
│                        │ x            stop                              │                        │
╰────────────────────────│ r            restart                           │────────────────────────╯
╭ nova ──────────────────│ p            pin or unpin                      │────────────────────────╮
│✖ 2 of 4 instances runni│ d            disable or enable                 │                        │
│crashed: frontend       │ S            save a temporary pin or disable   │                        │
│! branch slug \"billing\" │ o            open in the browser               │                        │
╰────────────────────────│ w            share on the web, or stop sharing │────────────────────────╯
╭ logs following ────────│ W            share behind a password           │────────────────────────╮
│No output yet.          │ O            open the shared address           │                        │
│                        │                                                │                        │
│                        │ Everything                                     │                        │
│                        │ I            stop idle instances               │                        │
│                        │ X            stop all instances                │                        │
│                        │ !            problems                          │                        │
│                        │ ?            this help                         │                        │
│                        │ q            quit                              │                        │
│                        │                                                │                        │
│                        │ pin* and off* last until the daemon restarts   │                        │
│                        ╰────────────────────────────────────────────────╯                        │
│                                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯

 ? help  q quit  s start  x stop  r restart  p pin  d disable  / filter  a show  tab logs"
        );
    }
}
