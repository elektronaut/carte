//! The TUI's state and how events change it. Nothing here touches the terminal or
//! the socket: `update` returns the commands for the IO side to carry out.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use carte_api::{
    Action, ControlRequest, ControlResponse, Credentials, InstanceStatus, LogLine, LogsRequest,
    ShareRequest, ShareResponse, Snapshot, UnshareRequest,
};
use ratatui::crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::input::{self, Confirm, Control, Focus, Intent, Mode};
use super::logs::{Logs, Prefix};
use super::target;
use super::tree::{self, Fold, Node, NodeId, Row, Show};

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    Redraw,
    Snapshot(Box<Snapshot>),
    Disconnected(String),
    Log {
        generation: u64,
        line: LogLine,
    },
    LogsEnded {
        generation: u64,
        error: Option<String>,
    },
    ControlDone {
        verb: &'static str,
        result: Result<ControlResponse, String>,
    },
    Shared(Result<ShareResponse, String>),
    Unshared(Result<Vec<String>, String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Control {
        request: ControlRequest,
        verb: &'static str,
    },
    StopAll {
        idle_only: bool,
    },
    FollowLogs {
        generation: u64,
        request: LogsRequest,
    },
    StopLogs,
    Share(ShareRequest),
    Unshare(UnshareRequest),
    Open(String),
    MouseCapture(bool),
    Copy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Connected,
    Lost(String),
}

/// Log lines picked with the mouse, by sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: u64,
    pub cursor: u64,
    pub dragged: bool,
    /// The pane followed new lines before the selection held it still.
    pub was_following: bool,
}

impl Selection {
    pub fn contains(&self, seq: u64) -> bool {
        (self.anchor.min(self.cursor)..=self.anchor.max(self.cursor)).contains(&seq)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

pub struct App {
    pub connection: Connection,
    pub snapshot: Option<Snapshot>,
    pub snapshot_at: Instant,
    pub now: Instant,
    pub roots: Vec<Node>,
    pub rows: Vec<Row>,
    pub fold: Fold,
    pub cursor: usize,
    pub selected: Option<NodeId>,
    pub focus: Focus,
    pub mode: Mode,
    pub logs: Logs,
    pub notice: Option<Notice>,
    /// Passwords of the shares made here, by project and stack: the daemon only hands
    /// them out when a share is made.
    pub credentials: BTreeMap<(String, String), Credentials>,
    /// Rows the tree showed last time it was drawn.
    pub tree_height: usize,
    /// Where the tree and the logs were drawn, and the tree's first visible row, for
    /// mapping mouse events.
    pub tree_area: Rect,
    pub tree_offset: usize,
    pub logs_area: Rect,
    pub selection: Option<Selection>,
    pub mouse: bool,
    pub quit: bool,
}

impl App {
    pub fn new(now: Instant) -> Self {
        App {
            connection: Connection::Connecting,
            snapshot: None,
            snapshot_at: now,
            now,
            roots: Vec::new(),
            rows: Vec::new(),
            fold: Fold::default(),
            cursor: 0,
            selected: None,
            focus: Focus::Tree,
            mode: Mode::Normal,
            logs: Logs::default(),
            notice: None,
            credentials: BTreeMap::new(),
            tree_height: 0,
            tree_area: Rect::default(),
            tree_offset: 0,
            logs_area: Rect::default(),
            selection: None,
            mouse: true,
            quit: false,
        }
    }

    pub fn update(&mut self, event: Event, now: Instant) -> Vec<Command> {
        self.now = now;
        let mut commands = Vec::new();
        match event {
            Event::Key(key) => {
                if let Some(intent) = input::intent(&self.mode, self.focus, key) {
                    self.apply(intent, &mut commands);
                }
            }
            Event::Mouse(mouse) if self.mode == Mode::Normal => self.mouse(mouse, &mut commands),
            Event::Mouse(_) | Event::Tick | Event::Redraw => {}
            Event::Snapshot(snapshot) => {
                self.connection = Connection::Connected;
                self.credentials.retain(|(project, stack), _| {
                    snapshot
                        .shares
                        .iter()
                        .any(|s| s.project == *project && s.stack == *stack)
                });
                self.snapshot = Some(*snapshot);
                self.snapshot_at = now;
                self.rebuild();
            }
            Event::Disconnected(message) => {
                self.connection = Connection::Lost(message);
                self.snapshot = None;
                self.rebuild();
            }
            Event::Log { generation, line } => self.logs.push(generation, line),
            Event::LogsEnded { generation, error } => self.logs.end(generation, error),
            Event::ControlDone { verb, result } => self.control_done(verb, result),
            Event::Shared(result) => self.shared(result),
            Event::Unshared(Ok(apps)) => {
                self.inform(&format!("no longer shared: {}", apps.join(", ")))
            }
            Event::Unshared(Err(message)) => self.warn(&message),
        }
        if let Some(notice) = &self.notice {
            let lasts = Duration::from_secs(if notice.error { 10 } else { 5 });
            if now.duration_since(notice.at) > lasts {
                self.notice = None;
            }
        }
        commands.extend(self.sync_logs());
        commands
    }

    pub fn selected_node(&self) -> Option<&Node> {
        let row = self.rows.get(self.cursor)?;
        Some(tree::node(&self.roots, &row.path))
    }

    /// The instance a node belongs to, for nodes at instance level or below.
    pub fn instance(&self, id: &NodeId) -> Option<&InstanceStatus> {
        let (NodeId::Instance {
            project,
            stack,
            app,
        }
        | NodeId::Process {
            project,
            stack,
            app,
            ..
        }) = id
        else {
            return None;
        };
        self.snapshot
            .as_ref()?
            .projects
            .iter()
            .find(|p| p.name == *project)?
            .stacks
            .iter()
            .find(|s| s.name == *stack)?
            .instances
            .iter()
            .find(|i| i.app == *app)
    }

    /// Seconds left before an instance idles out, counted down since the snapshot.
    pub fn idle_in(&self, secs: u64) -> u64 {
        secs.saturating_sub(self.now.duration_since(self.snapshot_at).as_secs())
    }

    fn rebuild(&mut self) {
        self.roots = self.snapshot.as_ref().map(tree::build).unwrap_or_default();
        self.refresh_rows();
    }

    /// Recomputes the visible rows and keeps the selection on the same node, or the
    /// closest ancestor still shown.
    fn refresh_rows(&mut self) {
        self.rows = self.fold.rows(&self.roots);
        let mut wanted = self.selected.clone();
        while let Some(id) = wanted {
            if let Some(i) = self
                .rows
                .iter()
                .position(|row| tree::node(&self.roots, &row.path).ids.contains(&id))
            {
                self.cursor = i;
                break;
            }
            wanted = id.parent();
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        self.selected = self.selected_node().map(|n| n.id().clone());
    }

    fn select(&mut self, index: usize) {
        self.cursor = index.min(self.rows.len().saturating_sub(1));
        self.selected = self.selected_node().map(|n| n.id().clone());
    }

    /// The wheel scrolls the pane under the pointer; a click focuses a pane, selects a
    /// tree row, or folds and unfolds the row that is already selected. Dragging over
    /// the logs selects lines and releasing copies them.
    fn mouse(&mut self, event: MouseEvent, commands: &mut Vec<Command>) {
        let at = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.logs_area.contains(at) => {
                self.end_selection();
                self.focus = Focus::Logs;
                let top = self.logs_area.y + 1;
                if at.y >= top
                    && let Some(seq) = self.logs.seq_at(usize::from(at.y - top))
                {
                    self.selection = Some(Selection {
                        anchor: seq,
                        cursor: seq,
                        dragged: false,
                        was_following: self.logs.scroll == 0,
                    });
                    self.logs.hold = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selection.is_some() => {
                let top = self.logs_area.y + 1;
                let bottom = top + self.logs.height as u16;
                let row = if at.y < top {
                    self.logs.scroll_by(1);
                    0
                } else if at.y >= bottom {
                    self.logs.scroll_by(-1);
                    self.logs.height.saturating_sub(1)
                } else {
                    usize::from(at.y - top)
                };
                if let Some(seq) = self.logs.seq_at(row)
                    && let Some(selection) = &mut self.selection
                {
                    selection.cursor = seq;
                    selection.dragged = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(selection) = self.selection else {
                    return;
                };
                if !selection.dragged {
                    self.end_selection();
                    return;
                }
                let (text, count) = self.logs.text_between(selection.anchor, selection.cursor);
                commands.push(Command::Copy(text));
                self.inform(&if count == 1 {
                    "copied 1 line".to_string()
                } else {
                    format!("copied {count} lines")
                });
                self.release_hold(selection);
                self.selection = Some(Selection {
                    dragged: false,
                    was_following: false,
                    ..selection
                });
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = event.kind == MouseEventKind::ScrollUp;
                if self.logs_area.contains(at) {
                    self.logs.scroll_by(if up { 3 } else { -3 });
                } else if self.tree_area.contains(at) {
                    self.move_by(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.logs_area.contains(at) {
                    self.focus = Focus::Logs;
                } else if self.tree_area.contains(at) {
                    self.end_selection();
                    self.focus = Focus::Tree;
                    let first = self.tree_area.y + 2;
                    if at.y < first || at.y + 1 >= self.tree_area.bottom() {
                        return;
                    }
                    let index = self.tree_offset + usize::from(at.y - first);
                    if index == self.cursor {
                        self.apply(Intent::Toggle, &mut Vec::new());
                    } else if index < self.rows.len() {
                        self.select(index);
                    }
                }
            }
            _ => {}
        }
    }

    /// Lets the logs move again, back to following if they did before.
    fn release_hold(&mut self, selection: Selection) {
        self.logs.hold = false;
        if selection.was_following {
            self.logs.follow();
        }
    }

    fn end_selection(&mut self) {
        if let Some(selection) = self.selection.take() {
            self.release_hold(selection);
        }
    }

    fn move_by(&mut self, delta: isize) {
        self.select(self.cursor.saturating_add_signed(delta));
    }

    fn apply(&mut self, intent: Intent, commands: &mut Vec<Command>) {
        let page = |height: usize| height.saturating_sub(1).max(1) as isize;
        match (intent, self.focus) {
            (Intent::Quit, _) => self.quit = true,
            (Intent::Help, _) => self.mode = Mode::Help,
            (Intent::Problems, _) => self.mode = Mode::Problems,
            (Intent::Close, _) => self.mode = Mode::Normal,
            (Intent::SwitchFocus, _) => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Logs,
                    Focus::Logs => Focus::Tree,
                }
            }
            (Intent::Move(delta), Focus::Tree) => self.move_by(delta),
            (Intent::Move(delta), Focus::Logs) => self.logs.scroll_by(-delta),
            (Intent::PageDown, Focus::Tree) => self.move_by(page(self.tree_height)),
            (Intent::PageUp, Focus::Tree) => self.move_by(-page(self.tree_height)),
            (Intent::PageDown, Focus::Logs) => self.logs.scroll_by(-page(self.logs.height)),
            (Intent::PageUp, Focus::Logs) => self.logs.scroll_by(page(self.logs.height)),
            (Intent::Top, Focus::Tree) => self.select(0),
            (Intent::Bottom, Focus::Tree) => self.select(usize::MAX),
            (Intent::Top, Focus::Logs) => self.logs.scroll_to_top(),
            (Intent::Bottom, Focus::Logs) => self.logs.follow(),
            (Intent::Collapse, _) => self.collapse(),
            (Intent::Expand, _) => self.expand(),
            (Intent::Toggle, _) => {
                if let Some(row) = self.rows.get(self.cursor).filter(|r| r.expandable) {
                    let expanded = !row.expanded;
                    self.set_expanded(expanded);
                }
            }
            (Intent::StartFilter, _) => self.mode = Mode::Filter,
            (Intent::FilterDone, _) => self.mode = Mode::Normal,
            (Intent::ClearFilter, _) => {
                self.mode = Mode::Normal;
                self.edit_filter(|filter| filter.clear());
            }
            (Intent::FilterChar(c), _) => self.edit_filter(|filter| filter.push(c)),
            (Intent::FilterBackspace, _) => self.edit_filter(|filter| {
                filter.pop();
            }),
            (Intent::ToggleMouse, _) => {
                self.mouse = !self.mouse;
                commands.push(Command::MouseCapture(self.mouse));
                self.inform(if self.mouse {
                    "Mouse on"
                } else {
                    "Mouse off: drag to select text, m turns it back on"
                });
            }
            (Intent::CycleShow, _) => {
                self.fold.show = self.fold.show.next();
                self.refresh_rows();
                self.inform(match self.fold.show {
                    Show::All => "showing every instance",
                    Show::Running => "showing only running instances",
                    Show::Shared => "showing only shared instances",
                });
            }
            (Intent::Control(control), _) => commands.extend(self.control(control)),
            (Intent::StopIdle, _) => commands.push(Command::StopAll { idle_only: true }),
            (Intent::StopAll, _) => self.mode = Mode::Confirm(Confirm::StopAll),
            (Intent::Yes, _) => match std::mem::replace(&mut self.mode, Mode::Normal) {
                Mode::Confirm(Confirm::StopAll) => {
                    commands.push(Command::StopAll { idle_only: false })
                }
                Mode::Confirm(Confirm::ReplaceShare { request, .. }) => {
                    commands.push(Command::Share(ShareRequest {
                        replace: true,
                        ..request
                    }))
                }
                _ => {}
            },
            (Intent::Open, _) => commands.extend(self.open()),
            (Intent::OpenPublic, _) => commands.extend(self.open_public()),
            (Intent::Share { auth }, _) => commands.extend(self.share(auth)),
        }
    }

    fn edit_filter(&mut self, edit: impl FnOnce(&mut String)) {
        match self.focus {
            Focus::Tree => {
                edit(&mut self.fold.filter);
                self.refresh_rows();
            }
            Focus::Logs => {
                let mut filter = self.logs.filter.clone();
                edit(&mut filter);
                self.logs.set_filter(filter);
            }
        }
    }

    fn set_expanded(&mut self, expanded: bool) {
        if let Some(id) = self.selected.clone() {
            self.fold.expanded.insert(id, expanded);
            self.refresh_rows();
        }
    }

    fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        if row.expanded {
            self.set_expanded(false);
        } else if let Some(parent) = self.rows[..self.cursor]
            .iter()
            .rposition(|r| r.depth < row.depth)
        {
            self.select(parent);
        }
    }

    fn expand(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        if row.expanded {
            self.move_by(1);
        } else if row.expandable {
            self.set_expanded(true);
        }
    }

    fn control(&mut self, control: Control) -> Option<Command> {
        if control == Control::Save {
            return self.save();
        }
        let node = self.selected_node()?;
        if matches!(node.id(), NodeId::Folder { .. }) {
            self.warn("select a project, stack or app in the folder");
            return None;
        }
        let action = match control {
            Control::Start => Action::Up,
            Control::Stop => Action::Down,
            Control::Restart => Action::Restart,
            Control::TogglePin if node.status.pinned.is_some() => Action::Unpin { save: false },
            Control::TogglePin => Action::Pin { save: false },
            Control::ToggleDisable if node.status.disabled.is_some() => {
                Action::Enable { save: false }
            }
            Control::ToggleDisable => Action::Disable { save: false },
            Control::Save => unreachable!("handled by save"),
        };
        let verb = match action {
            Action::Up => "starting",
            Action::Down => "stopping",
            Action::Restart => "restarting",
            Action::Pin { .. } => "pinned",
            Action::Unpin { .. } => "unpinned",
            Action::Disable { .. } => "disabled",
            Action::Enable { .. } => "enabled",
        };
        Some(Command::Control {
            request: ControlRequest {
                target: target::address(node.id()),
                action,
            },
            verb,
        })
    }

    /// Writes the temporary pin or disable of the selected process, or of the one
    /// process below the selection that has one, to the project file.
    fn save(&mut self) -> Option<Command> {
        let node = self.selected_node()?;
        let mut candidates = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if matches!(n.id(), NodeId::Process { .. }) {
                let temporary = Some(tree::Source::Temporary);
                if n.status.pinned == temporary {
                    candidates.push((n.id().clone(), Action::Pin { save: true }));
                } else if n.status.disabled == temporary {
                    candidates.push((n.id().clone(), Action::Disable { save: true }));
                }
            }
            stack.extend(&n.children);
        }
        let request = match candidates.as_slice() {
            [(id, action)] => ControlRequest {
                target: target::address(id),
                action: *action,
            },
            [] => {
                self.inform("Nothing temporary to save here");
                return None;
            }
            _ => {
                self.inform("Several temporary changes here; select the process to save");
                return None;
            }
        };
        Some(Command::Control {
            request,
            verb: "saved",
        })
    }

    fn control_done(&mut self, verb: &str, result: Result<ControlResponse, String>) {
        match result {
            Ok(response) => {
                let mut parts = Vec::new();
                if response.affected.is_empty() {
                    parts.push("nothing to do".to_string());
                } else {
                    parts.push(format!("{verb} {}", response.affected.join(", ")));
                }
                parts.extend(response.messages);
                self.inform(&parts.join("; "));
            }
            Err(message) => self.warn(&message),
        }
    }

    fn open(&mut self) -> Option<Command> {
        let id = self.selected_node()?.id().clone();
        match self.instance(&id) {
            Some(instance) => Some(Command::Open(instance.url.clone())),
            None => {
                self.warn("select an app to open");
                None
            }
        }
    }

    /// The instance under the selection, for an instance or one of its processes.
    fn selected_instance(&self) -> Option<(NodeId, &InstanceStatus)> {
        let id = match self.selected_node()?.id() {
            id @ NodeId::Process { .. } => id.parent()?,
            id => id.clone(),
        };
        let instance = self.instance(&id)?;
        Some((id, instance))
    }

    fn open_public(&mut self) -> Option<Command> {
        match self.selected_instance() {
            Some((_, instance)) => match instance.public_url.clone() {
                Some(url) => Some(Command::Open(url)),
                None => {
                    self.warn("not shared; w shares it");
                    None
                }
            },
            None => {
                self.warn("select an app to open");
                None
            }
        }
    }

    /// Shares the selected app, or ends its stack's share. With `auth` it shares, or
    /// adds a password to the share. When the provider carries one app at a time and
    /// another app has it, asks before taking its place.
    fn share(&mut self, auth: bool) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        let Some((id, instance)) = self.selected_instance() else {
            self.warn("select an app to share");
            return None;
        };
        let target = target::address(&id);
        if instance.public_url.is_some() && !auth {
            return Some(Command::Unshare(UnshareRequest { target }));
        }
        let request = ShareRequest {
            target,
            apps: Vec::new(),
            ttl_secs: None,
            auth,
            pin: false,
            replace: false,
        };
        let single_host = snapshot.tunnel.as_ref().is_some_and(|t| t.single_host);
        let (project, stack, app): (&str, &str, &str) =
            (id.project(), &instance.stack, &instance.app);
        let others: Vec<String> = snapshot
            .shares
            .iter()
            .flat_map(|share| {
                share.apps.iter().filter_map(move |a| {
                    let same = share.project == project && share.stack == stack && a.app == app;
                    (!same).then(|| format!("{}@{}", a.app, share.stack))
                })
            })
            .collect();
        if single_host && !others.is_empty() {
            self.mode = Mode::Confirm(Confirm::ReplaceShare {
                request,
                shared: others.join(", "),
            });
            return None;
        }
        Some(Command::Share(request))
    }

    fn shared(&mut self, result: Result<ShareResponse, String>) {
        let response = match result {
            Ok(response) => response,
            Err(message) => return self.warn(&message),
        };
        let share = &response.share;
        let urls: Vec<&str> = share.apps.iter().map(|a| a.url.as_str()).collect();
        let mut parts = vec![format!("shared at {}", urls.join(", "))];
        if let Some(credentials) = &response.credentials {
            parts.push(format!(
                "user {}, password {}",
                credentials.user, credentials.password
            ));
            self.credentials.insert(
                (share.project.clone(), share.stack.clone()),
                credentials.clone(),
            );
        }
        parts.extend(response.messages);
        self.inform(&parts.join("; "));
    }

    fn inform(&mut self, text: &str) {
        self.notice = Some(Notice {
            text: text.to_string(),
            error: false,
            at: self.now,
        });
    }

    fn warn(&mut self, text: &str) {
        self.notice = Some(Notice {
            text: text.to_string(),
            error: true,
            at: self.now,
        });
    }

    /// Starts a new log stream when the selection changed.
    fn sync_logs(&mut self) -> Option<Command> {
        let wanted = self.log_key();
        if wanted == self.logs.key {
            return None;
        }
        let prefix = self.log_prefix();
        self.selection = None;
        let generation = self.logs.restart(wanted.clone(), prefix);
        if wanted.is_none() {
            return Some(Command::StopLogs);
        }
        let Some(node) = self.selected_node() else {
            return Some(Command::StopLogs);
        };
        Some(Command::FollowLogs {
            generation,
            request: LogsRequest {
                target: target::address(node.id()),
                follow: true,
                tail: Some(1000),
                search: None,
            },
        })
    }

    fn log_key(&self) -> Option<String> {
        self.snapshot.as_ref()?;
        let id = self.selected_node()?.id();
        if matches!(id, NodeId::Folder { .. }) {
            return None;
        }
        Some(target::label(id))
    }

    fn log_prefix(&self) -> Prefix {
        match self.selected_node().map(Node::id) {
            Some(NodeId::Folder { .. } | NodeId::Project { .. }) => Prefix::Full,
            Some(NodeId::Stack { .. }) => Prefix::AppProcess,
            Some(id @ NodeId::Instance { .. })
                if self.instance(id).is_some_and(|i| i.processes.len() > 1) =>
            {
                Prefix::Process
            }
            _ => Prefix::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use carte_api::Address;

    use super::*;
    use crate::tui::fixtures::{sample, shared_sample};

    fn connected() -> (App, Instant) {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(sample())), now);
        (app, now)
    }

    fn press(app: &mut App, now: Instant, code: KeyCode) -> Vec<Command> {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), now)
    }

    fn follows(commands: &[Command]) -> Vec<String> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::FollowLogs { request, .. } => Some(match &request.target {
                    Address {
                        address, project, ..
                    } if address.is_empty() => project.clone().unwrap_or_default(),
                    target => target.address.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_first_snapshot_selects_the_top_and_follows_its_logs() {
        let now = Instant::now();
        let mut app = App::new(now);
        let commands = app.update(Event::Snapshot(Box::new(sample())), now);
        assert_eq!(follows(&commands), ["nova"]);
        assert_eq!(
            app.selected,
            Some(NodeId::Project {
                project: "nova".into()
            })
        );
    }

    #[test]
    fn moving_follows_the_new_selection() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        let commands = press(&mut app, now, KeyCode::Down);
        assert_eq!(follows(&commands), ["nova-billing@main"]);
        assert_eq!(app.logs.prefix, Prefix::Process);
        assert!(press(&mut app, now, KeyCode::Char('?')).is_empty());
    }

    #[test]
    fn controls_address_the_selection() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        let commands = press(&mut app, now, KeyCode::Char('r'));
        let [Command::Control { request, verb }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(request.target.address, "nova-billing@main");
        assert_eq!(request.action, Action::Restart);
        assert_eq!(*verb, "restarting");
    }

    #[test]
    fn saving_writes_the_temporary_pin() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        assert!(press(&mut app, now, KeyCode::Char('S')).is_empty());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("Nothing temporary to save here")
        );

        let mut snapshot = sample();
        let worker = &mut snapshot.projects[0].stacks[0].instances[0].processes[1];
        worker.policy = carte_api::Policy::Pinned;
        worker.temporary = true;
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        let commands = press(&mut app, now, KeyCode::Char('S'));
        let [Command::Control { request, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(request.target.address, "nova-billing@main/worker");
        assert_eq!(request.action, Action::Pin { save: true });
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let (mut app, now) = connected();
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.logs_area = Rect::new(40, 0, 40, 20);
        app.logs.height = 5;
        for i in 0..50 {
            app.logs.push(
                app.logs.generation,
                LogLine {
                    project: "nova".into(),
                    app: "nova-billing".into(),
                    stack: "main".into(),
                    process: "web".into(),
                    at_ms: i,
                    text: format!("line {i}"),
                },
            );
        }
        app.update(mouse(MouseEventKind::ScrollUp, 50, 5), now);
        assert_eq!(app.logs.scroll, 3);
        assert_eq!(app.cursor, 0);
        app.update(mouse(MouseEventKind::ScrollDown, 10, 5), now);
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn dragging_over_the_logs_copies_whole_lines() {
        let (mut app, now) = connected();
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.logs_area = Rect::new(40, 0, 40, 7);
        app.logs.height = 5;
        let push = |app: &mut App, i: u64| {
            app.logs.push(
                app.logs.generation,
                LogLine {
                    project: "nova".into(),
                    app: "nova-billing".into(),
                    stack: "main".into(),
                    process: "web".into(),
                    at_ms: i,
                    text: format!("line {i}"),
                },
            );
        };
        for i in 0..20 {
            push(&mut app, i);
        }
        let left = MouseButton::Left;

        assert!(
            app.update(mouse(MouseEventKind::Down(left), 50, 2), now)
                .is_empty()
        );
        assert!(
            app.update(mouse(MouseEventKind::Up(left), 50, 2), now)
                .is_empty()
        );
        assert_eq!(app.selection, None, "a click selects nothing");
        assert_eq!(app.focus, Focus::Logs);

        app.update(mouse(MouseEventKind::Down(left), 50, 2), now);
        push(&mut app, 20);
        app.update(mouse(MouseEventKind::Drag(left), 50, 4), now);
        let commands = app.update(mouse(MouseEventKind::Up(left), 50, 4), now);
        assert_eq!(
            commands,
            [Command::Copy(
                "nova-billing@main/web line 16\nnova-billing@main/web line 17\nnova-billing@main/web line 18"
                    .into()
            )],
            "the view held still while a line arrived"
        );
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("copied 3 lines")
        );
        assert_eq!(app.logs.scroll, 0, "following again");
        assert!(
            app.update(mouse(MouseEventKind::Up(left), 50, 4), now)
                .is_empty(),
            "copies once"
        );

        app.update(mouse(MouseEventKind::Down(left), 50, 5), now);
        app.update(mouse(MouseEventKind::Drag(left), 50, 0), now);
        app.update(mouse(MouseEventKind::Drag(left), 50, 0), now);
        let commands = app.update(mouse(MouseEventKind::Up(left), 50, 0), now);
        let [Command::Copy(text)] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(
            text.lines().count(),
            7,
            "dragging past the top scrolls and keeps selecting"
        );
    }

    #[test]
    fn clicks_select_rows_focus_panes_and_fold() {
        let (mut app, now) = connected();
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.logs_area = Rect::new(40, 0, 40, 20);
        let click = |column, row| mouse(MouseEventKind::Down(MouseButton::Left), column, row);

        app.update(click(50, 3), now);
        assert_eq!(app.focus, Focus::Logs);
        app.update(click(5, 4), now);
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(app.cursor, 2);
        let rows = app.rows.len();
        app.update(click(5, 4), now);
        assert_ne!(app.rows.len(), rows, "clicking the selected row folds it");
        app.update(click(5, 0), now);
        assert_eq!(app.cursor, 2, "the border and header aren't rows");
    }

    #[test]
    fn sharing_toggles_the_selected_app_and_keeps_its_password() {
        let (mut app, now) = connected();
        assert!(press(&mut app, now, KeyCode::Char('w')).is_empty());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("select an app to share")
        );

        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        let commands = press(&mut app, now, KeyCode::Char('W'));
        let [Command::Share(request)] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(request.target.address, "nova-billing@main");
        assert!(request.auth);

        let snapshot = shared_sample();
        app.update(
            Event::Shared(Ok(ShareResponse {
                share: snapshot.shares[0].clone(),
                credentials: Some(Credentials {
                    user: "carte".into(),
                    password: "s3cret".into(),
                }),
                messages: Vec::new(),
            })),
            now,
        );
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some(
                "shared at https://nova-billing--main--nova.demo.dev; user carte, password s3cret"
            )
        );
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        assert_eq!(app.credentials.len(), 1);

        let commands = press(&mut app, now, KeyCode::Char('w'));
        let [Command::Unshare(request)] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(request.target.address, "nova-billing@main");
        assert_eq!(
            press(&mut app, now, KeyCode::Char('O')),
            [Command::Open(
                "https://nova-billing--main--nova.demo.dev".into()
            )]
        );

        app.update(Event::Snapshot(Box::new(sample())), now);
        assert!(
            app.credentials.is_empty(),
            "an ended share forgets its password"
        );
    }

    #[test]
    fn a_folder_has_no_logs_or_controls() {
        let mut snapshot = sample();
        snapshot.projects[0].folder = Some("clients".into());
        let now = Instant::now();
        let mut app = App::new(now);
        let commands = app.update(Event::Snapshot(Box::new(snapshot)), now);
        assert!(follows(&commands).is_empty(), "{commands:?}");
        assert_eq!(app.rows.len(), 1, "folders start folded");
        assert!(press(&mut app, now, KeyCode::Char('x')).is_empty());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("select a project, stack or app in the folder")
        );
        press(&mut app, now, KeyCode::Right);
        let commands = press(&mut app, now, KeyCode::Down);
        assert_eq!(follows(&commands), ["nova"]);
    }

    #[test]
    fn a_one_app_provider_asks_before_switching_the_share() {
        let now = Instant::now();
        let mut app = App::new(now);
        let mut snapshot = shared_sample();
        snapshot.tunnel.as_mut().unwrap().single_host = true;
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        for _ in 0..3 {
            press(&mut app, now, KeyCode::Down);
        }
        assert!(press(&mut app, now, KeyCode::Char('w')).is_empty());
        let Mode::Confirm(Confirm::ReplaceShare { shared, .. }) = &app.mode else {
            panic!("{:?}", app.mode);
        };
        assert_eq!(shared, "nova-billing@main");
        assert!(press(&mut app, now, KeyCode::Char('n')).is_empty());
        assert_eq!(app.mode, Mode::Normal);

        press(&mut app, now, KeyCode::Char('w'));
        let commands = press(&mut app, now, KeyCode::Char('y'));
        let [Command::Share(request)] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(request.target.address, "nova-frontend@main");
        assert!(request.replace);
    }

    #[test]
    fn stopping_everything_asks_first() {
        let (mut app, now) = connected();
        assert!(press(&mut app, now, KeyCode::Char('X')).is_empty());
        assert_eq!(app.mode, Mode::Confirm(Confirm::StopAll));
        assert_eq!(
            press(&mut app, now, KeyCode::Char('y')),
            [Command::StopAll { idle_only: false }]
        );
        press(&mut app, now, KeyCode::Char('X'));
        assert!(press(&mut app, now, KeyCode::Char('n')).is_empty());
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn folding_keeps_the_cursor_on_the_folded_node() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        assert_eq!(app.rows.len(), 9);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Left);
        assert_eq!(app.cursor, 2, "left on a leaf goes to the parent");
        press(&mut app, now, KeyCode::Left);
        assert_eq!(app.rows.len(), 7);
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn filtering_narrows_the_tree_as_you_type() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Char('/'));
        for c in "front".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        assert_eq!(app.fold.filter, "front");
        assert_eq!(app.rows.len(), 5);
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.fold.filter, "");
        assert_eq!(app.rows.len(), 7);
    }

    #[test]
    fn a_cycles_through_all_running_and_shared() {
        let (mut app, now) = connected();
        for show in [Show::Running, Show::Shared, Show::All] {
            press(&mut app, now, KeyCode::Char('a'));
            assert_eq!(app.fold.show, show);
        }
    }

    #[test]
    fn a_snapshot_keeps_the_selection_by_identity() {
        let (mut app, now) = connected();
        for _ in 0..6 {
            press(&mut app, now, KeyCode::Down);
        }
        let selected = app.selected.clone();
        let mut next = sample();
        next.projects[0].stacks[0].instances.remove(0);
        next.projects[0].apps.remove(0);
        next.projects[0].stacks[1].instances.remove(0);
        app.update(Event::Snapshot(Box::new(next)), now);
        assert_eq!(app.selected, selected);
        assert_eq!(app.cursor, 2, "single-app stacks fold into one row each");
    }

    #[test]
    fn losing_the_daemon_stops_the_logs_and_reconnecting_resumes_them() {
        let (mut app, now) = connected();
        let commands = app.update(Event::Disconnected("gone".into()), now);
        assert_eq!(commands, [Command::StopLogs]);
        let commands = app.update(Event::Snapshot(Box::new(sample())), now);
        assert_eq!(follows(&commands), ["nova"]);
    }

    #[test]
    fn notices_expire() {
        let (mut app, now) = connected();
        app.update(
            Event::ControlDone {
                verb: "stopping",
                result: Ok(ControlResponse {
                    affected: vec!["nova-billing@main".into()],
                    messages: Vec::new(),
                }),
            },
            now,
        );
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("stopping nova-billing@main")
        );
        app.update(Event::Tick, now + Duration::from_secs(6));
        assert_eq!(app.notice, None);
    }
}
