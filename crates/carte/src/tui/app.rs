//! The TUI's state and how events change it. Nothing here touches the terminal or
//! the socket: `update` returns the commands for the IO side to carry out.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use carte_api::attach::{AttachRequest, Attached};
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Credentials, EnvResponse, EventsRequest,
    InstanceStatus, KubeProjectInfo, KubeServiceEntry, LogLine, LogsRequest, ProxiedRequest,
    RequestsRequest, ShareRequest, ShareResponse, Snapshot, UnshareRequest,
};
use carte_core::detach::{DetachFilter, DetachKeys};
use ratatui::crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::browse::{self, Browse};
use super::input::{self, Confirm, Control, Focus, Intent, Mode};
use super::jump::{self, Jump};
use super::logs::{Logs, Prefix};
use super::requests::Requests;
use super::tabs::{EnvSubject, EnvTab, Tab};
use super::target;
use super::theme::{self, Colors, Theme};
use super::tree::{self, Fold, Node, NodeId, Row, Show};
use crate::logs::Stripper;

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
    Request {
        generation: u64,
        request: Box<ProxiedRequest>,
    },
    RequestsEnded {
        generation: u64,
        error: Option<String>,
    },
    ControlDone {
        verb: &'static str,
        result: Result<ControlResponse, String>,
    },
    Shared(Result<ShareResponse, String>),
    Unshared(Result<Vec<String>, String>),
    Env {
        generation: u64,
        result: Result<EnvResponse, String>,
    },
    /// A kube project's Services, as the cluster lists them.
    Services {
        project: String,
        result: Result<Vec<KubeServiceEntry>, String>,
    },
    /// The process typing goes to, or why there is none.
    Attached {
        session: u64,
        result: Result<Attached, String>,
    },
    /// Output of the process typing goes to.
    TypingOutput {
        session: u64,
        data: Vec<u8>,
    },
    /// The process typing went to exited, or the connection to it was lost.
    TypingEnded {
        session: u64,
        message: Option<String>,
    },
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
    /// Follows events into the log pane, as `Event::Log` lines.
    FollowEvents {
        generation: u64,
        request: EventsRequest,
    },
    StopLogs,
    /// Follows the requests the proxy answers into the requests tab.
    FollowRequests {
        generation: u64,
        request: RequestsRequest,
    },
    StopRequests,
    /// Sends a `Tick` after the delay, for what waits on a short pause.
    Wake(Duration),
    Share(ShareRequest),
    Unshare(UnshareRequest),
    Open(String),
    Mark(Address),
    MouseCapture(bool),
    Copy(String),
    FetchEnv {
        generation: u64,
        target: Address,
    },
    /// Lists a kube project's Services.
    FetchServices(String),
    /// Connects to a process to type into it; its events carry `session`.
    Attach {
        session: u64,
        request: AttachRequest,
    },
    /// Types into the process.
    Type(Vec<u8>),
    Detach,
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

/// Typing into a process, from the log pane.
pub struct Typing {
    session: u64,
    /// `app@stack/process` once attached, the selection's address before.
    pub label: String,
    pub attached: bool,
    filter: DetachFilter,
    stripper: Stripper,
}

impl Typing {
    /// The process's unfinished last line, like a prompt and what's typed after it.
    pub fn pending(&self) -> String {
        self.stripper.pending()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// The theme popup: moving through it shows each theme until enter keeps one or esc
/// goes back to `previous`.
pub struct ThemePicker {
    pub themes: Vec<Theme>,
    pub cursor: usize,
    pub previous: Theme,
}

pub struct App {
    pub connection: Connection,
    /// The daemon's snapshot with the kube projects' listed Services merged in.
    pub snapshot: Option<Snapshot>,
    /// The daemon's snapshot as it came.
    daemon_snapshot: Option<Snapshot>,
    /// Each kube project's listing, once it was expanded.
    pub browse: BTreeMap<String, Browse>,
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
    pub requests: Requests,
    /// The tab the pane below the details shows.
    pub tab: Tab,
    /// While the details and tabs fill the main area without the tree, the focus
    /// from before.
    pub zoom: Option<Focus>,
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
    /// Where the tree's title counts problems or says the tunnel is down; empty while
    /// there are none.
    pub problems_area: Rect,
    pub selection: Option<Selection>,
    pub env: EnvTab,
    /// Where each tab's name was drawn on the top border of `logs_area`: its first
    /// column and the column after it.
    pub tab_spans: Vec<(Tab, u16, u16)>,
    pub jump: Option<Jump>,
    /// Selected and revealed once the first snapshot arrives.
    pub start_at: Option<NodeId>,
    pub mouse: bool,
    pub quit: bool,
    /// The keys that stop typing into a process.
    pub detach_keys: DetachKeys,
    pub typing: Option<Typing>,
    pub theme: Theme,
    pub colors: Colors,
    /// Whether a theme was picked this session.
    pub picked_theme: bool,
    pub picker: Option<ThemePicker>,
    typing_sessions: u64,
}

impl App {
    pub fn new(now: Instant) -> Self {
        App {
            connection: Connection::Connecting,
            snapshot: None,
            daemon_snapshot: None,
            browse: BTreeMap::new(),
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
            requests: Requests::default(),
            tab: Tab::Logs,
            zoom: None,
            notice: None,
            credentials: BTreeMap::new(),
            tree_height: 0,
            tree_area: Rect::default(),
            tree_offset: 0,
            logs_area: Rect::default(),
            problems_area: Rect::default(),
            selection: None,
            env: EnvTab::default(),
            tab_spans: Vec::new(),
            jump: None,
            start_at: None,
            mouse: true,
            quit: false,
            detach_keys: DetachKeys::default(),
            typing: None,
            theme: Theme::default(),
            colors: Colors::Truecolor,
            picked_theme: false,
            picker: None,
            typing_sessions: 0,
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
                let contexts: BTreeMap<&str, &str> = snapshot
                    .projects
                    .iter()
                    .filter_map(|p| Some((p.name.as_str(), p.kube.as_ref()?.context.as_str())))
                    .collect();
                let before = self.daemon_snapshot.as_ref();
                self.browse.retain(|project, _| {
                    let context = contexts.get(project.as_str());
                    let previous = before
                        .and_then(|s| s.projects.iter().find(|p| p.name == *project))
                        .and_then(|p| p.kube.as_ref())
                        .map(|k| k.context.as_str());
                    context.is_some() && (previous.is_none() || previous == context.copied())
                });
                self.daemon_snapshot = Some(*snapshot);
                self.snapshot_at = now;
                self.rebuild();
            }
            Event::Disconnected(message) => {
                self.connection = Connection::Lost(message);
                self.daemon_snapshot = None;
                self.browse.clear();
                self.rebuild();
            }
            Event::Services { project, result } => {
                let browse = match result {
                    Ok(services) => Browse::Loaded(services),
                    Err(message) => Browse::Failed(message),
                };
                self.browse.insert(project, browse);
                self.rebuild();
            }
            Event::Log { generation, line } => self.logs.push(generation, line),
            Event::LogsEnded { generation, error } => self.logs.end(generation, error),
            Event::Request {
                generation,
                request,
            } => self.requests.push(generation, *request),
            Event::RequestsEnded { generation, error } => self.requests.end(generation, error),
            Event::ControlDone { verb, result } => self.control_done(verb, result),
            Event::Shared(result) => self.shared(result),
            Event::Unshared(Ok(apps)) => {
                self.inform(&format!("no longer shared: {}", apps.join(", ")))
            }
            Event::Unshared(Err(message)) => self.warn(&message),
            Event::Env { generation, result } => {
                self.env.in_flight = false;
                if generation == self.env.generation {
                    let result = result.map_err(|message| {
                        message.replace("choose one with --process", "select one in the tree")
                    });
                    self.env.env = Some(result);
                }
            }
            Event::Attached { session, result } if self.typing_to(session) => match result {
                Ok(attached) => {
                    if let Some(typing) = &mut self.typing {
                        typing.label = format!("{}/{}", attached.instance, attached.process);
                        typing.attached = true;
                    }
                }
                Err(message) => {
                    self.stop_typing();
                    self.warn(&message);
                }
            },
            Event::TypingOutput { session, data } if self.typing_to(session) => {
                if let Some(typing) = &mut self.typing {
                    typing.stripper.feed(&data);
                }
            }
            Event::TypingEnded { session, message } if self.typing_to(session) => {
                if let Some(typing) = self.stop_typing() {
                    let message = message.unwrap_or_else(|| "lost the connection".to_string());
                    self.warn(&format!("{}: {message}", typing.label));
                }
            }
            Event::Attached { .. } | Event::TypingOutput { .. } | Event::TypingEnded { .. } => {}
        }
        if let Some(notice) = &self.notice {
            let lasts = Duration::from_secs(if notice.error { 10 } else { 5 });
            if now.duration_since(notice.at) > lasts {
                self.notice = None;
            }
        }
        if self.zoom.is_some() {
            self.focus = Focus::Logs;
        }
        commands.extend(self.sync_logs());
        commands.extend(self.sync_env());
        commands.extend(self.sync_requests());
        commands
    }

    pub fn selected_node(&self) -> Option<&Node> {
        let row = self.rows.get(self.cursor)?;
        Some(tree::node(&self.roots, &row.path))
    }

    /// The kube project named `project`, when it is one.
    pub fn kube_project(&self, project: &str) -> Option<&KubeProjectInfo> {
        self.snapshot
            .as_ref()?
            .projects
            .iter()
            .find(|p| p.name == project)?
            .kube
            .as_ref()
    }

    /// Warns when the selection is in a kube project, where `what` doesn't apply.
    fn refuse_kube(&mut self, what: &str) -> bool {
        let kube = self
            .selected_node()
            .is_some_and(|node| self.kube_project(node.id().project()).is_some());
        if kube {
            self.warn(&format!("{what} isn't available for kube projects"));
        }
        kube
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
        self.snapshot = self.daemon_snapshot.clone().map(|mut snapshot| {
            browse::merge(&mut snapshot, &self.browse);
            snapshot
        });
        self.roots = self.snapshot.as_ref().map(tree::build).unwrap_or_default();
        match self.start_at.take() {
            Some(id) if self.snapshot.is_some() => self.reveal(id),
            start_at => {
                self.start_at = start_at;
                self.refresh_rows();
            }
        }
    }

    /// Selects a node, unfolding everything above it.
    fn reveal(&mut self, id: NodeId) {
        if let Some(path) = tree::path_to(&self.roots, &id) {
            for depth in 1..path.len() {
                let ancestor = tree::node(&self.roots, &path[..depth]).id().clone();
                self.fold.expanded.insert(ancestor, true);
            }
            self.selected = Some(id.clone());
        }
        self.refresh_rows();
        if !self.selected_node().is_some_and(|n| n.ids.contains(&id)) {
            self.fold.filter.clear();
            self.fold.show = Show::All;
            self.selected = Some(id);
            self.refresh_rows();
        }
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
    /// tree row, or folds and unfolds the row that is already selected. Clicking the
    /// problem count lists the problems. Dragging over the logs selects lines and
    /// releasing copies them.
    fn mouse(&mut self, event: MouseEvent, commands: &mut Vec<Command>) {
        let at = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.logs_area.contains(at) => {
                self.end_selection();
                if at.y == self.logs_area.y
                    && let Some(&(tab, ..)) = self
                        .tab_spans
                        .iter()
                        .find(|(_, start, end)| (*start..*end).contains(&at.x))
                {
                    self.pick_tab(tab);
                    return;
                }
                self.focus = Focus::Logs;
                let top = self.logs_area.y + 1;
                let lines = match self.tab {
                    Tab::Logs | Tab::Events => true,
                    Tab::Requests => {
                        let listed = self.requests.open.is_none() || self.zoom.is_some();
                        let bottom = top + self.requests.height as u16;
                        if listed
                            && (top..bottom).contains(&at.y)
                            && !self.requests.detail_area.contains(at)
                        {
                            let row = usize::from(at.y - top);
                            self.requests.select_index(self.requests.offset + row);
                        }
                        false
                    }
                    Tab::Env => false,
                };
                if lines
                    && at.y >= top
                    && usize::from(at.y - top) < self.logs.height
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
                    match self.tab {
                        Tab::Logs | Tab::Events => self.logs.scroll_by(if up { 3 } else { -3 }),
                        Tab::Env => self.env.scroll_by(if up { -3 } else { 3 }),
                        Tab::Requests if self.requests.detail_area.contains(at) => {
                            let requests = &mut self.requests;
                            let delta = if up { -3 } else { 3 };
                            requests.detail_scroll =
                                requests.detail_scroll.saturating_add_signed(delta);
                        }
                        Tab::Requests => {
                            self.tab_key(Intent::Move(if up { -3 } else { 3 }));
                        }
                    }
                } else if self.tree_area.contains(at) {
                    self.move_by(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.problems_area.contains(at) {
                    self.mode = Mode::Problems;
                } else if self.logs_area.contains(at) {
                    self.focus = Focus::Logs;
                } else if self.tree_area.contains(at) {
                    self.end_selection();
                    self.focus = Focus::Tree;
                    let first = self.tree_area.y + 2;
                    if at.y < first || usize::from(at.y - first) >= self.tree_height {
                        return;
                    }
                    let index = self.tree_offset + usize::from(at.y - first);
                    if index == self.cursor {
                        self.apply(Intent::Toggle, commands);
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
        if self.mode == Mode::Jump {
            return self.jump_key(intent);
        }
        if self.mode == Mode::Themes {
            return self.theme_key(intent);
        }
        if self.mode == Mode::Type {
            if let Intent::Typed(key) = intent {
                commands.extend(self.type_key(key));
            }
            return;
        }
        if self.focus == Focus::Logs && self.tab_key(intent) {
            return;
        }
        match (intent, self.focus) {
            (Intent::Quit, _) => self.quit = true,
            (Intent::Help, _) => self.mode = Mode::Help,
            (Intent::Problems, _) => self.mode = Mode::Problems,
            (Intent::Close, _) => self.mode = Mode::Normal,
            (Intent::SwitchFocus, _) if self.zoom.is_some() => {
                self.zoom = None;
                self.focus = Focus::Tree;
            }
            (Intent::SwitchFocus, _) => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Logs,
                    Focus::Logs => Focus::Tree,
                }
            }
            (Intent::PickTab(number), _) => {
                if let Some(tab) = Tab::from_number(number) {
                    self.pick_tab(tab);
                }
            }
            (Intent::StepTab(delta), _) => self.pick_tab(self.tab.step(delta)),
            (Intent::Move(delta), Focus::Tree) => self.move_by(delta),
            (Intent::OpenRow, _) => {}
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
            (Intent::Expand, _) => commands.extend(self.expand()),
            (Intent::Toggle, _) => {
                let kube = self.selected_kube_project();
                if let Some(row) = self.rows.get(self.cursor) {
                    if kube.is_some() && !row.expanded {
                        commands.extend(self.expand());
                    } else if row.expandable {
                        let expanded = !row.expanded;
                        self.set_expanded(expanded);
                    }
                }
            }
            (Intent::StartFilter, _) => self.mode = Mode::Filter,
            (Intent::FilterDone, _) => self.mode = Mode::Normal,
            (Intent::ClearFilter, _)
                if self.zoom.is_some()
                    && self.mode == Mode::Normal
                    && self.logs.search.is_empty() =>
            {
                self.unzoom();
            }
            (Intent::ClearFilter, _) => {
                self.mode = Mode::Normal;
                if self.focus == Focus::Logs {
                    self.logs.filter = false;
                }
                self.edit_filter(|filter| filter.clear());
            }
            (Intent::ToggleLogFilter, _) => self.logs.toggle_filter(),
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
            (Intent::ToggleZoom, _) if self.zoom.is_some() => self.unzoom(),
            (Intent::ToggleZoom, _) => {
                self.zoom = Some(self.focus);
                self.focus = Focus::Logs;
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
            (Intent::OpenPublic, _) => commands.extend(self.public_url("open").map(Command::Open)),
            (Intent::Mark, _) => commands.extend(self.mark()),
            (Intent::CopyUrl, _) => commands.extend(self.copy_url()),
            (Intent::FindMatch { older }, _) => {
                if self.logs.search.is_empty() {
                    self.inform("/ searches the logs");
                } else if !self.logs.find(older) {
                    self.warn(&format!("no lines match {:?}", self.logs.search));
                }
            }
            (Intent::PickTheme, _) => self.pick_theme(),
            (Intent::StartJump, _) => {
                self.mode = Mode::Jump;
                self.jump = Some(Jump::default());
            }
            (Intent::CopyPublicUrl, _) => commands.extend(self.public_url("copy").map(|url| {
                self.inform(&format!("copied {url}"));
                Command::Copy(url)
            })),
            (Intent::Share { auth }, _) => commands.extend(self.share(auth)),
            (Intent::StartTyping, _) => commands.extend(self.start_typing()),
            (Intent::Typed(_), _) => {}
        }
    }

    fn unzoom(&mut self) {
        if let Some(focus) = self.zoom.take() {
            self.focus = focus;
        }
    }

    /// Connects to the selected process, or the selected app's HTTP process, and sends
    /// it every key until the detach keys.
    fn start_typing(&mut self) -> Option<Command> {
        if self.refuse_kube("typing into a process") {
            return None;
        }
        let id = self.selected_node()?.id().clone();
        if !matches!(id, NodeId::Instance { .. } | NodeId::Process { .. }) {
            self.warn("select an app or a process to type into");
            return None;
        }
        self.mode = Mode::Type;
        self.typing_sessions += 1;
        let session = self.typing_sessions;
        self.typing = Some(Typing {
            session,
            label: target::label(&id),
            attached: false,
            filter: self.detach_keys.filter(),
            stripper: Stripper::default(),
        });
        Some(Command::Attach {
            session,
            request: AttachRequest {
                target: target::address(&id),
                process: None,
                size: None,
                replay: true,
            },
        })
    }

    /// Typing is under way in `session`, not an earlier one.
    fn typing_to(&self, session: u64) -> bool {
        self.typing.as_ref().is_some_and(|t| t.session == session)
    }

    fn type_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(typing) = &mut self.typing else {
            self.mode = Mode::Normal;
            return Vec::new();
        };
        let (bytes, detach) = typing.filter.feed(&input::key_bytes(key));
        let mut commands = Vec::new();
        if !bytes.is_empty() {
            commands.push(Command::Type(bytes));
        }
        if detach && let Some(typing) = self.stop_typing() {
            self.inform(&format!("stopped typing into {}", typing.label));
            commands.push(Command::Detach);
        }
        commands
    }

    fn stop_typing(&mut self) -> Option<Typing> {
        if self.mode == Mode::Type {
            self.mode = Mode::Normal;
        }
        self.typing.take()
    }

    /// The theme as it draws on this terminal: text roles left at `Reset` take
    /// `fg`, and truecolor becomes 256 colours where the terminal lacks it.
    fn drawable(&self, theme: Theme) -> Theme {
        let theme = theme.inherit();
        match self.colors {
            Colors::Truecolor => theme,
            Colors::Indexed => theme.indexed(),
        }
    }

    pub fn use_theme(&mut self, theme: Theme) {
        self.theme = self.drawable(theme);
    }

    fn pick_theme(&mut self) {
        let themes: Vec<Theme> = theme::builtins(self.colors)
            .into_iter()
            .map(|t| self.drawable(t))
            .collect();
        let cursor = themes
            .iter()
            .position(|t| t.id == self.theme.id)
            .unwrap_or(0);
        self.picker = Some(ThemePicker {
            themes,
            cursor,
            previous: self.theme.clone(),
        });
        self.mode = Mode::Themes;
    }

    fn theme_key(&mut self, intent: Intent) {
        let Some(picker) = &mut self.picker else {
            self.mode = Mode::Normal;
            return;
        };
        match intent {
            Intent::Move(delta) => {
                picker.cursor = picker
                    .cursor
                    .saturating_add_signed(delta)
                    .min(picker.themes.len() - 1);
                self.theme = picker.themes[picker.cursor].clone();
            }
            Intent::Yes => {
                self.mode = Mode::Normal;
                self.picker = None;
                self.picked_theme = true;
                let label = self.theme.label.clone();
                self.inform(&format!("theme: {label}"));
            }
            Intent::Quit => {
                self.theme = picker.previous.clone();
                self.picker = None;
                self.quit = true;
            }
            _ => {
                self.theme = picker.previous.clone();
                self.mode = Mode::Normal;
                self.picker = None;
            }
        }
    }

    fn jump_key(&mut self, intent: Intent) {
        let Some(prompt) = &mut self.jump else {
            self.mode = Mode::Normal;
            return;
        };
        match intent {
            Intent::FilterChar(c) => {
                prompt.query.push(c);
                prompt.cursor = 0;
            }
            Intent::FilterBackspace => {
                prompt.query.pop();
                prompt.cursor = 0;
            }
            Intent::Move(delta) => prompt.cursor = prompt.cursor.saturating_add_signed(delta),
            Intent::FilterDone => {
                let all = jump::candidates(&self.roots);
                let found = jump::matches(&all, &prompt.query);
                let picked = found
                    .get(prompt.cursor.min(found.len().saturating_sub(1)))
                    .map(|c| c.id.clone());
                self.mode = Mode::Normal;
                self.jump = None;
                if let Some(id) = picked {
                    self.focus = Focus::Tree;
                    self.reveal(id);
                }
            }
            _ => {
                self.mode = Mode::Normal;
                self.jump = None;
            }
        }
    }

    /// Keys the tabs other than logs and events handle themselves, with the tabs pane
    /// focused; false leaves the key to the rest.
    fn tab_key(&mut self, intent: Intent) -> bool {
        let page = |height: usize| height.saturating_sub(1).max(1) as isize;
        match self.tab {
            Tab::Logs | Tab::Events => return false,
            Tab::Env => {
                let view = &mut self.env;
                match intent {
                    Intent::Move(delta) => view.scroll_by(delta),
                    Intent::PageDown => view.scroll_by(page(view.height)),
                    Intent::PageUp => view.scroll_by(-page(view.height)),
                    Intent::Top => view.scroll = 0,
                    Intent::Bottom => view.scroll = usize::MAX,
                    Intent::StartFilter => self.inform("/ searches the logs"),
                    Intent::FindMatch { .. } | Intent::OpenRow | Intent::ToggleLogFilter => {}
                    _ => return false,
                }
            }
            Tab::Requests => {
                let zoomed = self.zoom.is_some();
                let requests = &mut self.requests;
                let open = requests.open.is_some() && !zoomed;
                if zoomed && matches!(intent, Intent::ClearFilter) {
                    return false;
                }
                let scroll = |requests: &mut Requests, delta: isize| {
                    requests.detail_scroll = requests.detail_scroll.saturating_add_signed(delta);
                };
                match intent {
                    Intent::Move(delta) if open => scroll(requests, delta),
                    Intent::PageDown if open => scroll(requests, page(requests.height)),
                    Intent::PageUp if open => scroll(requests, -page(requests.height)),
                    Intent::Top if open => requests.detail_scroll = 0,
                    Intent::Bottom if open => requests.detail_scroll = usize::MAX,
                    Intent::ClearFilter if open => requests.close(),
                    Intent::Move(delta) => requests.select_by(delta),
                    Intent::PageDown => requests.select_by(page(requests.height)),
                    Intent::PageUp => requests.select_by(-page(requests.height)),
                    Intent::Top => requests.select_index(0),
                    Intent::Bottom => requests.follow(),
                    Intent::OpenRow => requests.open_selected(),
                    Intent::StartFilter => self.inform("/ searches the logs"),
                    Intent::FindMatch { .. } | Intent::ClearFilter | Intent::ToggleLogFilter => {}
                    _ => return false,
                }
                if zoomed {
                    self.requests.close();
                }
            }
        }
        true
    }

    /// Shows `tab`; picking the env tab again fetches the env again.
    fn pick_tab(&mut self, tab: Tab) {
        if tab == self.tab {
            match tab {
                Tab::Env => self.env.subject = EnvSubject::None,
                Tab::Logs | Tab::Events | Tab::Requests => {}
            }
        }
        self.tab = tab;
    }

    /// The env tab's subject: the selected process, or the process of the selected app
    /// that serves HTTP, or its only one.
    fn env_subject(&self) -> EnvSubject {
        match self.tab {
            Tab::Env => {}
            Tab::Logs | Tab::Events | Tab::Requests => return EnvSubject::None,
        }
        let unavailable = |text: &str| EnvSubject::Unavailable(text.to_string());
        let Some(id) = self.selected_node().map(Node::id) else {
            return unavailable("Select an app or a process to see its env.");
        };
        if self.kube_project(id.project()).is_some() {
            return unavailable("Env isn't available for kube projects.");
        }
        let NodeId::Instance {
            project,
            stack,
            app,
        } = id
        else {
            return match id {
                NodeId::Process { .. } => EnvSubject::Fetch {
                    id: id.clone(),
                    note: None,
                },
                _ => unavailable("Select an app or a process to see its env."),
            };
        };
        let processes = self.instance(id).map_or(&[][..], |i| &i.processes[..]);
        let process = |name: &str| NodeId::Process {
            project: project.clone(),
            stack: stack.clone(),
            app: app.clone(),
            process: name.to_string(),
        };
        let names = || {
            processes
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match processes {
            [] => EnvSubject::Fetch {
                id: id.clone(),
                note: None,
            },
            [only] => EnvSubject::Fetch {
                id: process(&only.name),
                note: None,
            },
            _ => match processes.iter().find(|p| p.http) {
                Some(web) => EnvSubject::Fetch {
                    id: process(&web.name),
                    note: Some(format!(
                        "The env of {}, which serves HTTP. Select a process to see another: {}.",
                        web.name,
                        names()
                    )),
                },
                None => EnvSubject::Unavailable(format!(
                    "Select a process to see its env: {}.",
                    names()
                )),
            },
        }
    }

    /// Fetches the env tab's subject once it changed, one fetch at a time: rendering
    /// an env runs the project's env tools in the daemon. A subject that changes while
    /// a fetch runs is fetched when it returns.
    fn sync_env(&mut self) -> Option<Command> {
        let subject = self.env_subject();
        if subject != self.env.subject {
            self.env.restart(subject);
        }
        if self.env.in_flight || self.env.env.is_some() {
            return None;
        }
        let EnvSubject::Fetch { id, .. } = &self.env.subject else {
            return None;
        };
        let target = target::address(id);
        self.env.in_flight = true;
        Some(Command::FetchEnv {
            generation: self.env.generation,
            target,
        })
    }

    fn edit_filter(&mut self, edit: impl FnOnce(&mut String)) {
        match self.focus {
            Focus::Tree => {
                edit(&mut self.fold.filter);
                self.refresh_rows();
            }
            Focus::Logs => {
                let mut search = self.logs.search.clone();
                edit(&mut search);
                self.logs.set_search(search);
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

    /// Opens the selected row, or moves into it when it's open. Opening a kube
    /// project lists its Services.
    fn expand(&mut self) -> Option<Command> {
        let row = self.rows.get(self.cursor)?;
        if row.expanded {
            self.move_by(1);
            return None;
        }
        if let Some(project) = self.selected_kube_project() {
            let shown = match self.browse.remove(&project) {
                Some(Browse::Loaded(services) | Browse::Loading(Some(services))) => Some(services),
                _ => None,
            };
            self.browse.insert(project.clone(), Browse::Loading(shown));
            self.set_expanded(true);
            return Some(Command::FetchServices(project));
        }
        if row.expandable {
            self.set_expanded(true);
        }
        None
    }

    /// The kube project whose row is selected.
    fn selected_kube_project(&self) -> Option<String> {
        let node = self.selected_node()?;
        let NodeId::Project { project } = &node.ids[0] else {
            return None;
        };
        self.kube_project(project).map(|_| project.clone())
    }

    fn control(&mut self, control: Control) -> Option<Command> {
        if control == Control::Save {
            return self.save();
        }
        if matches!(control, Control::TogglePin | Control::ToggleDisable)
            && self.refuse_kube(match control {
                Control::TogglePin => "pinning",
                _ => "disabling",
            })
        {
            return None;
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
                wait: false,
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
                wait: false,
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

    fn mark(&mut self) -> Option<Command> {
        let id = self.selected_node()?.id().clone();
        if matches!(id, NodeId::Folder { .. }) {
            self.warn("select a project to mark its logs");
            return None;
        }
        Some(Command::Mark(target::address(&id)))
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

    /// The selected app's public URL, or a notice why there is none.
    fn public_url(&mut self, verb: &str) -> Option<String> {
        match self.selected_instance() {
            Some((_, instance)) => match instance.public_url.clone() {
                Some(url) => Some(url),
                None => {
                    self.warn("not shared; w shares it");
                    None
                }
            },
            None => {
                self.warn(&format!("select an app to {verb}"));
                None
            }
        }
    }

    fn copy_url(&mut self) -> Option<Command> {
        match self.selected_instance() {
            Some((_, instance)) => {
                let url = instance.url.clone();
                self.inform(&format!("copied {url}"));
                Some(Command::Copy(url))
            }
            None => {
                self.warn("select an app to copy");
                None
            }
        }
    }

    /// Shares the selected app, or ends its stack's share. With `auth` it shares, or
    /// adds a password to the share. When the provider carries one app at a time and
    /// another app has it, asks before taking its place.
    fn share(&mut self, auth: bool) -> Option<Command> {
        if self.refuse_kube("sharing") {
            return None;
        }
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

    /// A kube project or namespace is selected, whose log pane shows its events since
    /// its Services' logs come from separate pods.
    fn follows_kube_events(&self) -> bool {
        self.selected_node().is_some_and(|n| {
            self.kube_project(n.id().project()).is_some()
                && !matches!(n.id(), NodeId::Instance { .. })
        })
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
        let node = self.selected_node();
        let events = match self.tab {
            Tab::Events => true,
            Tab::Logs | Tab::Env | Tab::Requests => false,
        };
        if events || self.follows_kube_events() {
            return Some(Command::FollowEvents {
                generation,
                request: EventsRequest {
                    target: node
                        .map(Node::id)
                        .filter(|id| !matches!(id, NodeId::Folder { .. }))
                        .map(target::address),
                    follow: true,
                    tail: Some(1000),
                },
            });
        }
        let Some(node) = node else {
            return Some(Command::StopLogs);
        };
        Some(Command::FollowLogs {
            generation,
            request: LogsRequest {
                target: target::address(node.id()),
                follow: true,
                tail: Some(1000),
                search: None,
                ..Default::default()
            },
        })
    }

    /// What the log pane follows: an address's logs, or the events about it (every
    /// event for a folder or an empty tree).
    fn log_key(&self) -> Option<String> {
        self.snapshot.as_ref()?;
        let id = self.selected_node().map(Node::id);
        match self.tab {
            Tab::Env | Tab::Requests => None,
            Tab::Events => Some(match id {
                Some(id) if !matches!(id, NodeId::Folder { .. }) => {
                    format!("events {}", target::label(id))
                }
                _ => "events".to_string(),
            }),
            Tab::Logs => {
                let id = id?;
                if matches!(id, NodeId::Folder { .. }) {
                    return None;
                }
                Some(target::label(id))
            }
        }
    }

    /// Starts a new requests stream when the selection changed: the requests for it
    /// and everything below, or every request for a folder or an empty tree.
    ///
    /// Moving through the tree waits for the selection to settle before it starts the
    /// next stream, since each start sends the scope's recent requests again.
    fn sync_requests(&mut self) -> Vec<Command> {
        let target = self
            .selected_node()
            .map(Node::id)
            .filter(|id| !matches!(id, NodeId::Folder { .. }))
            .cloned();
        let wanted = match self.tab {
            Tab::Requests if self.snapshot.is_some() => Some(match &target {
                Some(id) => format!("requests {}", target::label(id)),
                None => "requests".to_string(),
            }),
            Tab::Requests | Tab::Logs | Tab::Events | Tab::Env => None,
        };
        let mut commands = Vec::new();
        if wanted != self.requests.key {
            let moved = self.requests.key.is_some();
            self.requests.restart(wanted.clone());
            if moved {
                commands.push(Command::StopRequests);
            }
            if wanted.is_none() {
                self.requests.pending = None;
                return commands;
            }
            let settle = Duration::from_millis(150);
            let at = if moved { self.now + settle } else { self.now };
            self.requests.pending = Some(at);
            if moved {
                commands.push(Command::Wake(settle));
            }
        }
        match self.requests.pending {
            Some(at) if at <= self.now => {
                self.requests.pending = None;
                commands.push(Command::FollowRequests {
                    generation: self.requests.generation,
                    request: RequestsRequest {
                        target: target.as_ref().map(target::address),
                        follow: true,
                        tail: Some(200),
                    },
                });
            }
            _ => {}
        }
        commands
    }

    fn log_prefix(&self) -> Prefix {
        let events = match self.tab {
            Tab::Events => true,
            Tab::Logs | Tab::Env | Tab::Requests => false,
        };
        if events || self.follows_kube_events() {
            return Prefix::None;
        }
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
    fn number_keys_switch_tabs_and_leave_the_focus() {
        let (mut app, now) = connected();
        let commands = press(&mut app, now, KeyCode::Char('2'));
        let [Command::FollowEvents { request, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(
            request.target.as_ref().and_then(|t| t.project.clone()),
            Some("nova".into())
        );
        assert!(request.follow);
        assert_eq!(app.logs.prefix, Prefix::None);

        let commands = press(&mut app, now, KeyCode::Down);
        let [Command::FollowEvents { request, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(
            request.target.as_ref().map(|t| t.address.as_str()),
            Some("@main"),
            "events follow the selection"
        );

        let commands = press(&mut app, now, KeyCode::Char('1'));
        assert_eq!(follows(&commands), ["@main"]);
        assert_eq!(app.tab, Tab::Logs);

        assert!(
            press(&mut app, now, KeyCode::Char('4')).is_empty(),
            "no tab 4 yet"
        );
        assert_eq!(app.tab, Tab::Logs);
        press(&mut app, now, KeyCode::Char(']'));
        assert_eq!(app.tab, Tab::Events);
        press(&mut app, now, KeyCode::Char('['));
        let commands = press(&mut app, now, KeyCode::Char('['));
        assert_eq!(app.tab, Tab::Requests, "[ goes around to the last tab");
        let [Command::StopLogs, Command::FollowRequests { request, .. }] = commands.as_slice()
        else {
            panic!("{commands:?}");
        };
        assert_eq!(
            request.target.as_ref().map(|t| t.address.as_str()),
            Some("@main")
        );
        let commands = press(&mut app, now, KeyCode::Char('['));
        assert_eq!(app.tab, Tab::Env);
        assert_eq!(commands, [Command::StopRequests], "a stack has no env");
        assert_eq!(app.focus, Focus::Tree, "only tab moves the focus");
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn the_requests_tab_follows_the_selection_and_opens_a_request() {
        use crate::tui::requests::tests::request;

        let (mut app, now) = connected();
        let commands = press(&mut app, now, KeyCode::Char('5'));
        let [
            Command::StopLogs,
            Command::FollowRequests { request: asked, .. },
        ] = commands.as_slice()
        else {
            panic!("{commands:?}");
        };
        let target = asked.target.as_ref().expect("scoped to the project");
        assert_eq!(
            (target.address.as_str(), target.project.as_deref()),
            ("", Some("nova"))
        );
        assert!(asked.follow);

        let commands = press(&mut app, now, KeyCode::Down);
        assert_eq!(
            commands,
            [
                Command::StopRequests,
                Command::Wake(Duration::from_millis(150))
            ],
            "a move waits for the selection to settle"
        );
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Up);
        let later = |ms| now + Duration::from_millis(ms);
        assert!(app.update(Event::Tick, later(100)).is_empty());
        let commands = app.update(Event::Tick, later(200));
        let [
            Command::FollowRequests {
                generation,
                request: asked,
            },
        ] = commands.as_slice()
        else {
            panic!("{commands:?}");
        };
        assert_eq!(
            asked.target.as_ref().map(|t| t.address.as_str()),
            Some("@main"),
            "requests follow the settled selection"
        );
        assert_eq!(asked.tail, Some(200));
        let generation = *generation;
        for (seq, path) in [(1, "/"), (2, "/login")] {
            app.update(
                Event::Request {
                    generation,
                    request: Box::new(request(seq, path, Some(200))),
                },
                now,
            );
        }
        app.update(
            Event::Request {
                generation: generation - 1,
                request: Box::new(request(3, "/stale", Some(200))),
            },
            now,
        );
        assert_eq!(
            app.requests.rows().len(),
            2,
            "an earlier scope's request is dropped"
        );

        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Enter);
        assert_eq!(
            app.requests.opened().map(|r| r.path.as_str()),
            Some("/login")
        );
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.requests.open, None, "esc goes back to the list");
        assert_eq!(app.tab, Tab::Requests);
        assert_eq!(app.focus, Focus::Logs);
        press(&mut app, now, KeyCode::Up);
        press(&mut app, now, KeyCode::Up);
        press(&mut app, now, KeyCode::Enter);
        assert_eq!(app.requests.opened().map(|r| r.path.as_str()), Some("/"));
        press(&mut app, now, KeyCode::Esc);
        press(&mut app, now, KeyCode::Char('G'));
        assert_eq!(app.requests.selected, None, "G follows again");
        app.logs_area = Rect::new(40, 0, 40, 3);
        app.requests.height = 1;
        app.update(mouse(MouseEventKind::Down(MouseButton::Left), 50, 2), now);
        assert_eq!(app.requests.selected, None, "the bottom border isn't a row");
        app.update(mouse(MouseEventKind::Down(MouseButton::Left), 50, 1), now);
        assert_eq!(app.requests.selected, Some(1), "a click selects a request");

        let commands = press(&mut app, now, KeyCode::Char('1'));
        assert_eq!(follows(&commands), ["@main"]);
        assert!(commands.contains(&Command::StopRequests), "{commands:?}");
    }

    #[test]
    fn moving_follows_the_new_selection() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        let commands = press(&mut app, now, KeyCode::Down);
        assert_eq!(follows(&commands), ["nova-billing@main"]);
        assert_eq!(app.logs.prefix, Prefix::Process);
        assert!(press(&mut app, now, KeyCode::Char('?')).is_empty());
    }

    #[test]
    fn kube_services_refuse_what_doesnt_apply_and_follow_their_logs() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(
            Event::Snapshot(Box::new(crate::tui::fixtures::kube_sample())),
            now,
        );
        let mut followed = press(&mut app, now, KeyCode::Right);
        followed.extend(press(&mut app, now, KeyCode::Down));
        assert!(
            matches!(followed.last(), Some(Command::FollowEvents { .. })),
            "a namespace follows its events: {followed:?}"
        );
        let followed = press(&mut app, now, KeyCode::Down);
        let [Command::FollowLogs { request, .. }] = followed.as_slice() else {
            panic!("a Service follows its pod's logs: {followed:?}");
        };
        assert_eq!(request.target.address, "vlogs@logging");
        assert_eq!(request.target.project.as_deref(), Some("prod"));
        for (key, what) in [('w', "sharing"), ('p', "pinning"), ('d', "disabling")] {
            assert!(press(&mut app, now, KeyCode::Char(key)).is_empty());
            assert_eq!(
                app.notice.as_ref().map(|n| n.text.clone()),
                Some(format!("{what} isn't available for kube projects"))
            );
        }
        let commands = press(&mut app, now, KeyCode::Char('r'));
        assert!(
            matches!(commands.as_slice(), [Command::Control { .. }]),
            "{commands:?}"
        );
    }

    #[test]
    fn controls_address_the_selection() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
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
    fn a_marker_goes_to_the_selection() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        let commands = press(&mut app, now, KeyCode::Char('-'));
        let [Command::Mark(target)] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        assert_eq!(target.address, "nova-billing@main");
    }

    #[test]
    fn saving_writes_the_temporary_pin() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
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
    fn clicking_a_kube_project_lists_its_services() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(
            Event::Snapshot(Box::new(crate::tui::fixtures::kube_sample())),
            now,
        );
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.tree_height = 17;
        let commands = app.update(mouse(MouseEventKind::Down(MouseButton::Left), 5, 2), now);
        assert!(
            commands.contains(&Command::FetchServices("prod".into())),
            "{commands:?}"
        );
    }

    #[test]
    fn listings_go_when_the_daemon_or_the_cluster_changes() {
        let now = Instant::now();
        let mut app = App::new(now);
        let snapshot = crate::tui::fixtures::kube_sample();
        app.update(Event::Snapshot(Box::new(snapshot.clone())), now);
        let listed = |app: &mut App| {
            app.update(
                Event::Services {
                    project: "prod".into(),
                    result: Ok(vec![KubeServiceEntry {
                        namespace: "billing".into(),
                        name: "api".into(),
                        url: "https://api.billing.prod.test".into(),
                    }]),
                },
                now,
            );
        };
        listed(&mut app);
        app.update(Event::Snapshot(Box::new(snapshot.clone())), now);
        assert!(app.browse.contains_key("prod"), "kept across snapshots");

        let mut moved = snapshot.clone();
        moved.projects[0].kube.as_mut().unwrap().context = "other".into();
        app.update(Event::Snapshot(Box::new(moved)), now);
        assert!(app.browse.is_empty(), "another cluster");

        listed(&mut app);
        app.update(Event::Disconnected("gone".into()), now);
        assert!(app.browse.is_empty());

        app.update(Event::Snapshot(Box::new(snapshot.clone())), now);
        listed(&mut app);
        let mut removed = snapshot;
        removed.projects.clear();
        app.update(Event::Snapshot(Box::new(removed)), now);
        assert!(app.browse.is_empty(), "the project is gone");
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let (mut app, now) = connected();
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.tree_height = 17;
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
                    marker: false,
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
    fn the_wheel_scrolls_the_env_tab() {
        let (mut app, now) = connected();
        app.logs_area = Rect::new(40, 0, 40, 20);
        press(&mut app, now, KeyCode::Char('3'));
        app.update(mouse(MouseEventKind::ScrollDown, 50, 5), now);
        assert_eq!(app.env.scroll, 3);
        app.update(mouse(MouseEventKind::ScrollUp, 50, 5), now);
        assert_eq!(app.env.scroll, 0);
        assert_eq!(app.logs.scroll, 0, "the logs stay put");
    }

    #[test]
    fn dragging_over_the_logs_copies_whole_lines() {
        let (mut app, now) = connected();
        app.tree_area = Rect::new(0, 0, 40, 20);
        app.tree_height = 17;
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
                    marker: false,
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
        app.tree_height = 17;
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

        app.tree_height = 3;
        app.update(click(5, 5), now);
        assert_eq!(app.cursor, 2, "rows under the filter box aren't clickable");
        app.update(click(5, 3), now);
        assert_eq!(app.cursor, 1);
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
        press(&mut app, now, KeyCode::Right);
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
        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Down] {
            press(&mut app, now, code);
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
    fn y_copies_the_address_and_shift_y_the_shared_one() {
        let now = Instant::now();
        let mut app = App::new(now);
        app.update(Event::Snapshot(Box::new(shared_sample())), now);
        assert!(press(&mut app, now, KeyCode::Char('y')).is_empty());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("select an app to copy")
        );

        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        assert_eq!(
            press(&mut app, now, KeyCode::Char('y')),
            [Command::Copy("https://nova-billing.test".into())]
        );
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("copied https://nova-billing.test")
        );
        assert_eq!(
            press(&mut app, now, KeyCode::Char('Y')),
            [Command::Copy(
                "https://nova-billing--main--nova.demo.dev".into()
            )]
        );

        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        assert_eq!(
            press(&mut app, now, KeyCode::Char('y')),
            [Command::Copy("https://nova-billing.test".into())],
            "a process copies its app's address"
        );

        for code in [KeyCode::End, KeyCode::Right, KeyCode::End] {
            press(&mut app, now, code);
        }
        assert!(press(&mut app, now, KeyCode::Char('Y')).is_empty());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("not shared; w shares it")
        );
    }

    #[test]
    fn t_types_into_the_selected_app_until_the_detach_keys() {
        let (mut app, now) = connected();
        assert!(press(&mut app, now, KeyCode::Char('t')).is_empty());
        assert_eq!(app.mode, Mode::Normal, "a project has no process");

        for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
            press(&mut app, now, code);
        }
        let commands = press(&mut app, now, KeyCode::Char('t'));
        let [Command::Attach { session, request }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        let session = *session;
        assert_eq!(request.target.address, "nova-billing@main");
        assert_eq!(request.process, None);
        assert_eq!(app.mode, Mode::Type);
        app.update(
            Event::Attached {
                session,
                result: Ok(Attached {
                    instance: "nova-billing@main".into(),
                    process: "web".into(),
                }),
            },
            now,
        );
        app.update(
            Event::TypingOutput {
                session,
                data: b"=> 1\r\n(rdbg) ".to_vec(),
            },
            now,
        );
        assert_eq!(app.typing.as_ref().unwrap().pending(), "(rdbg) ");

        assert_eq!(
            press(&mut app, now, KeyCode::Char('q')),
            [Command::Type(b"q".to_vec())],
            "letters are typed, not commands"
        );
        let ctrl = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        assert_eq!(app.update(ctrl('c'), now), [Command::Type(vec![3])]);
        assert!(!app.quit);
        assert!(app.update(ctrl('q'), now).is_empty());
        assert_eq!(press(&mut app, now, KeyCode::Char('d')), [Command::Detach]);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.typing.is_none());
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("stopped typing into nova-billing@main/web")
        );

        press(&mut app, now, KeyCode::Char('t'));
        app.update(
            Event::TypingEnded {
                session,
                message: Some("an earlier session".into()),
            },
            now,
        );
        assert_eq!(
            app.mode,
            Mode::Type,
            "events of an earlier session don't count"
        );
        app.update(
            Event::TypingEnded {
                session: session + 1,
                message: Some("web exited with code 1".into()),
            },
            now,
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("nova-billing@main: web exited with code 1")
        );
    }

    #[test]
    fn typing_stops_with_control_punctuation_as_crossterm_reports_it() {
        let (mut app, now) = connected();
        app.detach_keys = "ctrl-] d".parse().unwrap();
        for code in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Char('t'),
        ] {
            press(&mut app, now, code);
        }
        assert_eq!(app.mode, Mode::Type);
        let ctrl_close_bracket = KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(app.update(Event::Key(ctrl_close_bracket), now).is_empty());
        assert_eq!(press(&mut app, now, KeyCode::Char('d')), [Command::Detach]);
        assert_eq!(app.mode, Mode::Normal);
    }

    fn fetches_env(commands: &[Command]) -> (u64, String) {
        let fetches: Vec<(u64, String)> = commands
            .iter()
            .filter_map(|c| match c {
                Command::FetchEnv { generation, target } => {
                    assert_eq!(target.project.as_deref(), Some("nova"));
                    Some((*generation, target.address.clone()))
                }
                _ => None,
            })
            .collect();
        let [fetch] = fetches.as_slice() else {
            panic!("{commands:?}");
        };
        fetch.clone()
    }

    #[test]
    fn the_env_tab_shows_the_selected_process_or_the_app_s_web_process() {
        let (mut app, now) = connected();
        assert_eq!(
            press(&mut app, now, KeyCode::Char('3')),
            [Command::StopLogs]
        );
        assert_eq!(
            app.env.subject,
            EnvSubject::Unavailable("Select an app or a process to see its env.".into())
        );

        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        let (first, address) = fetches_env(&press(&mut app, now, KeyCode::Down));
        assert_eq!(address, "nova-billing@main/web");
        let EnvSubject::Fetch {
            note: Some(note), ..
        } = &app.env.subject
        else {
            panic!("{:?}", app.env.subject);
        };
        assert!(note.contains("web, which serves HTTP"), "{note}");
        assert!(note.contains("web, worker"), "{note}");

        app.update(
            Event::Env {
                generation: first,
                result: Ok(EnvResponse {
                    instance: "nova-billing@main/web".into(),
                    vars: Vec::new(),
                }),
            },
            now,
        );
        assert!(matches!(app.env.env, Some(Ok(_))));
        press(&mut app, now, KeyCode::Tab);
        assert!(
            press(&mut app, now, KeyCode::Char('j')).is_empty(),
            "j scrolls the env"
        );
        assert_eq!(app.env.scroll, 1);
        assert_eq!(app.cursor, 2, "the tree stays put");
        let (again, _) = fetches_env(&press(&mut app, now, KeyCode::Char('3')));
        assert!(again > first, "3 on the env tab fetches it again");
        press(&mut app, now, KeyCode::Tab);

        press(&mut app, now, KeyCode::Right);
        let mut moved = press(&mut app, now, KeyCode::Down);
        moved.extend(press(&mut app, now, KeyCode::Down));
        assert!(
            !moved.iter().any(|c| matches!(c, Command::FetchEnv { .. })),
            "one fetch at a time: {moved:?}"
        );
        let commands = app.update(
            Event::Env {
                generation: again,
                result: Err("late".into()),
            },
            now,
        );
        assert_eq!(
            app.env.env, None,
            "an answer for an earlier selection is dropped"
        );
        let (latest, address) = fetches_env(&commands);
        assert_eq!(address, "nova-billing@main/worker", "the latest selection");
        assert!(matches!(
            app.env.subject,
            EnvSubject::Fetch { note: None, .. }
        ));
        app.update(
            Event::Env {
                generation: latest,
                result: Err(
                    "no process of nova-billing serves HTTP; choose one with --process: web".into(),
                ),
            },
            now,
        );
        assert_eq!(
            app.env.env,
            Some(Err(
                "no process of nova-billing serves HTTP; select one in the tree: web".into()
            ))
        );
        assert!(
            press(&mut app, now, KeyCode::Char('?')).is_empty(),
            "answered, so nothing more to fetch"
        );
    }

    #[test]
    fn starts_on_the_app_it_was_opened_in() {
        let mut snapshot = sample();
        snapshot.projects[0].folder = Some("clients/apps".into());
        let now = Instant::now();
        let mut app = App::new(now);
        let here = NodeId::Instance {
            project: "nova".into(),
            stack: "feature-x".into(),
            app: "nova-frontend".into(),
        };
        app.start_at = Some(here.clone());
        app.update(Event::Disconnected("not yet".into()), now);
        assert_eq!(app.start_at, Some(here.clone()), "waits for a snapshot");

        let commands = app.update(Event::Snapshot(Box::new(snapshot.clone())), now);
        assert_eq!(app.selected, Some(here.clone()));
        assert_eq!(follows(&commands), ["nova-frontend@feature-x"]);
        assert!(app.fold.is_expanded(&app.roots[0]), "its folder opened");
        assert!(
            app.fold.is_expanded(&app.roots[0].children[0]),
            "and the nested one"
        );

        press(&mut app, now, KeyCode::Up);
        app.update(Event::Snapshot(Box::new(snapshot)), now);
        assert_ne!(app.selected, Some(here), "only the first snapshot");
    }

    #[test]
    fn ctrl_p_jumps_to_a_typed_name() {
        let (mut app, now) = connected();
        press(&mut app, now, KeyCode::Char('a'));
        assert_eq!(app.fold.show, Show::Running);
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        app.update(Event::Key(ctrl_p), now);
        assert_eq!(app.mode, Mode::Jump);
        press(&mut app, now, KeyCode::Char('q'));
        assert!(!app.quit, "letters are text");
        press(&mut app, now, KeyCode::Backspace);
        for c in "frontfx".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        let commands = press(&mut app, now, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.selected,
            Some(NodeId::Instance {
                project: "nova".into(),
                stack: "feature-x".into(),
                app: "nova-frontend".into(),
            })
        );
        assert_eq!(follows(&commands), ["nova-frontend@feature-x"]);
        assert_eq!(app.fold.show, Show::All, "a stopped app shows again");

        app.update(Event::Key(ctrl_p), now);
        press(&mut app, now, KeyCode::Char('b'));
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.jump, None);
    }

    #[test]
    fn slash_searches_the_logs_and_n_steps_through_matches() {
        let (mut app, now) = connected();
        app.logs.height = 3;
        for i in 0..12 {
            let text = if i % 4 == 0 {
                format!("Completed 500 in {i}ms")
            } else {
                format!("line {i}")
            };
            app.logs.push(
                app.logs.generation,
                LogLine {
                    project: "nova".into(),
                    app: "nova-billing".into(),
                    stack: "main".into(),
                    process: "web".into(),
                    at_ms: i,
                    text,
                    marker: false,
                },
            );
        }
        assert!(press(&mut app, now, KeyCode::Char('n')).is_empty());
        assert_eq!(app.notice, None, "n belongs to the logs");

        press(&mut app, now, KeyCode::Tab);
        press(&mut app, now, KeyCode::Char('n'));
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("/ searches the logs")
        );
        press(&mut app, now, KeyCode::Char('/'));
        for c in "500".chars() {
            press(&mut app, now, KeyCode::Char(c));
        }
        press(&mut app, now, KeyCode::Enter);
        assert_eq!(app.logs.search, "500");
        assert_eq!(app.logs.current, Some(8), "the newest match");
        assert_eq!(app.logs.window(3).len(), 3, "every line stays");
        press(&mut app, now, KeyCode::Char('n'));
        assert_eq!(app.logs.current, Some(4));
        press(&mut app, now, KeyCode::Char('N'));
        assert_eq!(app.logs.current, Some(8));
        press(&mut app, now, KeyCode::Esc);
        assert_eq!(app.logs.search, "");
        assert_eq!(app.logs.current, None);
        assert_eq!(app.logs.scroll, 0, "following again");
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
        press(&mut app, now, KeyCode::Right);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Right);
        assert_eq!(app.rows.len(), 7);
        press(&mut app, now, KeyCode::Down);
        press(&mut app, now, KeyCode::Left);
        assert_eq!(app.cursor, 2, "left on a leaf goes to the parent");
        press(&mut app, now, KeyCode::Left);
        assert_eq!(app.rows.len(), 5);
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
        assert_eq!(app.rows.len(), 3);
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
        for code in [
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Down,
        ] {
            press(&mut app, now, code);
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
                    failures: Vec::new(),
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
