//! Draws the app state.

use std::time::Duration;

use carte_api::{
    BootProgress, Event, InstanceStatus, Policy, Problem, ProcessState, ProcessStatus,
    ProxiedRequest, Served, TunnelState,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};

use super::app::{App, Connection};
use super::browse::Browse;
use super::input::{Confirm, Focus, HELP, Mode};
use super::jump;
use super::logs::Logs;
use super::requests::Requests;
use super::tabs::{EnvSubject, Tab};
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

    app.problems_area = Rect::default();
    app.logs_area = Rect::default();
    app.tab_spans.clear();
    if app.snapshot.is_some() && app.zoom.is_some() {
        app.tree_area = Rect::default();
        render_selection(theme, frame, main, app);
    } else if app.snapshot.is_some() {
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
    frame.render_widget(footer_line(theme, app, footer.width), footer);

    match &app.mode {
        Mode::Help => render_help(theme, frame, main),
        Mode::Problems => render_problems(theme, frame, main, app),
        Mode::Confirm(confirm) => render_confirm(theme, frame, main, app, confirm),
        Mode::Jump => render_jump(theme, frame, main, app),
        Mode::Themes => render_themes(theme, frame, main, app),
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
    let width = |title: &[Span]| title.iter().map(Span::width).sum::<usize>();
    let mut problems_from = None;
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
            TunnelState::Failed => ("tunnel down", theme.error),
            TunnelState::Idle => ("", theme.muted),
            TunnelState::Unknown => ("tunnel unknown", theme.muted),
        };
        let what = if tunnel.state == TunnelState::Idle {
            format!("· {shared} shared ")
        } else {
            format!("· {shared} shared, {word} ")
        };
        if tunnel.state == TunnelState::Failed {
            problems_from = Some(width(&title));
        }
        title.push(Span::raw(what).fg(color));
    }
    if !snapshot.problems.is_empty() {
        let count = snapshot.problems.len();
        let noun = if count == 1 { "problem" } else { "problems" };
        problems_from.get_or_insert(width(&title));
        title.push(Span::raw(format!("· {count} {noun} ")).fg(theme.error));
    }
    if let Some(from) = problems_from {
        let column = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        let title_area = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            height: 1,
            ..area
        };
        let x = title_area.x.saturating_add(column(from));
        app.problems_area =
            Rect::new(x, area.y, column(width(&title) - from), 1).intersection(title_area);
    }
    let block = pane(theme, Line::from(title), app.focus == Focus::Tree);
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    let typing = app.mode == Mode::Filter && app.focus == Focus::Tree;
    if typing || !app.fold.filter.is_empty() {
        let input = Input {
            label: "filter",
            term: &app.fold.filter,
            count: None,
            keys: "enter keeps · esc clears",
            typing,
        };
        inner = render_input(theme, frame, inner, input);
    }
    app.tree_height = inner.height.saturating_sub(1) as usize;

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
    .row_highlight_style(theme.selection());
    let mut state = TableState::default()
        .with_offset(app.tree_offset)
        .with_selected((!app.rows.is_empty()).then_some(app.cursor));
    frame.render_stateful_widget(table, inner, &mut state);
    app.tree_area = area;
    app.tree_offset = state.offset();
    if app.rows.is_empty() {
        let message = if app.fold.filter.is_empty() && app.fold.show == Show::All {
            "No projects. Add one with `carte project add`."
        } else {
            "Nothing matches."
        };
        let row = Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        };
        frame.render_widget(Paragraph::new(message.fg(theme.muted)).centered(), row);
    }
}

/// A pane's filter or search.
struct Input<'a> {
    label: &'static str,
    term: &'a str,
    /// The current match and how many there are, like `3/12`.
    count: Option<String>,
    /// What the keys do while typing.
    keys: &'static str,
    typing: bool,
}

/// Draws the input at the bottom of a pane's `inner` area and returns the area above
/// it: under a rule with the label, the count and, while typing, the keys, or on one
/// row with the label when the pane is short. The term shows with a cursor while
/// typing, its end kept in view.
fn render_input(theme: &Theme, frame: &mut Frame, inner: Rect, input: Input) -> Rect {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let rows = if inner.height >= 10 { 2 } else { 1 };
    let [above, below] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(rows)]).areas(inner);
    let color = if input.typing {
        theme.border_focus
    } else {
        theme.border
    };
    let label = format!(" {} ", input.label);
    let count = input.count.map(|c| format!(" {c} ")).unwrap_or_default();
    let mut room = usize::from(below.width);
    let text_area = if rows == 2 {
        let mut title = vec![Span::raw(label).fg(color)];
        if input.typing {
            title.push(Span::raw(format!("· {} ", input.keys)).fg(theme.muted));
        }
        let block = Block::new()
            .borders(Borders::TOP)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(Line::from(title))
            .title(Line::from(count).fg(color).right_aligned());
        let text_area = block.inner(below);
        frame.render_widget(block, below);
        text_area
    } else {
        room = room.saturating_sub(label.width() + count.width());
        let [label_area, text_area, count_area] = Layout::horizontal([
            Constraint::Length(label.width() as u16),
            Constraint::Fill(1),
            Constraint::Length(count.width() as u16),
        ])
        .areas(below);
        frame.render_widget(Paragraph::new(label.fg(color).reversed()), label_area);
        frame.render_widget(Paragraph::new(count.fg(color)), count_area);
        text_area
    };
    let mut text: Vec<char> = input.term.chars().collect();
    if input.typing {
        text.push('▏');
    }
    let mut used = 0;
    let start = text
        .iter()
        .rposition(|c| {
            used += c.width().unwrap_or(0);
            used > room.saturating_sub(1)
        })
        .map_or(0, |i| i + 1);
    let shown: String = text[start..].iter().collect();
    frame.render_widget(Paragraph::new(format!(" {shown}")), text_area);
    above
}

/// On the selected row, `muted` text is drawn in `selection_fg`.
fn tree_row(
    theme: &Theme,
    app: &App,
    node: &Node,
    row: &tree::Row,
    selected: bool,
) -> Row<'static> {
    let row_theme;
    let theme = if selected {
        row_theme = Theme {
            muted: theme.selection_fg,
            ..theme.clone()
        };
        &row_theme
    } else {
        theme
    };
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
    Row::new([
        Cell::from(Line::from(spans)),
        Cell::from(detail),
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
        render_tabs(theme, frame, area, app);
        return;
    };
    let title = format!(" {} ", target::label(node.id()));
    let details = details(theme, app, node);
    let lines = if app.zoom.is_some() {
        details.joined(theme, usize::from(area.width.saturating_sub(2)))
    } else {
        details.0.into_iter().map(|(line, _)| line).collect()
    };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(pane(
        theme,
        Line::from(title).bold(),
        false,
    ));
    let height = bordered_height(&paragraph, area.width).min(area.height.saturating_sub(4));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(area);
    frame.render_widget(paragraph, top);
    render_tabs(theme, frame, bottom, app);
}

/// The details' lines, each marked whether it's a short fact that can share a row.
#[derive(Default)]
struct Details(Vec<(Line<'static>, bool)>);

impl Details {
    fn push(&mut self, line: Line<'static>) {
        self.0.push((line, true));
    }

    /// A line that keeps a row of its own, like an error.
    fn alone(&mut self, line: Line<'static>) {
        self.0.push((line, false));
    }

    fn extend(&mut self, other: Details) {
        self.0.extend(other.0);
    }

    /// Joins facts in a row while they fit in `width` columns, separated by ` · `.
    fn joined(self, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut joinable = false;
        for (line, fact) in self.0 {
            let fits = rows
                .last()
                .is_some_and(|row| row.width() + 3 + line.width() <= width);
            let spans: Vec<Span<'static>> = line
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), line.style.patch(span.style)))
                .collect();
            if joinable && fact && fits {
                let row = rows.last_mut().expect("a row");
                row.spans.push(Span::raw(" · ").fg(theme.muted));
                row.spans.extend(spans);
            } else {
                joinable = fact;
                rows.push(Line::from(spans));
            }
        }
        rows
    }
}

/// The rows a wrapped paragraph in a bordered pane takes at `width`, borders
/// included.
fn bordered_height(paragraph: &Paragraph, width: u16) -> u16 {
    u16::try_from(paragraph.line_count(width.saturating_sub(2))).unwrap_or(u16::MAX)
}

fn details(theme: &Theme, app: &App, node: &Node) -> Details {
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
    let mut lines = Details::default();
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
                    lines.alone(Line::from(error.clone()).fg(theme.error));
                }
                if let Some(Browse::Failed(error)) = app.browse.get(project) {
                    lines.alone(
                        Line::from(format!("listing Services failed: {error}")).fg(theme.error),
                    );
                }
            }
            let crashed: Vec<&str> = crashed_below(node);
            if !crashed.is_empty() {
                let word = if status.kube { "failed" } else { "crashed" };
                lines.alone(Line::from(
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
        lines.alone(Line::from(format!("! {}", problem.message)).fg(theme.error));
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
) -> Details {
    let mut lines = Details::default();
    lines.push(Line::from(vec![
        state,
        Span::raw(" · ").fg(theme.muted),
        Span::raw(instance.url.clone()).underlined(),
    ]));
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
        lines.alone(Line::from(error.clone()).fg(theme.error));
    }
    lines
}

/// Where a shared instance is public, how it's guarded, and its latest visitors.
fn share_details(theme: &Theme, app: &App, project: &str, instance: &InstanceStatus) -> Details {
    let mut lines = Details::default();
    let Some(public) = &instance.public_url else {
        return lines;
    };
    lines.push(Line::from(vec![
        Span::raw("shared at ").fg(theme.shared),
        Span::raw(public.clone()).underlined(),
    ]));
    let snapshot = app.snapshot.as_ref().expect("a snapshot to draw");
    if let Some(tunnel) = &snapshot.tunnel
        && tunnel.state == TunnelState::Failed
    {
        let message = tunnel.message.as_deref().unwrap_or("no reason given");
        lines.alone(Line::from(format!("the tunnel is down: {message}")).fg(theme.error));
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
    lines
}

fn process_details(
    theme: &Theme,
    app: &App,
    instance: &InstanceStatus,
    process: &ProcessStatus,
    state: Span<'static>,
) -> Details {
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
    let mut lines = Details::default();
    lines.push(Line::from(first));
    if let ProcessState::Exited { code, signal } = process.state {
        let how = match (code, signal) {
            (_, Some(signal)) => format!("killed by signal {signal}"),
            (Some(code), None) => format!("exited with code {code}"),
            (None, None) => "exited".into(),
        };
        let line = Line::from(how);
        if code == Some(0) {
            lines.push(line.fg(theme.muted));
        } else {
            lines.alone(line.fg(theme.error));
        }
    }
    if let Some(secs) = process.restart_in_secs {
        lines.push(
            Line::from(format!("restarting in {}", duration(app.idle_in(secs)))).fg(theme.warn),
        );
    }
    if process.gave_up {
        lines.alone(
            Line::from(
                "it crashed too often, so carte stopped restarting it; start it to try again",
            )
            .fg(theme.error),
        );
    }
    if let Some(error) = &instance.last_error
        && !matches!(process.state, ProcessState::Running | ProcessState::Done)
    {
        lines.alone(Line::from(error.clone()).fg(theme.error));
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

/// The pane below the details: the tabs on its top border, and the current one.
fn render_tabs(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Logs;
    let separator = if focused {
        theme.border_focus
    } else {
        theme.border
    };
    let id = app.selected_node().map(|n| n.id().clone());
    let kube = id
        .as_ref()
        .is_some_and(|id| app.kube_project(id.project()).is_some());
    let status = match app.tab {
        Tab::Logs | Tab::Events => logs_status(theme, &app.logs),
        Tab::Requests => requests_status(theme, &app.requests, app.zoom.is_some()),
        Tab::Env => Vec::new(),
    };
    let status = Line::from(status);
    let names_end = area.right().saturating_sub(1 + status.width() as u16);
    let mut title = vec![Span::raw(" ")];
    let mut x = area.x + 2;
    app.tab_spans.clear();
    for (i, tab) in Tab::ALL.into_iter().enumerate() {
        if i > 0 {
            title.push(Span::raw(" │ ").fg(separator));
            x += 3;
        }
        let name = format!("{} {}", tab.number(), tab.name());
        let width = name.chars().count() as u16;
        let style = match (tab == app.tab, tab.applies(id.as_ref(), kube)) {
            (true, true) => Style::new().fg(theme.accent).bold(),
            (true, false) => Style::new().fg(theme.muted).bold(),
            (false, true) => Style::new().fg(theme.fg),
            (false, false) => Style::new().fg(theme.muted),
        };
        title.push(Span::styled(name, style));
        if x + width <= names_end {
            app.tab_spans.push((tab, x, x + width));
        }
        x += width;
    }
    title.push(Span::raw(" "));
    let block = pane(theme, Line::from(title), focused).title(status.right_aligned());
    let inner = block.inner(area);
    app.logs_area = area;
    frame.render_widget(block, area);
    match app.tab {
        Tab::Logs | Tab::Events => render_logs(theme, frame, inner, app),
        Tab::Env => render_env(theme, frame, inner, app),
        Tab::Requests => render_requests(theme, frame, inner, app),
    }
}

/// Following, the selected row's place, or the way back from an open request.
fn requests_status(theme: &Theme, requests: &Requests, zoomed: bool) -> Vec<Span<'static>> {
    if requests.open.is_some() && !zoomed {
        return vec![Span::raw(" esc back to the list ").fg(theme.muted)];
    }
    vec![match requests.selected_index() {
        Some(index) => {
            Span::raw(format!(" {}/{} ", index + 1, requests.rows().len())).fg(theme.warn)
        }
        None => Span::raw(" following ").fg(theme.muted),
    }]
}

/// The requests tab: one row per request, or the open one in full. Zoomed, the list
/// is on the left and the selected request on the right.
fn render_requests(theme: &Theme, frame: &mut Frame, inner: Rect, app: &mut App) {
    app.requests.height = usize::from(inner.height);
    app.requests.detail_area = Rect::default();
    if app.zoom.is_some() {
        let [list, detail] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(inner);
        render_request_list(theme, frame, list, app);
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(theme.border));
        let detail_inner = block.inner(detail);
        frame.render_widget(block, detail);
        app.requests.detail_area = detail;
        let seq = app.requests.shown().map(|r| r.seq);
        render_request(theme, frame, detail_inner, app, seq);
        return;
    }
    if app.requests.open.is_some() {
        let seq = app.requests.open;
        render_request(theme, frame, inner, app, seq);
        return;
    }
    render_request_list(theme, frame, inner, app);
}

/// The request numbered `seq` in full, scrolled.
fn render_request(theme: &Theme, frame: &mut Frame, area: Rect, app: &mut App, seq: Option<u64>) {
    let requests = &mut app.requests;
    let Some(request) = seq.and_then(|seq| requests.request(seq)) else {
        return;
    };
    let lines = request_detail(theme, request, usize::from(area.width).max(1));
    requests.detail_scroll = requests
        .detail_scroll
        .min(lines.len().saturating_sub(usize::from(area.height)));
    let scroll = u16::try_from(requests.detail_scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

fn render_request_list(theme: &Theme, frame: &mut Frame, inner: Rect, app: &mut App) {
    let requests = &app.requests;
    if requests.is_empty() {
        let message = match &requests.ended {
            Some(reason) => format!("No requests: {reason}"),
            None => "No requests yet.".into(),
        };
        frame.render_widget(Paragraph::new(message.fg(theme.muted)), inner);
        return;
    }
    let range = app.requests.window(usize::from(inner.height));
    let requests = &app.requests;
    let selected = requests.selected_index();
    let method_width = requests
        .rows()
        .range(range.clone())
        .map(|r| r.method.chars().count())
        .max()
        .unwrap_or(0)
        .max(7);
    let path_width = usize::from(inner.width).saturating_sub(8 + method_width + 9 + 6 + 1 + 5);
    let rows: Vec<Row> = requests
        .rows()
        .range(range.clone())
        .enumerate()
        .map(|(i, request)| {
            let muted = if Some(range.start + i) == selected {
                theme.selection_fg
            } else {
                theme.muted
            };
            request_row(theme, request, muted, path_width)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(method_width as u16),
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(1),
        ],
    )
    .row_highlight_style(theme.selection());
    let mut state = TableState::default()
        .with_selected(selected.and_then(|index| index.checked_sub(range.start)));
    frame.render_stateful_widget(table, inner, &mut state);
}

/// A request's row, its path cut to `path_width` with an ellipsis.
fn request_row(
    theme: &Theme,
    request: &ProxiedRequest,
    muted: Color,
    path_width: usize,
) -> Row<'static> {
    let status = match request.status {
        Some(status) => Span::raw(status.to_string()).fg(status_color(theme, status)),
        None => Span::raw("cancelled").fg(muted),
    };
    let mark = if request.shared {
        Span::raw("↗").fg(theme.shared)
    } else {
        Span::raw("")
    };
    Row::new([
        Cell::from(Span::raw(crate::clock::local_hms(request.at_ms)).fg(muted)),
        Cell::from(request.method.clone()),
        Cell::from(ellipsis(&request.path, path_width)),
        Cell::from(Line::from(status).right_aligned()),
        Cell::from(Line::from(Span::raw(millis(total_ms(request))).fg(muted)).right_aligned()),
        Cell::from(mark),
    ])
}

fn status_color(theme: &Theme, status: u16) -> Color {
    match status {
        500.. => theme.error,
        400..500 => theme.warn,
        200..300 => theme.ok,
        _ => theme.fg,
    }
}

/// Held and upstream together.
fn total_ms(request: &ProxiedRequest) -> u64 {
    request.held_ms + request.upstream_ms.unwrap_or(0)
}

/// `12ms`, `1.2s`, `2m05s`.
fn millis(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..59_950 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => {
            let secs = (ms + 500) / 1000;
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }
}

/// `text` cut to `width` characters, ending in `…` when cut.
fn ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// An open request: what was asked and answered, how it was routed and how long it
/// took, then the headers in full.
fn request_detail(theme: &Theme, request: &ProxiedRequest, width: usize) -> Vec<Line<'static>> {
    let label = |text: &str| (format!("{text:<10}"), Style::new().fg(theme.muted));
    let field =
        |name: &str, value: String| wrap(vec![label(name), (value, Style::new())], width, 10);
    let mut lines = vec![Line::from(vec![
        Span::raw(format!("{} {}", request.method, request.path)).bold(),
        Span::raw("  "),
        match request.status {
            Some(status) => Span::raw(status.to_string()).fg(status_color(theme, status)),
            None => Span::raw("cancelled").fg(theme.muted),
        },
    ])];
    lines.extend(field("host", request.host.clone()));
    let mut route = Vec::new();
    if let Some(subject) = &request.subject {
        route.push(subject.label());
    }
    if let Some(checkout) = &request.route.checkout {
        route.push(format!("checkout {checkout}"));
    }
    if request.route.via_alias {
        route.push("through an alias".into());
    }
    if !route.is_empty() {
        lines.extend(field("route", route.join(" · ")));
    }
    if let Some(served) = &request.route.served {
        let served = match served {
            Served::Process { port } => format!("the process on port {port}"),
            Served::Forward => "the kube port-forward".into(),
            Served::File { path } => format!("the file {}", path.display()),
            Served::Page { title } => format!("carte's page: {title}"),
            Served::Unknown => "something this carte doesn't know".into(),
        };
        lines.extend(field("served by", served));
    }
    if !request.route.warmed.is_empty() {
        lines.extend(field("warmed", request.route.warmed.join(", ")));
    }
    if request.held_ms > 0 {
        lines.extend(field("held", millis(request.held_ms)));
    }
    if let Some(upstream) = request.upstream_ms {
        lines.extend(field("upstream", millis(upstream)));
    }
    if request.shared {
        let visitor = request.visitor.clone().unwrap_or_else(|| "unknown".into());
        lines.push(Line::from(vec![
            Span::styled(label("shared").0, Style::new().fg(theme.shared)),
            Span::raw(format!("from {visitor}")),
        ]));
    }
    if request.websocket {
        lines.extend(field("upgrade", "WebSocket".into()));
    }
    for (title, headers) in [
        ("request headers", &request.request_headers),
        ("response headers", &request.response_headers),
    ] {
        lines.push(Line::default());
        lines.push(Line::from(title.to_string()).bold());
        if headers.is_empty() {
            lines.push(Line::from("none".fg(theme.muted)));
        }
        for header in headers {
            lines.extend(wrap(
                vec![
                    (format!("{}: ", header.name), Style::new().fg(theme.muted)),
                    (header.value.clone(), Style::new()),
                ],
                width,
                2.min(width / 2),
            ));
        }
    }
    if request.headers_truncated {
        lines.push(Line::default());
        lines.push(Line::from(
            "Long headers were cut short, or some left out.".fg(theme.warn),
        ));
    }
    lines
}

/// How far up the logs are, for the pane's border.
fn logs_status(theme: &Theme, logs: &Logs) -> Vec<Span<'static>> {
    let mut title = vec![];
    title.push(if logs.scroll > 0 {
        Span::raw(format!(" ↑{} ", logs.scroll)).fg(theme.warn)
    } else {
        Span::raw(" following ").fg(theme.muted)
    });
    title
}

fn render_logs(theme: &Theme, frame: &mut Frame, mut inner: Rect, app: &mut App) {
    let events = match app.tab {
        Tab::Events => true,
        Tab::Logs | Tab::Env | Tab::Requests => false,
    };
    let logs = &app.logs;
    let typing = app.mode == Mode::Filter && app.focus == Focus::Logs;
    if typing || !logs.search.is_empty() {
        let count = (!logs.search.is_empty()).then(|| {
            let seqs = logs.match_seqs();
            let at = logs
                .current
                .and_then(|seq| seqs.iter().position(|&s| s == seq))
                .map_or(String::new(), |i| format!("{}/", i + 1));
            format!("{at}{}", seqs.len())
        });
        let input = Input {
            label: if logs.filter { "filter" } else { "search" },
            term: &logs.search,
            count,
            keys: if logs.filter {
                "ctrl-f searches · enter keeps · esc clears"
            } else {
                "ctrl-f filters · enter keeps · esc clears"
            },
            typing,
        };
        inner = render_input(theme, frame, inner, input);
    }
    app.logs.height = inner.height as usize;
    app.logs.width = inner.width as usize;
    let logs = &app.logs;

    let window = logs.window_rows(inner.height as usize);
    if window.lines.is_empty() {
        let stopped = app
            .selected_node()
            .is_some_and(|n| matches!(n.status.state, State::Stopped | State::Disabled));
        let message = match (&logs.ended, logs.is_empty()) {
            (_, false) if logs.filter => "Nothing matches.".into(),
            (Some(reason), _) if events => format!("No events: {reason}"),
            (Some(reason), _) => format!("No logs: {reason}"),
            (None, _) if events => "Nothing has happened here yet.".into(),
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
            line_rows = line_rows
                .into_iter()
                .map(|line| line.patch_style(theme.selection()))
                .collect();
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
    let needle = super::logs::fold(search);
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some((start, end)) = super::logs::find(text, &needle, from) {
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
    Line::default()
}

/// Problems the daemon reports, or a tunnel that is down while something is shared,
/// as the tree's title shows them.
fn has_problems(app: &App) -> bool {
    app.snapshot.as_ref().is_some_and(|s| {
        !s.problems.is_empty()
            || (!s.shares.is_empty()
                && s.tunnel
                    .as_ref()
                    .is_some_and(|t| t.state == TunnelState::Failed))
    })
}

/// The most used keys for the focused pane, as many as fit in `width`, after
/// `! problems` while there are any.
fn footer_line(theme: &Theme, app: &App, width: u16) -> Line<'static> {
    let (focus, tab) = (app.focus, app.tab);
    let problems = has_problems(app);
    let keys: &[(&str, &str)] = match (focus, tab) {
        (Focus::Tree, _) => &[
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
            ("1-5", "tabs"),
            ("z", "zoom"),
            ("tab", tab.name()),
        ],
        (Focus::Logs, Tab::Requests) if app.zoom.is_some() => &[
            ("?", "help"),
            ("q", "quit"),
            ("↑↓", "move"),
            ("G", "follow"),
            ("1-5", "tabs"),
            ("z", "zoom"),
            ("tab", "tree"),
        ],
        (Focus::Logs, Tab::Requests) => &[
            ("?", "help"),
            ("q", "quit"),
            ("↑↓", "move"),
            ("enter", "open"),
            ("esc", "back"),
            ("G", "follow"),
            ("1-5", "tabs"),
            ("z", "zoom"),
            ("tab", "tree"),
        ],
        (Focus::Logs, Tab::Logs | Tab::Events | Tab::Env) => &[
            ("?", "help"),
            ("q", "quit"),
            ("↑↓", "scroll"),
            ("g", "top"),
            ("G", "follow"),
            ("/", "search"),
            ("n", "next match"),
            ("1-5", "tabs"),
            ("z", "zoom"),
            ("tab", "tree"),
        ],
    };
    let problems = problems.then_some(("!", "problems", theme.error, Some(theme.error)));
    let keys = keys
        .iter()
        .map(|&(key, what)| (key, what, theme.accent, None));
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, what, key_color, color) in problems.into_iter().chain(keys) {
        let entry = key.chars().count() + 1 + what.chars().count();
        if used + entry > usize::from(width) {
            break;
        }
        used += entry + 2;
        spans.push(Span::raw(key).fg(key_color).bold());
        let what = Span::raw(format!(" {what}  "));
        spans.push(match color {
            Some(color) => what.fg(color),
            None => what,
        });
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

/// Each theme with a swatch of its colours on its own background.
fn render_themes(theme: &Theme, frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let width = picker
        .themes
        .iter()
        .map(|t| t.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = picker
        .themes
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut spans = vec![Span::raw(format!("{:width$}  ", t.label))];
            let swatch = Style::new().bg(t.bg);
            spans.push(Span::styled(" ", swatch));
            for color in [t.fg, t.accent, t.ok, t.warn, t.error, t.shared] {
                spans.push(Span::styled("●", swatch.fg(color)));
            }
            spans.push(Span::styled(" ", swatch));
            let line = Line::from(spans);
            if i == picker.cursor {
                line.style(theme.selection())
            } else {
                line
            }
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(
        "↑↓ preview · enter keeps · esc goes back".fg(theme.muted),
    ));
    popup(theme, frame, area, "theme", lines);
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
            line.style(theme.selection())
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

/// The env tab: one variable per line with where it came from, scrolled.
fn render_env(theme: &Theme, frame: &mut Frame, inner: Rect, app: &mut App) {
    let view = &mut app.env;
    let width = usize::from(inner.width).max(1);
    let mut lines: Vec<Line> = Vec::new();
    match &view.subject {
        EnvSubject::None => {}
        EnvSubject::Unavailable(message) => {
            lines.push(Line::from(message.clone().fg(theme.muted)));
        }
        EnvSubject::Fetch { note, .. } => {
            if let Some(note) = note {
                lines.extend(
                    wrap_words(note, width)
                        .into_iter()
                        .map(|row| row.fg(theme.muted).into()),
                );
            }
            match &view.env {
                None => lines.push(Line::from("Rendering…".fg(theme.muted))),
                Some(Err(message)) => lines.push(Line::from(message.clone()).fg(theme.error)),
                Some(Ok(env)) => {
                    let key_width = env.vars.iter().map(|v| v.key.len()).max().unwrap_or(0);
                    lines.extend(env.vars.iter().flat_map(|var| {
                        wrap(
                            vec![
                                (format!("{:key_width$}  ", var.key), Style::new().bold()),
                                (var.value.clone().unwrap_or_default(), Style::new()),
                                (format!("  {}", var.source), Style::new().fg(theme.muted)),
                            ],
                            width,
                            (key_width + 2).min(width / 2),
                        )
                    }));
                }
            }
        }
    }
    view.height = usize::from(inner.height);
    view.scroll = view.scroll.min(lines.len().saturating_sub(view.height));
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(view.scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
}

/// Breaks text into rows of at most `width` characters between words, and inside
/// words longer than that.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let fits = row.chars().count() + 1 + word.chars().count() <= width;
        if !row.is_empty() && !fits {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        for c in word.chars() {
            if row.chars().count() == width {
                rows.push(std::mem::take(&mut row));
            }
            row.push(c);
        }
    }
    rows.push(row);
    rows
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
╭ carte · 2 running · 1 problem ─────────────────────╮╭ nova-billing@main ─────────────────────────────────────────────╮
│                                         CPU   MEM  ││● running · https://nova-billing.test                           │
│▾ ✖ nova !              2/4                 3%  412M││checkout main · /work/nova-billing                              │
│  ▾ ✖ main              1/2                 3%  412M││worker pinned until the daemon restarts                         │
│    ▾ ● billing pin*    ready               3%  412M│╰────────────────────────────────────────────────────────────────╯
│        ● web           running             0%    0B│╭ 1 logs │ 2 events │ 3 env │ 5 requests ───────────── following ╮
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

 ! problems  ? help  q quit  s start  x stop  r restart  p pin  w share  / filter  a show  d disable  1-5 tabs  z zoom"
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
        let theme = app.theme.clone();
        for text in ["worker", "disabled"] {
            let unselected = draw_cell(&mut app, text);
            assert_eq!(unselected.fg, theme.muted, "{text} is muted");
        }
        while app.cursor < worker {
            press(&mut app, now, KeyCode::Down);
        }
        for text in ["worker", "disabled"] {
            let focused = draw_cell(&mut app, text);
            assert_eq!(focused.bg, theme.selection_bg, "{text} is selected");
            assert_ne!(focused.fg, theme.selection_bg);
            assert_eq!(focused.fg, theme.selection_fg);
        }
        press(&mut app, now, KeyCode::Tab);
        for text in ["worker", "disabled"] {
            let unfocused = draw_cell(&mut app, text);
            assert_eq!(unfocused.bg, theme.selection_bg, "{text} stays selected");
            assert_eq!(unfocused.fg, theme.selection_fg);
        }
    }

    /// The first cell of `text` in the tree.
    fn draw_cell(app: &mut App, text: &str) -> ratatui::buffer::Cell {
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
    fn the_footer_leads_with_problems_and_a_click_on_their_count_lists_them() {
        let mut app = busy_app();
        let now = app.now;
        let (width, height) = (120, 18);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for focus in [Focus::Tree, Focus::Logs] {
            app.focus = focus;
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let footer: String = (0..12).map(|x| buffer[(x, height - 1)].symbol()).collect();
            assert_eq!(footer, " ! problems ", "{focus:?}");
            assert_eq!(buffer[(1, height - 1)].fg, app.theme.error);
            assert_eq!(buffer[(4, height - 1)].fg, app.theme.error);
        }

        let buffer = terminal.backend().buffer();
        let title: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
        let at = title.find("1 problem").expect(&title);
        let x = title[..at].chars().count() as u16;
        app.update(click(2, 0), now);
        assert_eq!(
            app.mode,
            Mode::Normal,
            "the rest of the title isn't a button"
        );
        app.update(click(x + 3, 0), now);
        assert_eq!(app.mode, Mode::Problems);

        press(&mut app, now, KeyCode::Esc);
        let mut snapshot = app.snapshot.clone().unwrap();
        snapshot.problems.clear();
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, width, height);
        assert!(!screen.contains("problem"), "{screen}");
        app.update(click(x + 3, 0), now);
        assert_eq!(app.mode, Mode::Normal, "nothing to click without problems");
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn the_problem_count_is_clickable_in_the_stacked_layout_and_only_inside_the_title() {
        let mut app = busy_app();
        let now = app.now;
        let screen = draw(&mut app, 80, 24);
        let title = screen.lines().next().unwrap();
        let x = title[..title.find("1 problem").expect(title)]
            .chars()
            .count() as u16;
        app.update(click(x, 0), now);
        assert_eq!(app.mode, Mode::Problems, "{screen}");
        press(&mut app, now, KeyCode::Esc);

        let screen = draw(&mut app, 28, 24);
        let title = screen.lines().next().unwrap();
        assert_eq!(title, "╭ carte · 2 running · 1 pro╮");
        app.update(click(27, 0), now);
        assert_eq!(app.mode, Mode::Normal, "the corner isn't part of it");
        app.update(click(24, 0), now);
        assert_eq!(app.mode, Mode::Problems);
        press(&mut app, now, KeyCode::Esc);

        app.update(Event::Disconnected("gone".into()), now);
        draw(&mut app, 28, 24);
        app.update(click(24, 0), now);
        assert_eq!(
            app.mode,
            Mode::Normal,
            "the waiting screen has nothing to click"
        );
    }

    #[test]
    fn a_tunnel_down_is_a_problem_to_click() {
        let mut snapshot = shared_sample();
        let tunnel = snapshot.tunnel.as_mut().unwrap();
        tunnel.state = TunnelState::Failed;
        tunnel.message = Some("the connector exited".into());
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let screen = draw(&mut app, 120, 18);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[17].starts_with(" ! problems  ? help"), "{screen}");
        let at = lines[0].find("tunnel down").expect(&screen);
        let x = lines[0][..at].chars().count() as u16;
        app.update(click(2, 0), now);
        assert_eq!(app.mode, Mode::Normal);
        app.update(click(x, 0), now);
        assert_eq!(app.mode, Mode::Problems);
        let screen = draw(&mut app, 120, 18);
        assert!(
            screen.contains("cloudflare tunnel: the connector exited"),
            "{screen}"
        );
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
    fn renders_a_share_without_its_visits() {
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
        ] {
            assert!(screen.contains(wanted), "{wanted:?} in\n{screen}");
        }
        assert!(
            !screen.contains("GET /login"),
            "visits are in the requests tab\n{screen}"
        );
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
        let commands = press(&mut app, now, KeyCode::Char('2'));
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
        assert!(screen.contains("2 events │ 3 env"), "{screen}");
        assert!(screen.contains("─ following ╮"), "{screen}");
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
    fn z_zooms_the_right_side_and_joins_the_details() {
        let mut app = busy_app();
        let now = app.now;
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
            press(&mut app, now, code);
        }
        let generation = app.logs.generation;
        log(&mut app, generation, "web", "GET /up 200 in 2ms");
        press(&mut app, now, KeyCode::Char('z'));
        assert_eq!(app.focus, Focus::Logs);
        let screen = draw(&mut app, 120, 12);
        assert_eq!(
            screen,
            "\
╭ nova-billing@main ───────────────────────────────────────────────────────────────────────────────────────────────────╮
│● running · https://nova-billing.test · checkout main · /work/nova-billing · worker pinned until the daemon restarts  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ 1 logs │ 2 events │ 3 env │ 5 requests ─────────────────────────────────────────────────────────────────── following ╮
│web GET /up 200 in 2ms                                                                                                │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯

 ! problems  ? help  q quit  ↑↓ scroll  g top  G follow  / search  n next match  1-5 tabs  z zoom  tab tree"
        );
        assert_eq!(app.tree_area, Rect::default(), "the tree takes no clicks");

        press(&mut app, now, KeyCode::Char('z'));
        let screen = draw(&mut app, 120, 12);
        assert!(screen.contains("╭ carte · 2 running"), "{screen}");
        assert!(
            screen.contains("│● running · https://nova-billing.test"),
            "{screen}"
        );
        assert!(
            screen.contains("│checkout main"),
            "the details stack again\n{screen}"
        );

        assert_eq!(
            app.focus,
            Focus::Tree,
            "z goes back to the focus from before"
        );

        press(&mut app, now, KeyCode::Char('z'));
        press(&mut app, now, KeyCode::Char('/'));
        press(&mut app, now, KeyCode::Char('g'));
        press(&mut app, now, KeyCode::Enter);
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.logs.search, "", "esc clears the search first");
        assert!(app.zoom.is_some());
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.zoom, None, "then goes back");
        assert_eq!(app.focus, Focus::Tree);

        press(&mut app, now, KeyCode::Char('z'));
        press(&mut app, now, KeyCode::Tab);
        assert_eq!(app.zoom, None);
        assert_eq!(app.focus, Focus::Tree, "tab goes back to the tree");
    }

    #[test]
    fn tabs_take_clicks_while_zoomed_and_env_takes_no_search() {
        let mut app = busy_app();
        let now = app.now;
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
            press(&mut app, now, code);
        }
        press(&mut app, now, KeyCode::Char('z'));
        let screen = draw(&mut app, 120, 12);
        let (y, row) = screen
            .lines()
            .enumerate()
            .find(|(_, r)| r.contains("2 events"))
            .expect(&screen);
        let x = row[..row.find("2 events").unwrap()].chars().count() as u16;
        app.update(click(x + 2, y as u16), now);
        assert_eq!(app.tab, Tab::Events, "{screen}");
        assert!(app.zoom.is_some());

        press(&mut app, now, KeyCode::Char('3'));
        press(&mut app, now, KeyCode::Char('/'));
        assert_eq!(app.mode, Mode::Normal, "the env has nothing to search");
        let screen = draw(&mut app, 120, 12);
        assert!(!screen.contains("│ search"), "{screen}");
    }

    #[test]
    fn a_jump_while_zoomed_keeps_the_right_side() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('z'));
        app.update(
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            now,
        );
        for c in "frontend".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        press(&mut app, now, KeyCode::Enter);
        assert_eq!(app.focus, Focus::Logs);
        assert!(app.zoom.is_some());
        let screen = draw(&mut app, 80, 16);
        assert!(screen.starts_with("╭ nova-frontend@main ─"), "{screen}");
        let cursor = app.cursor;
        press(&mut app, now, KeyCode::Down);
        assert_eq!(app.cursor, cursor, "keys don't move the hidden tree");
    }

    #[test]
    fn zoomed_details_join_while_they_fit() {
        let mut app = busy_app();
        let now = app.now;
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Down] {
            press(&mut app, now, code);
        }
        press(&mut app, now, KeyCode::Char('z'));
        let screen = draw(&mut app, 60, 16);
        let rows: Vec<&str> = screen.lines().collect();
        assert_eq!(
            rows[..6],
            [
                "╭ nova-frontend@main ──────────────────────────────────────╮",
                "│✖ crashed · https://nova-frontend.test                    │",
                "│checkout main · /work/nova-frontend                       │",
                "│web exited with code 1 before it was ready                │",
                "│! stack \"billing\" names an app                            │",
                "╰──────────────────────────────────────────────────────────╯",
            ],
            "errors keep their rows, and a fact that doesn't fit starts one\n{screen}"
        );
        let screen = draw(&mut app, 120, 16);
        assert!(
            screen.contains(
                "│✖ crashed · https://nova-frontend.test · checkout main · /work/nova-frontend   "
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
╭ carte · 2 running · 1 problem ───────────────────────────────────────────────╮
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
╭ 1 logs │ 2 events │ 3 env │ 5 requests ─────────────────────────── following ╮
│No output yet.                                                                │
│                                                                              │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯

 ! problems  ? help  q quit  s start  x stop  r restart  p pin  w share"
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

    fn env_response(instance: &str) -> Event {
        let var = |key: &str, value: &str, source: &str| carte_api::EnvVar {
            key: key.into(),
            value: Some(value.into()),
            masked: false,
            source: source.into(),
            overrides: Vec::new(),
        };
        Event::Env {
            generation: 0,
            result: Ok(carte_api::EnvResponse {
                instance: instance.into(),
                vars: vec![
                    var("CARTE_APP", "nova-billing", "carte"),
                    var(
                        "PATH",
                        "/Users/alex/.local/share/mise/installs/ruby/3.4/bin:/usr/local/bin:/usr/bin:/bin",
                        "mise",
                    ),
                    var("PORT", "<assigned at start>", "carte"),
                ],
            }),
        }
    }

    fn answer_env(app: &mut App, instance: &str) {
        let Event::Env { result, .. } = env_response(instance) else {
            unreachable!();
        };
        let generation = app.env.generation;
        app.update(Event::Env { generation, result }, app.now);
    }

    #[test]
    fn renders_a_process_env_that_wraps_and_scrolls() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Char('3'));
        answer_env(&mut app, "nova-billing@main/web");
        let screen = draw(&mut app, 60, 30);
        let pane: Vec<&str> = screen
            .lines()
            .skip_while(|l| !l.starts_with("╭ 1 logs"))
            .take(6)
            .collect();
        assert_eq!(
            pane,
            [
                "╭ 1 logs │ 2 events │ 3 env │ 5 requests ──────────────────╮",
                "│CARTE_APP  nova-billing  carte                            │",
                "│PATH       /Users/alex/.local/share/mise/installs/ruby/3.4│",
                "│           /bin:/usr/local/bin:/usr/bin:/bin  mise        │",
                "│PORT       <assigned at start>  carte                     │",
                "│                                                          │",
            ],
            "{screen}"
        );

        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Char('G'));
        let screen = draw(&mut app, 60, 18);
        assert!(screen.contains("│PORT"), "{screen}");
        assert!(
            !screen.contains("CARTE_APP"),
            "scrolled to the end\n{screen}"
        );
    }

    #[test]
    fn the_env_tab_on_an_app_says_which_process_it_shows() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Char('3'));
        answer_env(&mut app, "nova-billing@main/web");
        let screen = draw(&mut app, 120, 30);
        assert!(
            screen.contains("│The env of web, which serves HTTP. Select a process to see "),
            "{screen}"
        );
        assert!(screen.contains("│another: web, worker."), "{screen}");
        assert!(
            screen.contains("│CARTE_APP  nova-billing  carte"),
            "{screen}"
        );
    }

    #[test]
    fn the_tab_border_marks_the_current_tab_and_mutes_what_doesnt_apply() {
        let mut app = busy_app();
        let now = app.now;
        let screen = draw(&mut app, 120, 30);
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("1 logs │ 2 events │ 3 env"))
            .expect(&screen);
        assert!(line.contains("following ╮"), "{line}");
        let column = |name: &str| {
            let at = line.find(name).unwrap();
            line[..at].chars().count() as u16
        };
        let cell = |app: &mut App, name: &str| {
            let x = column(name);
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
            terminal.draw(|frame| render(frame, app)).unwrap();
            terminal.backend().buffer()[(x, y as u16)].clone()
        };
        let theme = Theme::terminal();
        let logs = cell(&mut app, "1 logs");
        assert_eq!(logs.fg, theme.accent);
        assert!(logs.modifier.contains(Modifier::BOLD), "the current tab");
        assert_eq!(cell(&mut app, "2 events").fg, theme.fg);
        assert_eq!(
            cell(&mut app, "3 env").fg,
            theme.muted,
            "a project has no env"
        );

        let click = |column| {
            Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Left,
                ),
                column,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            })
        };
        let commands = app.update(click(column("events") + 2), now);
        assert_eq!(app.tab, Tab::Events);
        assert!(
            matches!(commands.as_slice(), [Command::FollowEvents { .. }]),
            "{commands:?}"
        );
        assert_eq!(app.focus, Focus::Tree, "a tab click leaves the focus");
        app.update(click(column("1 logs") - 2), now);
        assert_eq!(app.tab, Tab::Events, "the border beside a tab isn't one");
        assert_eq!(cell(&mut app, "2 events").fg, theme.accent);
        let commands = app.update(click(column("3 env")), now);
        assert_eq!(app.tab, Tab::Env, "a muted tab opens too");
        assert_eq!(commands, [Command::StopLogs]);

        app.update(Event::Disconnected("gone".into()), now);
        draw(&mut app, 120, 30);
        app.update(click(column("1 logs")), now);
        assert_eq!(app.tab, Tab::Env, "no tabs while the daemon is away");
    }

    #[test]
    fn the_logs_status_hides_the_tab_names_it_covers() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Tab);
        draw(&mut app, 36, 30);
        let tabs: Vec<Tab> = app.tab_spans.iter().map(|(tab, ..)| *tab).collect();
        assert_eq!(tabs, [Tab::Logs, Tab::Events], "env is under the status");
    }

    fn requests_app() -> App {
        use crate::tui::requests::tests::request;

        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('5'));
        let generation = app.requests.generation;
        let mut shared = request(3, "/admin", Some(500));
        shared.shared = true;
        shared.visitor = Some("203.0.113.9".into());
        shared.held_ms = 1500;
        shared.upstream_ms = Some(250);
        shared.subject = Some(carte_api::EventSubject {
            project: "nova".into(),
            stack: Some("feature-x".into()),
            app: Some("nova-billing".into()),
            process: Some("web".into()),
        });
        shared.route.checkout = Some("main".into());
        shared.route.served = Some(carte_api::Served::Process { port: 41207 });
        shared.route.warmed = vec!["nova-frontend".into()];
        shared.response_headers = vec![carte_api::Header {
            name: "set-cookie".into(),
            value: "session=abc".into(),
        }];
        shared.headers_truncated = true;
        for request in [
            request(1, "/", Some(200)),
            request(2, "/assets/app.css", None),
            shared,
        ] {
            app.update(
                Event::Request {
                    generation,
                    request: Box::new(request),
                },
                now,
            );
        }
        app
    }

    #[test]
    fn renders_requests_with_their_status_and_a_mark_for_shared() {
        let mut app = requests_app();
        let screen = draw(&mut app, 120, 30);
        let time = crate::clock::local_hms(0);
        for wanted in [
            format!("│{time} GET     /                                  200   12ms  │"),
            format!("│{time} GET     /assets/app.css              cancelled   12ms  │"),
            format!("│{time} GET     /admin                             500   1.8s ↗│"),
        ] {
            assert!(screen.contains(&wanted), "{wanted:?} in\n{screen}");
        }
        assert!(screen.contains("5 requests"), "{screen}");
        assert!(screen.contains("─ following ╮"), "{screen}");

        let now = app.now;
        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Up);
        let screen = draw(&mut app, 120, 30);
        assert!(screen.contains("─ 3/3 ╮"), "{screen}");
    }

    #[test]
    fn the_newest_requests_show_while_following() {
        use crate::tui::requests::tests::request;

        let mut app = requests_app();
        let generation = app.requests.generation;
        for seq in 4..40 {
            let path = format!("/page/{seq}");
            app.update(
                Event::Request {
                    generation,
                    request: Box::new(request(seq, &path, Some(200))),
                },
                app.now,
            );
        }
        let screen = draw(&mut app, 120, 30);
        assert!(screen.contains("/page/39"), "{screen}");
        assert!(!screen.contains("/page/4 "), "{screen}");
    }

    #[test]
    fn renders_an_open_request_in_full() {
        let mut app = requests_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Enter);
        let screen = draw(&mut app, 120, 40);
        for wanted in [
            "│GET /admin  500",
            "│host      nova-billing.test",
            "│route     nova-billing@feature-x/web · checkout main",
            "│served by the process on port 41207",
            "│warmed    nova-frontend",
            "│held      1.5s",
            "│upstream  250ms",
            "│shared    from 203.0.113.9",
            "│request headers",
            "│accept: */*",
            "│response headers",
            "│set-cookie: session=abc",
            "│Long headers were cut short, or some left out.",
            "esc back to the list ╮",
        ] {
            assert!(screen.contains(wanted), "{wanted:?} in\n{screen}");
        }
        press(&mut app, now, KeyCode::Esc);
        let screen = draw(&mut app, 120, 40);
        assert!(
            screen.contains("/assets/app.css"),
            "back to the list\n{screen}"
        );
    }

    #[test]
    fn zoomed_the_requests_split_into_the_list_and_the_selected_one() {
        let mut app = requests_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Char('z'));
        let screen = draw(&mut app, 120, 30);
        let row = screen
            .lines()
            .find(|l| l.contains("200   12ms"))
            .expect(&screen);
        assert!(
            row.contains("│GET /admin  500"),
            "the newest beside the list\n{screen}"
        );
        assert!(screen.contains("│request headers"), "{screen}");

        press(&mut app, now, KeyCode::Up);
        press(&mut app, now, KeyCode::Up);
        let screen = draw(&mut app, 120, 30);
        assert!(
            screen.contains("│GET /assets/app.css  cancelled"),
            "{screen}"
        );
        assert!(!screen.contains("GET /admin  500"), "{screen}");

        let detail = app.requests.detail_area;
        app.update(
            Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::ScrollDown,
                column: detail.x + 2,
                row: detail.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        assert_eq!(
            app.requests.detail_scroll, 3,
            "the wheel scrolls the request"
        );
        assert_eq!(app.requests.selected, Some(2), "and leaves the list");
        let screen = draw(&mut app, 120, 30);
        assert!(!screen.contains("enter open"), "{screen}");
        assert!(!screen.contains("esc back"), "{screen}");

        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.zoom, None, "esc goes back from the zoom");
        let screen = draw(&mut app, 120, 30);
        assert!(screen.contains("─ 2/3 ╮"), "the list again\n{screen}");
    }

    #[test]
    fn long_paths_end_in_an_ellipsis_and_durations_round_up() {
        assert_eq!(ellipsis("/assets/app.css", 8), "/assets…");
        assert_eq!(ellipsis("/up", 8), "/up");
        assert_eq!(millis(59_940), "59.9s");
        assert_eq!(millis(59_960), "1m00s");
        assert_eq!(millis(125_400), "2m05s");
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
        let screen = draw(&mut app, 120, 24);
        assert!(
            screen.contains("─ ↑2 ╮"),
            "the term stays off the border\n{screen}"
        );
        assert!(screen.contains("Booting"), "every line stays\n{screen}");
        assert!(screen.contains("││ search ─"), "{screen}");
        assert!(screen.contains("─ 1/2 │"), "{screen}");
        assert!(screen.contains("││ get  "), "{screen}");

        let spans = highlight(&Theme::terminal(), "GET /a then get /b", "get", true);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["", "GET", " /a then ", "get", " /b"]);
        assert_eq!(spans[1].style, Style::new().black().on_yellow());
        let spans = highlight(&Theme::terminal(), "GET /up", "get", false);
        assert_eq!(spans[1].style, Style::new().yellow().bold());
        for (text, search, matched) in [
            ("Fakturaer på Øst: ÆØÅ", "æøå", "ÆØÅ"),
            ("ble Æ og å", "Å", "å"),
            ("ΟΔΟΣ ok", "οδος", "ΟΔΟΣ"),
            ("İzmir İzmir", "iz", "İz"),
            ("to İ and back", "İ", "İ"),
        ] {
            let spans = highlight(&Theme::terminal(), text, search, false);
            let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(texts[1], matched, "{search} in {text}");
            assert_eq!(texts.concat(), text);
        }
    }

    #[test]
    fn the_input_sits_at_the_bottom_of_the_focused_pane() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('/'));
        for c in "bill".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        let screen = draw(&mut app, 120, 30);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(
            rows[25].starts_with("│ filter · enter keeps · esc clears ───"),
            "a rule over the input in a tall pane\n{screen}"
        );
        assert!(rows[26].starts_with("│ bill▏    "), "{screen}");
        assert!(rows[27].starts_with("╰───"), "{screen}");
        assert_eq!(rows[28], "", "the status line doesn't repeat it");

        let screen = draw(&mut app, 80, 24);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(
            rows[9].starts_with("│ filter  bill▏    "),
            "one row in a short pane\n{screen}"
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(10, 9)].symbol(), "b");
        assert_eq!(buffer[(10, 9)].fg, app.theme.fg, "normal text");

        for c in "日本語のログを探す".chars().cycle().take(60) {
            press(&mut app, now, KeyCode::Char(c));
        }
        let screen = draw(&mut app, 80, 24);
        let row = screen.lines().nth(9).unwrap();
        assert!(
            row.ends_with("▏│") || row.ends_with("▏ │"),
            "the cursor stays in view\n{screen}"
        );
        for _ in 0..60 {
            press(&mut app, now, KeyCode::Backspace);
        }

        press(&mut app, now, KeyCode::Enter);
        let screen = draw(&mut app, 80, 24);
        assert!(
            screen.contains("│ filter  bill    "),
            "kept without a cursor\n{screen}"
        );
        press(&mut app, now, KeyCode::Esc);
        let screen = draw(&mut app, 80, 24);
        assert!(!screen.contains(" filter  "), "{screen}");
    }

    #[test]
    fn the_logs_keep_room_for_lines_and_clicks_on_the_input_select_nothing() {
        let mut app = busy_app();
        let now = app.now;
        let generation = app.logs.generation;
        for i in 0..10 {
            log(&mut app, generation, "web", &format!("GET /up {i}"));
        }
        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Char('/'));
        press(&mut app, now, KeyCode::Char('g'));
        press(&mut app, now, KeyCode::Enter);
        let screen = draw(&mut app, 80, 24);
        assert!(app.logs.height >= 3, "{screen}");
        let rows: Vec<&str> = screen.lines().collect();
        let input = rows
            .iter()
            .position(|r| r.starts_with("│ search  g"))
            .expect(&screen);
        let down = Event::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: 10,
            row: input as u16,
            modifiers: KeyModifiers::NONE,
        });
        app.update(down, now);
        assert_eq!(app.selection, None, "{screen}");
        assert!(!app.logs.hold);
    }

    #[test]
    fn ctrl_f_filters_the_logs_instead_of_searching_them() {
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
        let screen = draw(&mut app, 120, 24);
        assert!(screen.contains("Booting"), "{screen}");
        assert!(screen.contains("ctrl-f filters · enter keeps"), "{screen}");

        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        app.update(Event::Key(ctrl_f), now);
        assert_eq!(app.logs.search, "get", "the term stays");
        let screen = draw(&mut app, 120, 24);
        assert!(!screen.contains("Booting"), "{screen}");
        assert!(screen.contains("GET /up"), "{screen}");
        assert!(screen.contains("││ filter · ctrl-f searches"), "{screen}");
        assert!(screen.contains("ctrl-f searches · enter keeps"), "{screen}");

        press(&mut app, now, KeyCode::Char('x'));
        let screen = draw(&mut app, 120, 24);
        assert!(screen.contains("Nothing matches."), "{screen}");

        press(&mut app, now, KeyCode::Esc);
        assert!(!app.logs.filter, "the next search starts as a search");
        let screen = draw(&mut app, 120, 24);
        assert!(screen.contains("Booting"), "{screen}");
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

    #[test]
    fn the_theme_picker_previews_and_goes_back_or_keeps() {
        let mut app = busy_app();
        let now = app.now;
        press(&mut app, now, KeyCode::Char('T'));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Catppuccin Mocha"), "{screen}");
        assert!(screen.contains("Rosé Pine Dawn"), "{screen}");

        press(&mut app, now, KeyCode::Down);
        assert_eq!(app.theme.id, "catppuccin-latte", "moving previews");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            app.theme.bg,
            "the whole screen takes the theme's background"
        );
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.theme, Theme::terminal(), "esc goes back");
        assert_eq!(app.mode, Mode::Normal);

        press(&mut app, now, KeyCode::Char('T'));
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Enter);
        assert_eq!(app.theme.id, "catppuccin-frappe", "enter keeps");
        assert_eq!(app.mode, Mode::Normal);
        press(&mut app, now, KeyCode::Char('T'));
        assert_eq!(
            app.picker.as_ref().unwrap().cursor,
            2,
            "opens at the current theme"
        );
    }

    #[test]
    fn without_truecolor_themes_draw_in_256_colours() {
        let mut app = busy_app();
        app.truecolor = false;
        let now = app.now;
        press(&mut app, now, KeyCode::Char('T'));
        press(&mut app, now, KeyCode::Down);
        assert!(
            matches!(app.theme.bg, Color::Indexed(_)),
            "{:?}",
            app.theme.bg
        );
    }
}
