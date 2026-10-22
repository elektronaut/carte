//! Draws the app state.

use std::time::Duration;

use carte_api::{
    BootProgress, Event, InstanceStatus, Policy, Problem, ProcessState, ProcessStatus, TunnelState,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, Connection};
use super::browse::Browse;
use super::input::{Confirm, Focus, HELP, Mode};
use super::jump;
use super::target;
use super::theme::Theme;
use super::tree::{self, Node, NodeId, Show, Source, State, Status};

pub fn render(frame: &mut Frame, app: &mut App) {
    let theme = app.theme.clone();
    let theme = &theme;
    frame.render_widget(Block::new().style(theme.base()), frame.area());
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
        render_tree(theme, frame, left, app);
        render_selection(theme, frame, right, app);
    } else {
        render_waiting(theme, frame, main, app);
    }
    frame.render_widget(status_line(theme, app, status.width), status);
    frame.render_widget(
        footer_line(theme, app.focus, app.events, footer.width),
        footer,
    );

    match &app.mode {
        Mode::Help => render_help(theme, frame, main),
        Mode::Problems => render_problems(theme, frame, main, app),
        Mode::Confirm(confirm) => render_confirm(theme, frame, main, app, confirm),
        Mode::Env => render_env(theme, frame, main, app),
        Mode::Jump => render_jump(theme, frame, main, app),
        Mode::Normal | Mode::Filter | Mode::Type => {}
    }
}

fn pane(theme: &Theme, title: Line<'static>, focused: bool) -> Block<'static> {
    let (border, title_color) = if focused {
        (theme.border_focus, theme.border_focus)
    } else {
        (theme.border, theme.fg)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .style(theme.base())
        .title_style(Style::new().fg(title_color))
        .title(title)
}

fn render_waiting(theme: &Theme, frame: &mut Frame, area: Rect, app: &App) {
    let lines = match &app.connection {
        Connection::Lost(message) => vec![
            Line::from("The carte daemon isn't reachable.".bold()),
            Line::default(),
            Line::from(message.clone()),
            Line::default(),
            Line::from("Retrying every second.".fg(theme.muted)),
        ],
        Connection::Connecting | Connection::Connected => {
            vec![Line::from("Connecting to the carte daemon…")]
        }
    };
    let height = lines.len() as u16 + 2;
    let block = pane(theme, Line::from(" carte "), false);
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

fn render_tree(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    let mut title = vec![
        Span::raw(" carte "),
        Span::raw(format!("· {} running ", snapshot.running_instances)).fg(theme.muted),
    ];
    match app.fold.show {
        Show::All => {}
        Show::Running => title.push(Span::raw("· running only ").fg(theme.muted)),
        Show::Shared => title.push(Span::raw("· shared only ").fg(theme.muted)),
    }
    if !snapshot.shares.is_empty()
        && let Some(tunnel) = &snapshot.tunnel
    {
        let shared: usize = snapshot.shares.iter().map(|s| s.apps.len()).sum();
        let (word, color) = match tunnel.state {
            TunnelState::Connected => ("online", theme.shared),
            TunnelState::Starting => ("connecting", theme.warn),
            TunnelState::Failed => ("tunnel down (!)", theme.error),
            TunnelState::Idle => ("", theme.muted),
            TunnelState::Unknown => ("tunnel unknown", theme.muted),
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
        title.push(Span::raw(format!("· {count} {noun} (!) ")).fg(theme.error));
    }
    let block = pane(theme, Line::from(title), app.focus == Focus::Tree);
    app.tree_height = block.inner(area).height.saturating_sub(1) as usize;

    let rows: Vec<Row> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let node = tree::node(&app.roots, &row.path);
            tree_row(theme, app, node, row, i == app.cursor)
        })
        .collect();
    let header = Row::new(["", "", "CPU", "MEM"]).fg(theme.muted);
    let selected_style = if app.focus == Focus::Tree {
        Style::new().bg(theme.selection_bg)
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
            ..pane(theme, Line::default(), false).inner(area)
        };
        frame.render_widget(Paragraph::new(message.fg(theme.muted)).centered(), inner);
    }
}

/// On the selected row, `muted` text turns `selection_fg`, so it stays readable on
/// the selection's background.
fn tree_row(
    theme: &Theme,
    app: &App,
    node: &Node,
    row: &tree::Row,
    selected: bool,
) -> Row<'static> {
    let status = &node.status;
    let aggregate = matches!(
        node.id(),
        NodeId::Folder { .. } | NodeId::Project { .. } | NodeId::Stack { .. }
    );
    let (symbol, color) = symbol(theme, status.state);
    let mut spans = vec![
        Span::raw("  ".repeat(row.depth)),
        Span::raw(match (row.expandable, row.expanded) {
            (false, _) => "  ",
            (true, true) => "▾ ",
            (true, false) => "▸ ",
        })
        .fg(theme.muted),
        Span::raw(symbol).fg(color),
        Span::raw(" "),
    ];
    let label = Span::raw(node.label.clone());
    spans.push(match &node.ids[0] {
        NodeId::Folder { .. } => label.bold(),
        NodeId::Project { .. } => label.bold(),
        _ if status.state == State::Disabled => label.fg(theme.muted),
        NodeId::Stack { .. } => label.fg(theme.accent).bold(),
        _ if status.kube && status.state == State::Stopped => label.fg(theme.muted),
        _ => label,
    });
    spans.extend(badges(theme, status, aggregate));

    let detail = if let NodeId::Project { project } = node.id()
        && let Some(kube) = app.kube_project(project)
    {
        match (app.browse.get(project), &kube.error) {
            (Some(Browse::Loading(_)), _) => Span::raw("⎈ listing…").fg(theme.warn),
            (Some(Browse::Failed(error)), _) | (_, Some(error)) => {
                Span::raw(format!("⎈ {error}")).fg(theme.error)
            }
            _ => Span::raw(format!("⎈ {}", kube.context)).fg(theme.muted),
        }
    } else if status.kube && matches!(node.id(), NodeId::Stack { .. }) && status.running == 0 {
        Span::raw(match status.total {
            1 => "1 service".to_string(),
            n => format!("{n} services"),
        })
        .fg(theme.muted)
    } else if aggregate {
        Span::raw(if status.running > 0 {
            format!("{}/{}", status.running, status.total)
        } else {
            String::new()
        })
        .fg(theme.muted)
    } else {
        detail(theme, app, node.id(), status)
    };
    let (cpu, memory) = if status.running > 0 && !status.kube {
        (
            format!("{:.0}%", status.cpu_percent),
            format_bytes(status.memory_bytes),
        )
    } else {
        (String::new(), String::new())
    };
    let mut detail = vec![detail];
    if selected {
        for span in spans.iter_mut().chain(&mut detail) {
            if span.style.fg == Some(theme.muted) {
                span.style.fg = Some(theme.selection_fg);
            }
        }
    }
    Row::new([
        Cell::from(Line::from(spans)),
        Cell::from(Line::from(detail)),
        Cell::from(Line::from(cpu).right_aligned()),
        Cell::from(Line::from(memory).right_aligned()),
    ])
}

fn symbol(theme: &Theme, state: State) -> (&'static str, Color) {
    match state {
        State::Ready => ("●", theme.ok),
        State::Starting | State::Restarting => ("◐", theme.warn),
        State::Paused => ("◆", theme.paused),
        State::Suspended => ("◆", theme.warn),
        State::Stopping => ("◑", theme.warn),
        State::Stopped | State::Exited => ("○", theme.muted),
        State::Done => ("✓", theme.muted),
        State::Crashed => ("✖", theme.error),
        State::Disabled => ("⊘", theme.muted),
        State::Unknown => ("?", theme.muted),
    }
}

/// Policies show on the instances and processes they apply to, not on groups.
fn badges(theme: &Theme, status: &Status, aggregate: bool) -> Vec<Span<'static>> {
    let marked = |word: &str, source: Source| match source {
        Source::Config => word.to_string(),
        Source::Temporary => format!("{word}*"),
    };
    let mut spans = Vec::new();
    if aggregate {
        if status.problems > 0 {
            spans.push(Span::raw(" !").fg(theme.error).bold());
        }
        return spans;
    }
    if status.fallback {
        spans.push(Span::raw(" main").fg(theme.muted));
    }
    if let Some(source) = status.pinned {
        spans.push(Span::raw(format!(" {}", marked("pin", source))).fg(theme.accent));
    }
    if let Some(source) = status.disabled {
        spans.push(Span::raw(format!(" {}", marked("off", source))).fg(theme.muted));
    }
    if status.shared {
        spans.push(Span::raw(" shared").fg(theme.shared));
    }
    if status.gave_up && status.state != State::Crashed {
        spans.push(Span::raw(" gave up").fg(theme.error));
    }
    if status.problems > 0 {
        spans.push(Span::raw(" !").fg(theme.error).bold());
    }
    spans
}

fn detail(theme: &Theme, app: &App, id: &NodeId, status: &Status) -> Span<'static> {
    let process = matches!(id, NodeId::Process { .. });
    match status.state {
        State::Ready if !status.held_by.is_empty() => {
            let more = match status.held_by.len() {
                1 => String::new(),
                n => format!(" +{}", n - 1),
            };
            Span::raw(format!("held by {}{more}", status.held_by[0])).fg(theme.muted)
        }
        State::Ready => match status.idle_in_secs {
            Some(secs) => {
                Span::raw(format!("idle in {}", duration(app.idle_in(secs)))).fg(theme.muted)
            }
            None if process => Span::raw("running").fg(theme.ok),
            None => Span::raw("ready").fg(theme.ok),
        },
        State::Starting => Span::raw("starting").fg(theme.warn),
        State::Restarting => match status.restart_in_secs {
            Some(secs) => {
                Span::raw(format!("restarting in {}", duration(app.idle_in(secs)))).fg(theme.warn)
            }
            None => Span::raw("restarting").fg(theme.warn),
        },
        State::Paused => Span::raw("paused").fg(theme.paused),
        State::Suspended => Span::raw("suspended").fg(theme.warn),
        State::Stopping => Span::raw("stopping").fg(theme.warn),
        State::Crashed if process && status.gave_up => Span::raw("gave up").fg(theme.error),
        State::Crashed if process => match status.restart_in_secs {
            Some(secs) => {
                Span::raw(format!("restarting in {}", duration(app.idle_in(secs)))).fg(theme.warn)
            }
            None => Span::raw("exited").fg(theme.error),
        },
        State::Crashed if status.kube => Span::raw("failed").fg(theme.error),
        State::Crashed => Span::raw("crashed").fg(theme.error),
        State::Exited => Span::raw("exited").fg(theme.muted),
        State::Done => Span::raw("done").fg(theme.muted),
        State::Disabled => Span::raw("disabled").fg(theme.muted),
        State::Unknown => Span::raw("unknown").fg(theme.muted),
        State::Stopped => Span::raw(""),
    }
}

/// `starting for 12s · waiting for web to answer /up · running: worker`
fn boot_progress(app: &App, instance: &InstanceStatus, boot: &BootProgress) -> String {
    let elapsed = Duration::from_millis(boot.elapsed_ms) + app.now.duration_since(app.snapshot_at);
    let mut text = format!(
        "starting for {} · waiting for {}",
        duration(elapsed.as_secs()),
        boot.waiting_for
    );
    let running: Vec<&str> = instance
        .processes
        .iter()
        .filter(|p| !p.http && p.state == ProcessState::Running)
        .map(|p| p.name.as_str())
        .collect();
    if !running.is_empty() {
        text.push_str(&format!(" · running: {}", running.join(", ")));
    }
    text
}

fn duration(secs: u64) -> String {
    carte_core::duration::format(Duration::from_secs(secs))
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    match bytes {
        b if b < KB => format!("{b}B"),
        b if b < KB * KB => format!("{}K", b / KB),
        b if b < KB * KB * KB => format!("{}M", b / (KB * KB)),
        b => format!("{:.1}G", b as f64 / (KB * KB * KB) as f64),
    }
}

// Selection: details and logs

fn render_selection(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(node) = app.selected_node() else {
        render_logs(theme, frame, area, app);
        return;
    };
    let title = format!(" {} ", target::label(node.id()));
    let paragraph = Paragraph::new(details(theme, app, node))
        .wrap(Wrap { trim: false })
        .block(pane(theme, Line::from(title).bold(), false));
    let height = bordered_height(&paragraph, area.width).min(area.height.saturating_sub(4));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(area);
    frame.render_widget(paragraph, top);
    render_logs(theme, frame, bottom, app);
}

/// The rows a wrapped paragraph in a bordered pane takes at `width`, borders
/// included.
fn bordered_height(paragraph: &Paragraph, width: u16) -> u16 {
    u16::try_from(paragraph.line_count(width.saturating_sub(2))).unwrap_or(u16::MAX)
}

fn details(theme: &Theme, app: &App, node: &Node) -> Vec<Line<'static>> {
    let status = &node.status;
    let (symbol, color) = symbol(theme, status.state);
    let state_word = match status.state {
        State::Ready => "running",
        State::Starting => "starting",
        State::Restarting => "restarting",
        State::Paused => "paused at a breakpoint",
        State::Suspended => "suspended by a signal",
        State::Stopping => "stopping",
        State::Stopped => "stopped",
        State::Exited => "exited",
        State::Done => "done",
        State::Crashed if status.kube => "failed",
        State::Crashed => "crashed",
        State::Disabled => "disabled",
        State::Unknown => "unknown",
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
                first.push(Span::raw(format!(" · branch {branch}")).fg(theme.muted));
            }
            lines.push(Line::from(first));
            if let NodeId::Project { project } = id
                && let Some(kube) = app.kube_project(project)
            {
                lines.push(Line::from(format!("kube context {}", kube.context)).fg(theme.muted));
                if let Some(error) = &kube.error {
                    lines.push(Line::from(error.clone()).fg(theme.error));
                }
                if let Some(Browse::Failed(error)) = app.browse.get(project) {
                    lines.push(
                        Line::from(format!("listing Services failed: {error}")).fg(theme.error),
                    );
                }
            }
            let crashed: Vec<&str> = crashed_below(node);
            if !crashed.is_empty() {
                let word = if status.kube { "failed" } else { "crashed" };
                lines.push(Line::from(
                    format!("{word}: {}", crashed.join(", ")).fg(theme.error),
                ));
            }
        }
        NodeId::Instance { .. } => {
            if let Some(instance) = app.instance(id) {
                lines.extend(instance_details(
                    theme,
                    app,
                    id.project(),
                    instance,
                    state,
                    status,
                ));
            }
        }
        NodeId::Process { process, .. } => {
            if let Some(instance) = app.instance(id)
                && let Some(proc) = instance.processes.iter().find(|p| p.name == *process)
            {
                lines.extend(process_details(theme, app, instance, proc, state));
            }
        }
    }
    if let Some(event) = latest_event(app, id) {
        lines.push(Line::from(event_note(event, crate::clock::now_ms())).fg(theme.muted));
    }
    for problem in problems_for(app, id) {
        lines.push(Line::from(format!("! {}", problem.message)).fg(theme.error));
    }
    lines
}

/// The newest event about the node or anything under it.
fn latest_event<'a>(app: &'a App, id: &NodeId) -> Option<&'a Event> {
    let (folder, project, stack, app_name) = match id {
        NodeId::Folder { folder } => (Some(folder.as_str()), None, None, None),
        NodeId::Project { project } => (None, Some(project.as_str()), None, None),
        NodeId::Stack { project, stack } => (None, Some(project.as_str()), Some(stack), None),
        NodeId::Instance {
            project,
            stack,
            app,
        }
        | NodeId::Process {
            project,
            stack,
            app,
            ..
        } => (None, Some(project.as_str()), Some(stack), Some(app)),
    };
    app.snapshot
        .iter()
        .flat_map(|s| &s.projects)
        .filter(|p| project.is_none_or(|name| p.name == name))
        .filter(|p| {
            folder.is_none_or(|f| {
                p.folder.as_deref().is_some_and(|pf| {
                    pf.strip_prefix(f)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                })
            })
        })
        .flat_map(|p| &p.stacks)
        .filter(|s| stack.is_none_or(|name| s.name == *name))
        .flat_map(|s| &s.instances)
        .filter(|i| app_name.is_none_or(|name| i.app == *name))
        .filter_map(|i| i.last_event.as_ref())
        .max_by_key(|e| e.seq)
}

/// `stopped 3m ago: idle for 15m`, naming what it's about when that isn't obvious.
fn event_note(event: &Event, now_ms: u64) -> String {
    let ago = crate::clock::ago(event.at_ms, now_ms);
    let mut note = format!("{} {ago} ago", event.kind.words());
    if !event.message.is_empty() {
        note.push_str(&format!(": {}", event.message));
    }
    note
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
    theme: &Theme,
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
    if let Some(boot) = &instance.boot {
        lines.push(Line::from(boot_progress(app, instance, boot)).fg(theme.warn));
    }
    lines.extend(share_details(theme, app, project, instance));
    if let Some(kube) = &instance.kube {
        let place = match (&kube.pod, kube.port) {
            (Some(pod), Some(port)) => format!("pod {pod} :{port} · namespace {}", instance.stack),
            _ => format!("namespace {}", instance.stack),
        };
        lines.push(Line::from(place.fg(theme.muted)));
    } else {
        let checkout = if status.fallback {
            "main (no worktree for this stack)".to_string()
        } else {
            instance.checkout.clone()
        };
        let mut checkout = format!("checkout {checkout}");
        if !instance.checkout_path.as_os_str().is_empty() {
            checkout.push_str(&format!(" · {}", instance.checkout_path.display()));
        }
        lines.push(Line::from(checkout.fg(theme.muted)));
    }
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
        lines.push(Line::from(notes.join(" · ")).fg(theme.muted));
    }
    if let Some(error) = &instance.last_error
        && (status.state == State::Crashed || status.kube)
    {
        lines.push(Line::from(error.clone()).fg(theme.error));
    }
    lines
}

/// Where a shared instance is public, how it's guarded, and its latest visitors.
fn share_details(
    theme: &Theme,
    app: &App,
    project: &str,
    instance: &InstanceStatus,
) -> Vec<Line<'static>> {
    let Some(public) = &instance.public_url else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(vec![
        Span::raw("shared at ").fg(theme.shared),
        Span::raw(public.clone()).underlined(),
    ])];
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    if let Some(tunnel) = &snapshot.tunnel
        && tunnel.state == TunnelState::Failed
    {
        let message = tunnel.message.as_deref().unwrap_or("no reason given");
        lines.push(Line::from(format!("the tunnel is down: {message}")).fg(theme.error));
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
    lines.push(Line::from(notes.join(" · ")).fg(theme.muted));
    let visits: Vec<_> = share
        .requests
        .iter()
        .filter(|r| r.app == instance.app)
        .collect();
    for request in visits.iter().rev().take(3).rev() {
        let line = Line::from(format!("← {} {}", request.method, request.path));
        lines.push(if request.refused {
            line.fg(theme.error)
        } else {
            line.fg(theme.muted)
        });
    }
    lines
}

fn process_details(
    theme: &Theme,
    app: &App,
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
        first.push(Span::raw(format!(" · {fact}")).fg(theme.muted));
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
            line.fg(theme.muted)
        } else {
            line.fg(theme.error)
        });
    }
    if let Some(secs) = process.restart_in_secs {
        lines.push(
            Line::from(format!("restarting in {}", duration(app.idle_in(secs)))).fg(theme.warn),
        );
    }
    if process.gave_up {
        lines.push(
            Line::from(
                "it crashed too often, so carte stopped restarting it; start it to try again",
            )
            .fg(theme.error),
        );
    }
    if let Some(error) = &instance.last_error
        && !matches!(process.state, ProcessState::Running | ProcessState::Done)
    {
        lines.push(Line::from(error.clone()).fg(theme.error));
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

fn render_logs(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let logs = &app.logs;
    let mut title = vec![Span::raw(if app.events { " events " } else { " logs " })];
    if !logs.search.is_empty() {
        let seqs = logs.match_seqs();
        let at = logs
            .current
            .and_then(|seq| seqs.iter().position(|&s| s == seq))
            .map_or(String::new(), |i| format!("{}/", i + 1));
        title.push(Span::raw(format!("/{} {at}{} ", logs.search, seqs.len())).fg(theme.warn));
    }
    title.push(if logs.scroll > 0 {
        Span::raw(format!("↑{} ", logs.scroll)).fg(theme.warn)
    } else {
        Span::raw("following ").fg(theme.muted)
    });
    let block = pane(theme, Line::from(title), app.focus == Focus::Logs);
    let inner = block.inner(area);
    app.logs_area = area;
    frame.render_widget(block, area);
    app.logs.height = inner.height as usize;
    app.logs.width = inner.width as usize;
    let logs = &app.logs;

    let window = logs.window_rows(inner.height as usize);
    if window.lines.is_empty() {
        let stopped = app
            .selected_node()
            .is_some_and(|n| matches!(n.status.state, State::Stopped | State::Disabled));
        let message = match (&logs.ended, logs.is_empty()) {
            (Some(reason), _) if app.events => format!("No events: {reason}"),
            (Some(reason), _) => format!("No logs: {reason}"),
            (None, _) if app.events => "Nothing has happened here yet.".into(),
            (None, _) if logs.key.is_some() && stopped => "Not running; s starts it.".into(),
            (None, _) if logs.key.is_some() => "No output yet.".into(),
            (None, _)
                if app
                    .selected_node()
                    .is_some_and(|n| matches!(n.id(), NodeId::Folder { .. })) =>
            {
                "A folder has no logs; select a project.".into()
            }
            (None, _) => String::new(),
        };
        frame.render_widget(Paragraph::new(message.fg(theme.muted)), inner);
        return;
    }
    let room = (inner.width as usize).saturating_sub(window.indent).max(1);
    let mut rows: Vec<Line> = Vec::new();
    for (seq, line) in &window.lines {
        let selected = app.selection.is_some_and(|s| s.contains(*seq));
        let mut line_rows = if line.marker {
            vec![Line::from(
                Span::raw(crate::logs::marker_rule(line.at_ms, inner.width as usize)).bold(),
            )]
        } else {
            let text = highlight(theme, &line.text, &logs.search, logs.current == Some(*seq));
            let label = logs.prefix.label(line);
            wrap_spans(text, room)
                .into_iter()
                .enumerate()
                .map(|(i, mut spans)| {
                    let lead = match (&label, i) {
                        (Some(label), 0) => {
                            let color = theme.prefix(logs.color_index(label));
                            Span::raw(format!("{label:<width$}", width = window.indent)).fg(color)
                        }
                        _ => Span::raw(" ".repeat(window.indent)),
                    };
                    spans.insert(0, lead);
                    Line::from(spans)
                })
                .collect()
        };
        if selected {
            line_rows = line_rows.into_iter().map(Line::reversed).collect();
        }
        rows.extend(line_rows);
    }
    let rows: Vec<Line> = rows.into_iter().skip(window.skip).collect();
    frame.render_widget(Paragraph::new(rows), inner);
}

/// Splits styled text into rows of at most `room` columns, by character, the way
/// `Logs::rows` counts them.
fn wrap_spans(spans: Vec<Span<'static>>, room: usize) -> Vec<Vec<Span<'static>>> {
    use unicode_width::UnicodeWidthChar;
    let mut rows = vec![Vec::new()];
    let mut used = 0;
    for span in spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            let width = c.width().unwrap_or(0);
            if used + width > room && used > 0 {
                if !piece.is_empty() {
                    let row = rows.last_mut().expect("a row");
                    row.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                rows.push(Vec::new());
                used = 0;
            }
            piece.push(c);
            used += width;
        }
        if !piece.is_empty() {
            rows.last_mut()
                .expect("a row")
                .push(Span::styled(piece, span.style));
        }
    }
    rows
}

/// Marks every match of the search in a line, more strongly on the current match's
/// line.
fn highlight(theme: &Theme, text: &str, search: &str, current: bool) -> Vec<Span<'static>> {
    if search.is_empty() {
        return vec![Span::raw(text.to_string())];
    }
    let lower = text.to_ascii_lowercase();
    let needle = search.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        spans.push(Span::raw(text[from..start].to_string()));
        let matched = Span::raw(text[start..end].to_string());
        spans.push(matched.style(theme.search_match(current)));
        from = end;
    }
    spans.push(Span::raw(text[from..].to_string()));
    spans
}

// Status, footer and popups

/// The typing line keeps the detach keys in view and shows the end of a long line.
fn status_line(theme: &Theme, app: &App, width: u16) -> Line<'static> {
    if let Some(typing) = &app.typing {
        let keys = format!(" {} stops ", app.detach_keys);
        if !typing.attached {
            return Line::from(vec![
                Span::raw(keys).fg(theme.muted),
                Span::raw(format!("attaching to {}…", typing.label)).fg(theme.warn),
            ]);
        }
        let label = format!("{} › ", typing.label);
        let room = usize::from(width)
            .saturating_sub(keys.chars().count() + label.chars().count() + 1)
            .max(1);
        let pending: Vec<char> = typing.pending().chars().collect();
        let shown: String = pending[pending.len().saturating_sub(room)..]
            .iter()
            .collect();
        return Line::from(vec![
            Span::raw(keys).fg(theme.muted),
            Span::raw(label).fg(theme.warn),
            Span::raw(format!("{shown}▏")),
        ]);
    }
    if app.mode == Mode::Filter {
        let (what, filter) = match app.focus {
            Focus::Tree => ("filter tree", &app.fold.filter),
            Focus::Logs => ("search logs", &app.logs.search),
        };
        return Line::from(vec![
            Span::raw(format!(" {what}: ")).fg(theme.muted),
            Span::raw(format!("{filter}▏")).fg(theme.warn),
            Span::raw("  enter keeps · esc clears").fg(theme.muted),
        ]);
    }
    if let Some(notice) = &app.notice {
        let text = Span::raw(format!(" {}", notice.text));
        return Line::from(if notice.error {
            text.fg(theme.error)
        } else {
            text.fg(theme.ok)
        });
    }
    if let Connection::Lost(_) = app.connection {
        return Line::from(" reconnecting to the daemon…".fg(theme.warn));
    }
    if !app.fold.filter.is_empty() {
        return Line::from(format!(" filter: {} (esc clears)", app.fold.filter).fg(theme.muted));
    }
    Line::default()
}

/// The most used keys for the focused pane, as many as fit in `width`.
fn footer_line(theme: &Theme, focus: Focus, events: bool, width: u16) -> Line<'static> {
    let other = if events { "logs" } else { "events" };
    let keys: &[(&str, &str)] = match focus {
        Focus::Tree => &[
            ("?", "help"),
            ("q", "quit"),
            ("s", "start"),
            ("x", "stop"),
            ("r", "restart"),
            ("p", "pin"),
            ("w", "share"),
            ("/", "filter"),
            ("a", "show"),
            ("d", "disable"),
            ("tab", "logs"),
        ],
        Focus::Logs => &[
            ("?", "help"),
            ("q", "quit"),
            ("↑↓", "scroll"),
            ("g", "top"),
            ("G", "follow"),
            ("/", "search"),
            ("n", "next match"),
            ("e", other),
            ("tab", "tree"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, what) in keys {
        let entry = key.chars().count() + 1 + what.chars().count();
        if used + entry > usize::from(width) {
            break;
        }
        used += entry + 2;
        spans.push(Span::raw(*key).fg(theme.accent).bold());
        spans.push(Span::raw(format!(" {what}  ")));
    }
    Line::from(spans)
}

fn popup(theme: &Theme, frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
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
            pane(theme, Line::from(format!(" {title} ")).bold(), true)
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}

fn render_help(theme: &Theme, frame: &mut Frame, area: Rect) {
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
                Span::raw(format!("{key:width$}  ")).fg(theme.accent),
                Span::raw(what.to_string()),
            ]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(
        "pin* and off* last until the daemon restarts".fg(theme.muted),
    ));
    popup(theme, frame, area, "keys", lines);
}

/// The jump prompt near the top, with the best matches below what was typed.
fn render_jump(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let all = jump::candidates(&app.roots);
    let Some(prompt) = &mut app.jump else {
        return;
    };
    let found = jump::matches(&all, &prompt.query);
    prompt.cursor = prompt.cursor.min(found.len().saturating_sub(1));
    let shown = 10;
    let first = prompt.cursor.saturating_sub(shown - 1);
    let mut lines = vec![Line::from(vec![
        Span::raw("› ").fg(theme.accent),
        Span::raw(format!("{}▏", prompt.query)),
    ])];
    if found.is_empty() {
        lines.push(Line::from("Nothing matches.".fg(theme.muted)));
    }
    for (i, candidate) in found.iter().enumerate().skip(first).take(shown) {
        let line = Line::from(format!("  {}", candidate.name));
        lines.push(if i == prompt.cursor {
            line.reversed()
        } else {
            line
        });
    }
    let width = area.width.saturating_sub(4).min(64);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height / 6,
        width,
        height: (lines.len() as u16 + 2).min(area.height),
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            pane(theme, Line::from(" jump to ").bold(), true)
                .padding(ratatui::widgets::Padding::horizontal(1))
                .title_bottom(Line::from(" ↑↓ pick · enter goes · esc closes ").fg(theme.muted)),
        ),
        rect,
    );
}

/// The selected process's env, one variable per line with where it came from, in a
/// popup that fills most of the screen and scrolls.
fn render_env(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(view) = &mut app.env else {
        return;
    };
    let rect = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    let title = match &view.env {
        Some(Ok(env)) => &env.instance,
        _ => &view.title,
    };
    let block = pane(theme, Line::from(format!(" env · {title} ")).bold(), true)
        .padding(ratatui::widgets::Padding::horizontal(1))
        .title_bottom(Line::from(" ↑↓ scroll · esc closes ").fg(theme.muted));
    let inner = block.inner(rect);
    let width = usize::from(inner.width).max(1);
    let lines: Vec<Line> = match &view.env {
        None => vec![Line::from("Rendering…".fg(theme.muted))],
        Some(Err(message)) => vec![Line::from(message.clone()).fg(theme.error)],
        Some(Ok(env)) => {
            let key_width = env.vars.iter().map(|v| v.key.len()).max().unwrap_or(0);
            env.vars
                .iter()
                .flat_map(|var| {
                    wrap(
                        vec![
                            (format!("{:key_width$}  ", var.key), Style::new().bold()),
                            (var.value.clone().unwrap_or_default(), Style::new()),
                            (format!("  {}", var.source), Style::new().fg(theme.muted)),
                        ],
                        width,
                        (key_width + 2).min(width / 2),
                    )
                })
                .collect()
        }
    };
    view.height = usize::from(inner.height);
    view.scroll = view.scroll.min(lines.len().saturating_sub(view.height));
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(view.scroll).unwrap_or(u16::MAX), 0))
            .block(block),
        rect,
    );
}

/// Breaks styled text into rows of at most `width` characters, indenting the rows
/// after the first by `indent`.
fn wrap(parts: Vec<(String, Style)>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (text, style) in parts {
        let mut chunk = String::new();
        for c in text.chars() {
            if used == width {
                if !chunk.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Line::from(std::mem::take(&mut row)));
                row.push(Span::raw(" ".repeat(indent)));
                used = indent;
            }
            chunk.push(c);
            used += 1;
        }
        if !chunk.is_empty() {
            row.push(Span::styled(chunk, style));
        }
    }
    rows.push(Line::from(row));
    rows
}

fn render_problems(theme: &Theme, frame: &mut Frame, area: Rect, app: &App) {
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
    popup(theme, frame, area, "problems", lines);
}

fn render_confirm(theme: &Theme, frame: &mut Frame, area: Rect, app: &App, confirm: &Confirm) {
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
        theme,
        frame,
        area,
        title,
        vec![
            Line::from(question),
            Line::default(),
            Line::from(vec![
                Span::raw("y").fg(theme.accent).bold(),
                Span::raw(verb),
                Span::raw("any other key").fg(theme.accent).bold(),
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
    use crate::tui::fixtures::{kube_sample, process, sample, shared_sample};

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

    #[test]
    fn a_bordered_pane_is_as_tall_as_its_wrapped_lines() {
        let paragraph = Paragraph::new("x".repeat(9))
            .wrap(Wrap { trim: false })
            .block(pane(&Theme::terminal(), Line::from("t"), false));
        assert_eq!(bordered_height(&paragraph, 11), 3);
        assert_eq!(bordered_height(&paragraph, 10), 4);
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
            message: "stack \"billing\" names an app".into(),
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
                    marker: false,
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
        press(&mut app, now, KeyCode::Right);
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
│  ▸ ● feature-x         1/2                 0%    0B││web    GET /up 200 in 2ms                                       │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
│                                                    ││                                                                │
╰────────────────────────────────────────────────────╯╰────────────────────────────────────────────────────────────────╯

 ? help  q quit  s start  x stop  r restart  p pin  w share  / filter  a show  d disable  tab logs"
        );
    }

    #[test]
    fn muted_text_on_the_selected_row_stays_readable() {
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Disabled;
        billing.processes[1].policy = Policy::Disabled;
        let now = Instant::now();
        let mut app = App::new(now);
        app.start_at = Some(NodeId::Instance {
            project: "nova".into(),
            stack: "main".into(),
            app: "nova-billing".into(),
        });
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        press(&mut app, now, KeyCode::Right);
        let worker = app
            .rows
            .iter()
            .position(|row| tree::node(&app.roots, &row.path).label == "worker")
            .unwrap();
        let unselected = draw_cells(&mut app, "worker");
        assert_eq!(unselected.fg, app.theme.muted, "a disabled label is muted");
        while app.cursor < worker {
            press(&mut app, now, KeyCode::Down);
        }
        let theme = app.theme.clone();
        let focused = draw_cells(&mut app, "worker");
        assert_eq!(focused.bg, theme.selection_bg, "the row is selected");
        assert_ne!(focused.fg, theme.selection_bg);
        assert_eq!(focused.fg, theme.selection_fg);
        press(&mut app, now, KeyCode::Tab);
        let unfocused = draw_cells(&mut app, "worker");
        assert!(unfocused.modifier.contains(Modifier::REVERSED));
        assert_eq!(unfocused.fg, theme.selection_fg);
    }

    /// The first cell of `text` in the tree.
    fn draw_cells(app: &mut App, text: &str) -> ratatui::buffer::Cell {
        let (width, height) = (120, 18);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let tree = app.tree_area;
        for y in tree.top()..tree.bottom() {
            let line: String = (tree.left()..tree.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if let Some(at) = line.find(text) {
                let x = tree.left() + line[..at].chars().count() as u16;
                return buffer[(x, y)].clone();
            }
        }
        panic!("{text} isn't drawn");
    }

    #[test]
    fn renders_a_marker_as_a_rule_across_the_logs() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        let generation = app.logs.generation;
        log(&mut app, generation, "web", "before");
        let at_ms = 1_760_000_000_000;
        app.update(
            Event::Log {
                generation,
                line: LogLine {
                    project: "nova".into(),
                    app: "nova-billing".into(),
                    stack: "main".into(),
                    process: String::new(),
                    at_ms,
                    text: String::new(),
                    marker: true,
                },
            },
            now,
        );
        log(&mut app, generation, "web", "after");
        let screen = draw(&mut app, 120, 18);
        let rule = crate::logs::marker_rule(at_ms, 64);
        let rows: Vec<&str> = screen.lines().collect();
        let at = rows.iter().position(|r| r.contains(&rule)).expect(&screen);
        assert!(rows[at - 1].contains("web before"), "{screen}");
        assert!(rows[at + 1].contains("web after"), "{screen}");
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
        press(&mut app, now, KeyCode::Right);
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
        for code in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::End,
            KeyCode::Right,
            KeyCode::Home,
        ] {
            press(&mut app, now, code);
        }
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
    fn renders_events_and_the_latest_one_in_the_details() {
        let now = Instant::now();
        let mut app = App::new(now);
        let mut snapshot = sample();
        let event = |seq, kind, message: &str, ago_ms| carte_api::Event {
            seq,
            at_ms: crate::clock::now_ms() - ago_ms,
            kind,
            message: message.into(),
            subjects: vec![],
            boot_ms: None,
            exit_code: None,
        };
        let stacks = &mut snapshot.projects[0].stacks;
        stacks[0].instances[0].last_event = Some(event(
            7,
            carte_api::EventKind::Stopped,
            "idle for 15m",
            185_000,
        ));
        stacks[1].instances[0].last_event =
            Some(event(3, carte_api::EventKind::Ready, "in 2.1s", 600_000));
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let commands = press(&mut app, now, KeyCode::Char('e'));
        let Some(Command::FollowEvents { generation, .. }) = commands.first() else {
            panic!("{commands:?}");
        };
        app.update(
            Event::Log {
                generation: *generation,
                line: LogLine {
                    project: String::new(),
                    app: String::new(),
                    stack: String::new(),
                    process: String::new(),
                    at_ms: 0,
                    text: "14:32:05  nova-billing  stopped: idle for 15m".into(),
                    marker: false,
                },
            },
            now,
        );
        let screen = draw(&mut app, 80, 24);
        assert!(
            screen.contains("stopped 3m ago: idle for 15m"),
            "the project shows its newest event:\n{screen}"
        );
        assert!(screen.contains("╭ events following"), "{screen}");
        assert!(
            screen.contains("│14:32:05  nova-billing  stopped: idle for 15m"),
            "{screen}"
        );
    }

    #[test]
    fn renders_how_far_a_start_has_come() {
        let now = Instant::now();
        let mut app = App::new(now);
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.state = InstanceState::Starting;
        billing.processes[0].state = ProcessState::Starting;
        billing.boot = Some(carte_api::BootProgress {
            elapsed_ms: 12_400,
            waiting_for: "web to answer /up on port 4000".into(),
        });
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        let later = now + std::time::Duration::from_secs(3);
        app.update(Event::Tick, later);
        let screen = draw(&mut app, 160, 24);
        assert!(
            screen.contains(
                "starting for 15s · waiting for web to answer /up on port 4000 · running: worker"
            ),
            "{screen}"
        );
    }

    #[test]
    fn renders_a_crash_and_its_problem() {
        let mut app = busy_app();
        let now = app.now;
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Down] {
            press(&mut app, now, code);
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
│  ▸ ● feature-x                                   1/2                 0%    0B│
│                                                                              │
│                                                                              │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ nova-frontend@main ──────────────────────────────────────────────────────────╮
│✖ crashed · https://nova-frontend.test                                        │
│checkout main · /work/nova-frontend                                           │
│web exited with code 1 before it was ready                                    │
│! stack \"billing\" names an app                                                │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ logs following ──────────────────────────────────────────────────────────────╮
│No output yet.                                                                │
│                                                                              │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯

 ? help  q quit  s start  x stop  r restart  p pin  w share  / filter  a show"
        );
    }

    #[test]
    fn renders_a_process_carte_gave_up_on() {
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Exited {
            code: None,
            signal: Some(9),
        };
        billing.processes[1].gave_up = true;
        billing.processes[1].restarts = 4;
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Right] {
            press(&mut app, now, code);
        }
        for _ in 0..2 {
            press(&mut app, now, KeyCode::Down);
        }
        let screen = draw(&mut app, 120, 12);
        assert!(screen.contains("● billing gave up"), "{screen}");
        assert!(screen.contains("✖ worker        gave up"), "{screen}");
        assert!(screen.contains("✖ crashed · 4 restarts"), "{screen}");
        assert!(screen.contains("killed by signal 9"), "{screen}");
        assert!(
            screen.contains("it crashed too often, so carte stopped restarting it"),
            "{screen}"
        );
    }

    #[test]
    fn renders_the_wait_before_restarting_a_crashed_process() {
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Exited {
            code: Some(1),
            signal: None,
        };
        billing.processes[1].restart_in_secs = Some(8);
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Right] {
            press(&mut app, now, code);
        }
        for _ in 0..2 {
            press(&mut app, now, KeyCode::Down);
        }
        let later = now + Duration::from_secs(3);
        app.update(Event::Tick, later);
        let screen = draw(&mut app, 120, 12);
        assert!(
            screen.contains("worker        restarting in 5s"),
            "{screen}"
        );
        assert!(screen.contains("exited with code 1"), "{screen}");
    }

    #[test]
    fn renders_an_instance_whose_web_process_restarts() {
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.state = InstanceState::Restarting;
        billing.processes[0].state = ProcessState::Exited {
            code: Some(1),
            signal: None,
        };
        billing.processes[0].restart_in_secs = Some(4);
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
            press(&mut app, now, code);
        }
        let screen = draw(&mut app, 120, 12);
        assert!(screen.contains("◐ billing"), "{screen}");
        assert!(screen.contains("◐ restarting"), "{screen}");
        assert!(screen.contains("restarting in 4s"), "{screen}");

        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.state = InstanceState::Restarting;
        billing.processes[0].state = ProcessState::Starting;
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 120, 12);
        assert!(screen.contains("restarting"), "{screen}");
        assert!(!screen.contains("restarting in"), "{screen}");
    }

    #[test]
    fn renders_a_process_paused_at_a_breakpoint() {
        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Paused;
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        for code in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Down,
        ] {
            press(&mut app, now, code);
        }
        let screen = draw(&mut app, 120, 14);
        assert!(
            screen.contains("● billing"),
            "a paused worker leaves it ready: {screen}"
        );
        assert!(screen.contains("◆ worker        paused"), "{screen}");
        assert!(screen.contains("◆ paused at a breakpoint"), "{screen}");

        let mut snapshot = sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.state = InstanceState::Paused;
        billing.processes[0].state = ProcessState::Paused;
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 120, 14);
        assert!(screen.contains("◆ billing"), "{screen}");
    }

    #[test]
    fn typing_shows_the_prompt_and_the_way_out() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(sample())), now);
        for code in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Char('t'),
        ] {
            press(&mut app, now, code);
        }
        let screen = draw(&mut app, 120, 12);
        assert!(
            screen.contains(" ctrl-q d stops attaching to nova-billing@main…"),
            "{screen}"
        );
        app.update(
            Event::Attached {
                session: 1,
                result: Ok(carte_api::attach::Attached {
                    instance: "nova-billing@main".into(),
                    process: "web".into(),
                }),
            },
            now,
        );
        app.update(
            Event::TypingOutput {
                session: 1,
                data: b"\x1b[1m(rdbg)\x1b[0m n".to_vec(),
            },
            now,
        );
        let screen = draw(&mut app, 120, 12);
        assert!(
            screen.contains(" ctrl-q d stops nova-billing@main/web › (rdbg) n▏"),
            "{screen}"
        );
        app.update(
            Event::TypingOutput {
                session: 1,
                data: vec![b'x'; 300],
            },
            now,
        );
        let screen = draw(&mut app, 120, 12);
        assert!(screen.contains("› xxxx"), "{screen}");
        assert!(
            screen.contains("xxx▏"),
            "the end of a long line shows: {screen}"
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
 ? help  q quit  s start  x stop  r restart  p pin  w share  / filter  a show"
        );
    }

    #[test]
    fn renders_an_env_that_wraps_and_scrolls() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Char('v'));
        let var = |key: &str, value: &str, source: &str| carte_api::EnvVar {
            key: key.into(),
            value: Some(value.into()),
            masked: false,
            source: source.into(),
            overrides: Vec::new(),
        };
        app.update(
            Event::Env(Ok(carte_api::EnvResponse {
                instance: "nova-billing@main/web".into(),
                vars: vec![
                    var("CARTE_APP", "nova-billing", "carte"),
                    var(
                        "PATH",
                        "/Users/alex/.local/share/mise/installs/ruby/3.4/bin:/usr/local/bin:/usr/bin:/bin",
                        "mise",
                    ),
                    var("PORT", "<assigned at start>", "carte"),
                ],
            })),
            now,
        );
        let screen = draw(&mut app, 60, 14);
        let popup: Vec<&str> = screen.lines().skip(1).take(10).collect();
        assert_eq!(
            popup,
            [
                "│ ╭ env · nova-billing@main/web ─────────────────────────╮ │",
                "│▾│ CARTE_APP  nova-billing  carte                       │M│",
                "│ │ PATH       /Users/alex/.local/share/mise/installs/ru │M│",
                "│ │            by/3.4/bin:/usr/local/bin:/usr/bin:/bin   │M│",
                "╰─│            mise                                      │─╯",
                "╭ │ PORT       <assigned at start>  carte                │─╮",
                "╰─│                                                      │─╯",
                "╭ │                                                      │─╮",
                "│N│                                                      │ │",
                "│ ╰ ↑↓ scroll · esc closes ──────────────────────────────╯ │",
            ],
            "{screen}"
        );

        press(&mut app, now, KeyCode::Char('G'));
        let screen = draw(&mut app, 60, 10);
        assert!(screen.contains("PORT"), "{screen}");
        assert!(
            !screen.contains("CARTE_APP"),
            "scrolled to the end\n{screen}"
        );
    }

    #[test]
    fn renders_the_jump_prompt() {
        let mut app = busy_app();
        let now = app.now;
        app.update(
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            now,
        );
        for c in "bill".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        press(&mut app, now, KeyCode::Down);
        let screen = draw(&mut app, 60, 20);
        let popup: Vec<&str> = screen.lines().skip(3).take(7).collect();
        assert_eq!(
            popup,
            [
                "│ ╭ jump to ─────────────────────────────────────────────╮M│",
                "│ │ › bill▏                                              │B│",
                "│ │   nova-billing@main                                  │ │",
                "│ │   nova-billing@main/web                              │ │",
                "│ │   nova-billing@feature-x                             │ │",
                "╰─│   nova-billing@main/worker                           │─╯",
                "╭ ╰ ↑↓ pick · enter goes · esc closes ───────────────────╯─╮",
            ],
            "{screen}"
        );
    }

    #[test]
    fn highlights_every_match_and_counts_them() {
        let mut app = busy_app();
        let now = app.now;
        let generation = app.logs.generation;
        for text in ["GET /up", "Booting", "GET /a then get /b"] {
            log(&mut app, generation, "web", text);
        }
        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Char('/'));
        for c in "get".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        press(&mut app, now, KeyCode::Enter);
        press(&mut app, now, KeyCode::Char('n'));
        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains("╭ logs /get 1/2 ↑"), "{screen}");
        assert!(screen.contains("Booting"), "every line stays\n{screen}");

        let spans = highlight(&Theme::terminal(), "GET /a then get /b", "get", true);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["", "GET", " /a then ", "get", " /b"]);
        assert_eq!(spans[1].style, Style::new().black().on_yellow());
        let spans = highlight(&Theme::terminal(), "GET /up", "get", false);
        assert_eq!(spans[1].style, Style::new().yellow().bold());
    }

    /// The kube sample with its project opened and an empty listing back.
    fn kube_app(now: Instant) -> App {
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(kube_sample())), now);
        let commands = press(&mut app, now, KeyCode::Right);
        assert_eq!(commands, vec![Command::FetchServices("prod".into())]);
        app.update(
            Event::Services {
                project: "prod".into(),
                result: Ok(Vec::new()),
            },
            now,
        );
        app
    }

    #[test]
    fn a_kube_project_starts_closed_and_lists_its_services_when_opened() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(kube_sample())), now);
        let screen = draw(&mut app, 110, 12);
        assert!(screen.contains("▸ ✖ prod"), "{screen}");
        assert!(!screen.contains("grafana"), "{screen}");

        assert_eq!(
            press(&mut app, now, KeyCode::Right),
            vec![Command::FetchServices("prod".into())]
        );
        let screen = draw(&mut app, 110, 12);
        assert!(screen.contains("⎈ listing…"), "{screen}");

        let entry = |namespace: &str, name: &str| carte_api::KubeServiceEntry {
            namespace: namespace.into(),
            name: name.into(),
            url: format!("https://{name}.{namespace}.prod.test"),
        };
        app.update(
            Event::Services {
                project: "prod".into(),
                result: Ok(vec![
                    entry("billing", "api"),
                    entry("billing", "worker"),
                    entry("kube-system", "kube-dns"),
                    entry("monitoring", "alertmanager"),
                ]),
            },
            now,
        );
        let screen = draw(&mut app, 110, 14);
        assert!(screen.contains("▸ ○ billing       2 services"), "{screen}");
        assert!(
            screen.contains("alertmanage"),
            "namespaces in use are open: {screen}"
        );
        let rows: Vec<&str> = screen.lines().collect();
        let system = rows.iter().position(|l| l.contains("kube-system")).unwrap();
        let monitoring = rows.iter().position(|l| l.contains("monitoring")).unwrap();
        assert!(system < monitoring, "namespaces by name: {screen}");

        press(&mut app, now, KeyCode::Char('/'));
        for c in "work".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        let screen = draw(&mut app, 110, 14);
        assert!(
            screen.contains("worker"),
            "the filter finds closed namespaces: {screen}"
        );
        assert!(!screen.contains("alertmanage"), "{screen}");

        press(&mut app, now, KeyCode::Esc);
        press(&mut app, now, KeyCode::Left);
        press(&mut app, now, KeyCode::Home);
        assert_eq!(press(&mut app, now, KeyCode::Left), Vec::<Command>::new());
        assert_eq!(
            press(&mut app, now, KeyCode::Right),
            vec![Command::FetchServices("prod".into())],
            "opening it again lists them again"
        );
        app.update(
            Event::Services {
                project: "prod".into(),
                result: Err("The cluster refused".into()),
            },
            now,
        );
        let screen = draw(&mut app, 110, 14);
        assert!(screen.contains("⎈ The cluster re"), "{screen}");
        assert!(
            screen.contains("listing Services failed: The cluster refused"),
            "{screen}"
        );
    }

    #[test]
    fn long_log_lines_wrap_under_their_text() {
        let mut app = busy_app();
        let now = Instant::now();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        let commands = press(&mut app, now, KeyCode::Down);
        let generation = commands
            .iter()
            .find_map(|c| match c {
                Command::FollowLogs { generation, .. } => Some(*generation),
                _ => None,
            })
            .unwrap();
        log(
            &mut app,
            generation,
            "web",
            "logger=http t=2026-10-20T10:30:45Z level=info msg=\"Request Completed\" path=/api/health status=200",
        );
        let screen = draw(&mut app, 110, 24);
        assert!(screen.contains("status=200"), "{screen}");
        assert!(screen.contains("logger=http"), "{screen}");
    }

    #[test]
    fn renders_kube_projects() {
        let now = Instant::now();
        let mut app = kube_app(now);
        let screen = draw(&mut app, 110, 24);
        assert!(screen.contains("prod"), "{screen}");
        assert!(screen.contains("⎈ prod-context"), "{screen}");
        assert!(screen.contains("failed"), "{screen}");
        assert!(screen.contains("held by 2"), "{screen}");
        assert!(screen.contains("failed: vlogs"), "{screen}");
        assert!(!screen.contains("0 B"), "no memory for Services: {screen}");

        let mut snapshot = kube_sample();
        snapshot.projects[0].kube.as_mut().unwrap().error =
            Some("login failed: Run `aws sso login`, then reload.".into());
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 110, 24);
        assert!(screen.contains("⎈ login failed"), "{screen}");
        assert!(
            screen.contains("login failed: Run `aws sso login`, then reload."),
            "{screen}"
        );
    }

    #[test]
    fn a_kube_project_with_one_namespace_keeps_its_own_row() {
        let now = Instant::now();
        let mut app = App::new(now);
        let mut snapshot = kube_sample();
        snapshot.projects[0].stacks.remove(0);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 110, 12);
        assert!(screen.contains("⎈ prod-context"), "{screen}");
    }

    #[test]
    fn renders_a_service_s_pod() {
        let now = Instant::now();
        let mut app = kube_app(now);
        let grafana = (0..10)
            .find(|_| {
                press(&mut app, now, KeyCode::Down);
                app.selected_node()
                    .is_some_and(|n| n.label == "grafana" || n.search.contains("grafana"))
            })
            .is_some();
        assert!(grafana);
        let screen = draw(&mut app, 110, 30);
        assert!(
            screen.contains("pod grafana-7d9f-x2k :3000 · namespace monitoring"),
            "{screen}"
        );
        assert!(!screen.contains("checkout"), "{screen}");

        let mut snapshot = kube_sample();
        snapshot.projects[0].stacks[1].instances[0].last_error =
            Some("error forwarding port 3000: connection refused".into());
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 110, 30);
        assert!(
            screen.contains("error forwarding port 3000: connection refused"),
            "a ready Service shows its last error: {screen}"
        );
    }

    #[test]
    fn renders_help() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('?'));
        let screen = draw(&mut app, 100, 50);
        for (_, keys) in HELP {
            for (key, what) in keys.iter() {
                assert!(
                    screen.lines().any(|l| l.contains(key) && l.contains(what)),
                    "{key} {what} in\n{screen}"
                );
            }
        }
        assert!(screen.contains("pin* and off* last until the daemon restarts"));
    }
}
