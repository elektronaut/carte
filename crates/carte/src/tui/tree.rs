//! The folder ▸ project ▸ stack ▸ app ▸ process tree built from a snapshot, and the
//! rows it shows once collapsed nodes, filters and folding are applied.

use std::collections::{BTreeMap, HashMap};

use carte_api::{
    Hold, InstanceState, InstanceStatus, Policy, ProcessState, ProcessStatus, ProjectStatus,
    Snapshot, StackStatus,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum NodeId {
    /// A subdirectory of `projects/`, only for grouping, by its `/`-separated path.
    Folder {
        folder: String,
    },
    Project {
        project: String,
    },
    Stack {
        project: String,
        stack: String,
    },
    Instance {
        project: String,
        stack: String,
        app: String,
    },
    Process {
        project: String,
        stack: String,
        app: String,
        process: String,
    },
}

impl NodeId {
    /// Empty for a folder.
    pub fn project(&self) -> &str {
        match self {
            NodeId::Folder { .. } => "",
            NodeId::Project { project }
            | NodeId::Stack { project, .. }
            | NodeId::Instance { project, .. }
            | NodeId::Process { project, .. } => project,
        }
    }

    pub fn parent(&self) -> Option<NodeId> {
        match self.clone() {
            NodeId::Folder { .. } | NodeId::Project { .. } => None,
            NodeId::Stack { project, .. } => Some(NodeId::Project { project }),
            NodeId::Instance { project, stack, .. } => Some(NodeId::Stack { project, stack }),
            NodeId::Process {
                project,
                stack,
                app,
                ..
            } => Some(NodeId::Instance {
                project,
                stack,
                app,
            }),
        }
    }

    fn expanded_by_default(&self) -> bool {
        matches!(self, NodeId::Project { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ready,
    Starting,
    Stopping,
    Stopped,
    /// A process that exited cleanly on its own.
    Exited,
    /// A task that finished.
    Done,
    Crashed,
    Disabled,
}

/// Where a pin or disable comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Config,
    Temporary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: State,
    /// Running instances below an aggregate node, out of `total`.
    pub running: usize,
    pub total: usize,
    pub pinned: Option<Source>,
    pub disabled: Option<Source>,
    pub shared: bool,
    /// An instance in a stack that runs from the main checkout.
    pub fallback: bool,
    pub idle_in_secs: Option<u64>,
    pub held_by: Vec<String>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub problems: usize,
    /// carte stopped restarting the process, or one of the instance's; a badge on
    /// a running instance, the detail of a crashed process.
    pub gave_up: bool,
    /// Seconds until carte restarts a crashed process.
    pub restart_in_secs: Option<u64>,
}

impl Status {
    fn new(state: State) -> Self {
        Status {
            state,
            running: 0,
            total: 0,
            pinned: None,
            disabled: None,
            shared: false,
            fallback: false,
            idle_in_secs: None,
            held_by: Vec::new(),
            cpu_percent: 0.0,
            memory_bytes: 0,
            problems: 0,
            gave_up: false,
            restart_in_secs: None,
        }
    }

    /// Worth showing when only running things are.
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            State::Ready | State::Starting | State::Stopping | State::Crashed
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Levels merged into this node because each had a single child, outermost first.
    pub ids: Vec<NodeId>,
    pub label: String,
    /// Lowercased names of every merged level, for filtering.
    pub search: String,
    pub status: Status,
    pub children: Vec<Node>,
}

impl Node {
    /// The innermost merged level, which controls and logs act on.
    pub fn id(&self) -> &NodeId {
        self.ids.last().expect("a node has at least one id")
    }

    /// A stack folded into its project opens like the project.
    fn expanded_by_default(&self) -> bool {
        match self.ids.as_slice() {
            [.., NodeId::Project { .. }, NodeId::Stack { .. }] => true,
            _ => self.id().expanded_by_default(),
        }
    }

    fn leaf(id: NodeId, label: &str, search: &str, status: Status) -> Self {
        Node {
            ids: vec![id],
            label: label.to_string(),
            search: search.to_lowercase(),
            status,
            children: Vec::new(),
        }
    }

    /// Folds single children into their parent: a project with only the main stack
    /// shows its apps directly, and a single-app project shows the app's state on the
    /// project's row. Processes never fold, so an instance stays addressable as a whole.
    fn collapse(mut self) -> Self {
        self.children = self.children.into_iter().map(Node::collapse).collect();
        while self.children.len() == 1 && !matches!(self.children[0].id(), NodeId::Process { .. }) {
            let child = self.children.pop().expect("one child");
            self.ids.extend(child.ids);
            self.search = format!("{} {}", self.search, child.search);
            let problems = self.status.problems;
            self.status = Status {
                problems,
                ..child.status
            };
            self.children = child.children;
        }
        self
    }
}

/// Folders first, by name, then the projects outside any folder, at every level.
pub fn build(snapshot: &Snapshot) -> Vec<Node> {
    let mut root = Dir::default();
    for project in &snapshot.projects {
        let node = project_node(snapshot, project).collapse();
        let mut dir = &mut root;
        for segment in project.folder.iter().flat_map(|f| f.split('/')) {
            dir = dir.folders.entry(segment.to_string()).or_default();
        }
        dir.projects.push(node);
    }
    root.into_nodes(None)
}

#[derive(Default)]
struct Dir {
    folders: BTreeMap<String, Dir>,
    projects: Vec<Node>,
}

impl Dir {
    fn into_nodes(self, path: Option<&str>) -> Vec<Node> {
        let folders: Vec<Node> = self
            .folders
            .into_iter()
            .map(|(name, dir)| {
                let folder = match path {
                    Some(parent) => format!("{parent}/{name}"),
                    None => name.clone(),
                };
                let children = dir.into_nodes(Some(&folder));
                folder_node(folder, &name, children)
            })
            .collect();
        folders.into_iter().chain(self.projects).collect()
    }
}

/// `folder` is the full path below `projects/`, `name` its last segment.
fn folder_node(folder: String, name: &str, children: Vec<Node>) -> Node {
    let mut status = aggregate(&children);
    status.problems = children.iter().map(|c| c.status.problems).sum();
    Node {
        children,
        status,
        ..Node::leaf(
            NodeId::Folder { folder },
            &format!("{name}/"),
            name,
            Status::new(State::Stopped),
        )
    }
}

fn project_node(snapshot: &Snapshot, project: &ProjectStatus) -> Node {
    let children: Vec<Node> = project
        .stacks
        .iter()
        .map(|stack| stack_node(snapshot, project, stack))
        .collect();
    let mut status = aggregate(&children);
    status.problems = snapshot
        .problems
        .iter()
        .filter(|p| p.project.as_deref() == Some(&project.name))
        .count();
    Node {
        children,
        status,
        ..Node::leaf(
            NodeId::Project {
                project: project.name.clone(),
            },
            &project.name,
            &project.name,
            Status::new(State::Stopped),
        )
    }
}

fn stack_node(snapshot: &Snapshot, project: &ProjectStatus, stack: &StackStatus) -> Node {
    let children: Vec<Node> = stack
        .instances
        .iter()
        .map(|inst| instance_node(snapshot, project, inst))
        .collect();
    let search = format!("{} {}", stack.name, stack.branch.as_deref().unwrap_or(""));
    Node {
        status: aggregate(&children),
        children,
        ..Node::leaf(
            NodeId::Stack {
                project: project.name.clone(),
                stack: stack.name.clone(),
            },
            &stack.name,
            &search,
            Status::new(State::Stopped),
        )
    }
}

fn instance_node(snapshot: &Snapshot, project: &ProjectStatus, inst: &InstanceStatus) -> Node {
    let short = |name: &str| {
        project
            .apps
            .iter()
            .find(|a| a.name == name)
            .map_or(name, |a| a.short.as_str())
            .to_string()
    };
    let label = short(&inst.app);
    let id = NodeId::Instance {
        project: project.name.clone(),
        stack: inst.stack.clone(),
        app: inst.app.clone(),
    };
    let http = inst
        .processes
        .iter()
        .find(|p| p.http)
        .or(inst.processes.first());
    let disabled = http.and_then(|p| policy_source(p, Policy::Disabled));
    let pinned_procs: Vec<&ProcessStatus> = inst
        .processes
        .iter()
        .filter(|p| p.policy == Policy::Pinned)
        .collect();
    let pinned = (!pinned_procs.is_empty()).then(|| {
        if pinned_procs.iter().any(|p| p.temporary) {
            Source::Temporary
        } else {
            Source::Config
        }
    });
    let state = match inst.state {
        InstanceState::Ready => State::Ready,
        InstanceState::Starting => State::Starting,
        InstanceState::Stopping => State::Stopping,
        InstanceState::Crashed => State::Crashed,
        InstanceState::Stopped if disabled.is_some() => State::Disabled,
        InstanceState::Stopped => State::Stopped,
    };
    let held_by = inst
        .holds
        .iter()
        .filter_map(|hold| match hold {
            Hold::Pinned => None,
            Hold::Requests { count } => Some(plural(*count, "request")),
            Hold::Connections { count } => Some(plural(*count, "connection")),
            Hold::Dependent { app } => Some(short(app)),
            Hold::Exec { count } => Some(plural(*count, "exec")),
        })
        .collect();
    let mut status = Status {
        running: usize::from(matches!(state, State::Ready | State::Starting)),
        total: 1,
        pinned,
        disabled,
        shared: inst.public_url.is_some(),
        fallback: inst.stack != "main" && inst.checkout == "main",
        idle_in_secs: inst.idle_in_secs,
        held_by,
        cpu_percent: inst.cpu_percent,
        memory_bytes: inst.memory_bytes,
        gave_up: inst.processes.iter().any(|p| p.gave_up),
        ..Status::new(state)
    };
    status.problems = snapshot
        .problems
        .iter()
        .filter(|p| {
            p.project.as_deref() == Some(&project.name) && p.app.as_ref() == Some(&inst.app)
        })
        .count();
    let children = if inst.processes.len() > 1 {
        inst.processes
            .iter()
            .map(|p| process_node(&id, p))
            .collect()
    } else {
        Vec::new()
    };
    Node {
        children,
        ..Node::leaf(id, &label, &format!("{label} {}", inst.app), status)
    }
}

fn process_node(instance: &NodeId, process: &ProcessStatus) -> Node {
    let NodeId::Instance {
        project,
        stack,
        app,
    } = instance.clone()
    else {
        unreachable!("processes belong to instances");
    };
    let disabled = policy_source(process, Policy::Disabled);
    let state = match process.state {
        ProcessState::Running => State::Ready,
        ProcessState::Starting => State::Starting,
        ProcessState::Stopping => State::Stopping,
        ProcessState::Exited { code: Some(0), .. } => State::Exited,
        ProcessState::Exited { .. } => State::Crashed,
        ProcessState::Disabled => State::Disabled,
        ProcessState::Done => State::Done,
        ProcessState::Stopped if disabled.is_some() => State::Disabled,
        ProcessState::Stopped => State::Stopped,
    };
    let status = Status {
        running: usize::from(matches!(state, State::Ready | State::Starting)),
        total: 1,
        pinned: policy_source(process, Policy::Pinned),
        disabled,
        cpu_percent: process.cpu_percent,
        memory_bytes: process.memory_bytes,
        gave_up: process.gave_up,
        restart_in_secs: process.restart_in_secs,
        ..Status::new(state)
    };
    Node::leaf(
        NodeId::Process {
            project,
            stack,
            app,
            process: process.name.clone(),
        },
        &process.name,
        &process.name,
        status,
    )
}

fn policy_source(process: &ProcessStatus, policy: Policy) -> Option<Source> {
    (process.policy == policy).then_some(if process.temporary {
        Source::Temporary
    } else {
        Source::Config
    })
}

fn plural(count: u32, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// The state of a group: a crash anywhere shows, then starting, then running.
fn aggregate(children: &[Node]) -> Status {
    let any = |state| children.iter().any(|c| c.status.state == state);
    let state = [
        State::Crashed,
        State::Starting,
        State::Ready,
        State::Stopping,
    ]
    .into_iter()
    .find(|s| any(*s))
    .unwrap_or(State::Stopped);
    let any_source = |source: fn(&Status) -> Option<Source>| {
        children
            .iter()
            .filter_map(|c| source(&c.status))
            .max_by_key(|s| *s == Source::Temporary)
    };
    Status {
        running: children.iter().map(|c| c.status.running).sum(),
        total: children.iter().map(|c| c.status.total).sum(),
        pinned: any_source(|s| s.pinned),
        disabled: any_source(|s| s.disabled),
        shared: children.iter().any(|c| c.status.shared),
        cpu_percent: children.iter().map(|c| c.status.cpu_percent).sum(),
        memory_bytes: children.iter().map(|c| c.status.memory_bytes).sum(),
        ..Status::new(state)
    }
}

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Child indexes from the roots to the node.
    pub path: Vec<usize>,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
}

/// Which instances the tree shows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    #[default]
    All,
    Running,
    Shared,
}

impl Show {
    pub fn next(self) -> Self {
        match self {
            Show::All => Show::Running,
            Show::Running => Show::Shared,
            Show::Shared => Show::All,
        }
    }

    fn keeps(self, status: &Status) -> bool {
        match self {
            Show::All => true,
            Show::Running => status.is_active(),
            Show::Shared => status.shared,
        }
    }
}

/// What decides which rows show.
#[derive(Debug, Default, Clone)]
pub struct Fold {
    /// Explicit expand/collapse choices, keyed by the node's innermost id.
    pub expanded: HashMap<NodeId, bool>,
    pub filter: String,
    pub show: Show,
}

impl Fold {
    pub fn is_expanded(&self, node: &Node) -> bool {
        self.expanded
            .get(node.id())
            .copied()
            .unwrap_or_else(|| node.expanded_by_default())
    }

    /// The visible rows. A filter keeps matching nodes with their subtrees and opens
    /// the ancestors of matches; `show` hides instances and groups that are inactive
    /// or not shared.
    pub fn rows(&self, roots: &[Node]) -> Vec<Row> {
        let filter = self.filter.to_lowercase();
        let mut rows = Vec::new();
        for (i, node) in roots.iter().enumerate() {
            self.walk(node, vec![i], 0, &filter, filter.is_empty(), &mut rows);
        }
        rows
    }

    fn walk(
        &self,
        node: &Node,
        path: Vec<usize>,
        depth: usize,
        filter: &str,
        matched_above: bool,
        out: &mut Vec<Row>,
    ) -> bool {
        let process = matches!(node.id(), NodeId::Process { .. });
        if !process && !self.show.keeps(&node.status) {
            return false;
        }
        let matched = matched_above || node.search.contains(filter);
        let mut below = Vec::new();
        for (i, child) in node.children.iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(i);
            self.walk(child, child_path, depth + 1, filter, matched, &mut below);
        }
        if !matched && below.is_empty() {
            return false;
        }
        let expandable = !below.is_empty();
        let expanded = expandable && (!matched || self.is_expanded(node));
        out.push(Row {
            path,
            depth,
            expandable,
            expanded,
        });
        if expanded {
            out.extend(below);
        }
        true
    }
}

/// The child indexes from the roots to the node that holds `id`.
pub fn path_to(roots: &[Node], id: &NodeId) -> Option<Vec<usize>> {
    roots.iter().enumerate().find_map(|(i, node)| {
        if node.ids.contains(id) {
            return Some(vec![i]);
        }
        let mut path = path_to(&node.children, id)?;
        path.insert(0, i);
        Some(path)
    })
}

pub fn node<'a>(roots: &'a [Node], path: &[usize]) -> &'a Node {
    let (first, rest) = path.split_first().expect("a row has a path");
    rest.iter()
        .fold(&roots[*first], |node, &i| &node.children[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures::{instance, process, project, snapshot, stack};

    fn labels(roots: &[Node], fold: &Fold) -> Vec<String> {
        fold.rows(roots)
            .iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth), node(roots, &row.path).label))
            .collect()
    }

    #[test]
    fn a_project_with_only_main_lists_its_apps() {
        let snap = snapshot(vec![project(
            "nova",
            &["nova-billing", "nova-frontend"],
            vec![stack(
                "main",
                vec![
                    instance("nova-billing", "main", "main", InstanceState::Stopped),
                    instance("nova-frontend", "main", "main", InstanceState::Stopped),
                ],
            )],
        )]);
        let roots = build(&snap);
        assert_eq!(
            roots[0].ids,
            vec![
                NodeId::Project {
                    project: "nova".into()
                },
                NodeId::Stack {
                    project: "nova".into(),
                    stack: "main".into()
                }
            ]
        );
        assert_eq!(
            labels(&roots, &Fold::default()),
            ["nova", "  billing", "  frontend"]
        );
    }

    #[test]
    fn a_never_started_instance_lists_its_processes() {
        let mut web = instance("acme", "main", "main", InstanceState::Stopped);
        web.processes = vec![
            process("web", ProcessState::Stopped),
            process("worker", ProcessState::Stopped),
        ];
        web.processes[1].policy = Policy::Disabled;
        let snap = snapshot(vec![project(
            "acme",
            &["acme"],
            vec![stack("main", vec![web])],
        )]);
        let roots = build(&snap);
        let mut fold = Fold::default();
        fold.expanded.insert(roots[0].id().clone(), true);
        assert_eq!(labels(&roots, &fold), ["acme", "  web", "  worker"]);
        assert_eq!(roots[0].status.state, State::Stopped);
        let states: Vec<State> = roots[0].children.iter().map(|c| c.status.state).collect();
        assert_eq!(states, [State::Stopped, State::Disabled]);
    }

    #[test]
    fn a_single_app_project_shows_the_app_on_its_row() {
        let mut web = instance("acme", "main", "main", InstanceState::Ready);
        web.processes = vec![process("web", ProcessState::Running)];
        let snap = snapshot(vec![project(
            "acme",
            &["acme"],
            vec![stack("main", vec![web])],
        )]);
        let roots = build(&snap);
        assert_eq!(labels(&roots, &Fold::default()), ["acme"]);
        assert_eq!(roots[0].ids.len(), 3);
        assert_eq!(roots[0].status.state, State::Ready);
        assert!(
            roots[0].children.is_empty(),
            "a single process stays folded"
        );
    }

    #[test]
    fn folders_group_projects_and_start_folded() {
        let single = |name: &str, folder: Option<&str>, state| {
            let mut p = project(
                name,
                &[name],
                vec![stack("main", vec![instance(name, "main", "main", state)])],
            );
            p.folder = folder.map(str::to_string);
            p
        };
        let snap = snapshot(vec![
            single("blog", None, InstanceState::Stopped),
            single("wiki", Some("agency"), InstanceState::Ready),
            single("marketplace", Some("agency"), InstanceState::Stopped),
        ]);
        let roots = build(&snap);
        let mut fold = Fold::default();
        assert_eq!(labels(&roots, &fold), ["agency/", "blog"]);
        assert_eq!((roots[0].status.running, roots[0].status.total), (1, 2));

        fold.expanded.insert(
            NodeId::Folder {
                folder: "agency".into(),
            },
            true,
        );
        assert_eq!(
            labels(&roots, &fold),
            ["agency/", "  wiki", "  marketplace", "blog"]
        );

        let filtered = Fold {
            filter: "mar".into(),
            ..Fold::default()
        };
        assert_eq!(labels(&roots, &filtered), ["agency/", "  marketplace"]);

        let merged = NodeId::Instance {
            project: "marketplace".into(),
            stack: "main".into(),
            app: "marketplace".into(),
        };
        assert_eq!(
            path_to(&roots, &merged),
            Some(vec![0, 1]),
            "found in the project row it was merged into"
        );
        assert_eq!(
            path_to(
                &roots,
                &NodeId::Project {
                    project: "nope".into()
                }
            ),
            None
        );
    }

    #[test]
    fn folders_nest_and_aggregate_everything_below() {
        let single = |name: &str, folder: Option<&str>, state| {
            let mut p = project(
                name,
                &[name],
                vec![stack("main", vec![instance(name, "main", "main", state)])],
            );
            p.folder = folder.map(str::to_string);
            p
        };
        let mut snap = snapshot(vec![
            single("acme", None, InstanceState::Stopped),
            single("shop", Some("clients/sites"), InstanceState::Crashed),
            single("nova", Some("clients/apps"), InstanceState::Ready),
            single("mono", Some("clients"), InstanceState::Stopped),
        ]);
        snap.problems.push(carte_api::Problem {
            project: Some("shop".into()),
            app: None,
            message: "broken".into(),
        });
        let roots = build(&snap);
        let mut fold = Fold::default();
        assert_eq!(labels(&roots, &fold), ["clients/", "acme"]);
        assert_eq!(roots[0].status.state, State::Crashed);
        assert_eq!((roots[0].status.running, roots[0].status.total), (1, 3));
        assert_eq!(roots[0].status.problems, 1);

        let folder = |path: &str| NodeId::Folder {
            folder: path.into(),
        };
        fold.expanded.insert(folder("clients"), true);
        assert_eq!(
            labels(&roots, &fold),
            ["clients/", "  apps/", "  sites/", "  mono", "acme"]
        );
        assert_eq!(roots[0].children[0].id(), &folder("clients/apps"));
        assert_eq!(roots[0].children[0].status.state, State::Ready);
        assert_eq!(roots[0].children[1].status.problems, 1);

        fold.expanded.insert(folder("clients/sites"), true);
        assert_eq!(
            labels(&roots, &fold),
            [
                "clients/", "  apps/", "  sites/", "    shop", "  mono", "acme"
            ]
        );
    }

    #[test]
    fn single_app_stacks_fold_into_the_stack_row() {
        let snap = snapshot(vec![project(
            "acme",
            &["acme"],
            vec![
                stack(
                    "main",
                    vec![instance("acme", "main", "main", InstanceState::Stopped)],
                ),
                stack(
                    "feature-x",
                    vec![instance(
                        "acme",
                        "feature-x",
                        "feature-x",
                        InstanceState::Ready,
                    )],
                ),
            ],
        )]);
        let roots = build(&snap);
        assert_eq!(
            labels(&roots, &Fold::default()),
            ["acme", "  main", "  feature-x"]
        );
        assert_eq!(
            roots[0].children[1].id(),
            &NodeId::Instance {
                project: "acme".into(),
                stack: "feature-x".into(),
                app: "acme".into()
            }
        );
    }

    fn big() -> Vec<Node> {
        let mut frontend = instance(
            "nova-frontend",
            "feature-x",
            "feature-x",
            InstanceState::Ready,
        );
        frontend.processes = vec![
            process("web", ProcessState::Running),
            process("css", ProcessState::Running),
        ];
        build(&snapshot(vec![project(
            "nova",
            &["nova-billing", "nova-frontend"],
            vec![
                stack(
                    "main",
                    vec![
                        instance("nova-billing", "main", "main", InstanceState::Ready),
                        instance("nova-frontend", "main", "main", InstanceState::Stopped),
                    ],
                ),
                stack(
                    "feature-x",
                    vec![
                        instance("nova-billing", "feature-x", "main", InstanceState::Stopped),
                        frontend,
                    ],
                ),
            ],
        )]))
    }

    #[test]
    fn stacks_and_instances_start_folded_and_unfold_on_request() {
        let roots = big();
        let mut fold = Fold::default();
        assert_eq!(labels(&roots, &fold), ["nova", "  main", "  feature-x"]);
        fold.expanded
            .insert(roots[0].children[1].id().clone(), true);
        fold.expanded
            .insert(roots[0].children[1].children[1].id().clone(), true);
        assert_eq!(
            labels(&roots, &fold),
            [
                "nova",
                "  main",
                "  feature-x",
                "    billing",
                "    frontend",
                "      web",
                "      css"
            ]
        );
    }

    #[test]
    fn filters_keep_matches_and_open_their_ancestors() {
        let roots = big();
        let fold = Fold {
            filter: "CSS".into(),
            ..Fold::default()
        };
        assert_eq!(
            labels(&roots, &fold),
            ["nova", "  feature-x", "    frontend", "      css"]
        );
        let fold = Fold {
            filter: "feature".into(),
            ..Fold::default()
        };
        assert_eq!(
            labels(&roots, &fold),
            ["nova", "  feature-x"],
            "a matching stack keeps its fold"
        );
    }

    #[test]
    fn running_only_hides_stopped_instances_and_empty_groups() {
        let roots = big();
        let mut fold = Fold {
            show: Show::Running,
            ..Fold::default()
        };
        for stack in &roots[0].children {
            fold.expanded.insert(stack.id().clone(), true);
        }
        assert_eq!(
            labels(&roots, &fold),
            [
                "nova",
                "  main",
                "    billing",
                "  feature-x",
                "    frontend"
            ]
        );
    }

    #[test]
    fn shared_shows_shared_instances_whether_running_or_not() {
        let mut snap = snapshot(vec![project(
            "nova",
            &["nova-billing", "nova-frontend"],
            vec![
                stack(
                    "main",
                    vec![
                        instance("nova-billing", "main", "main", InstanceState::Ready),
                        instance("nova-frontend", "main", "main", InstanceState::Stopped),
                    ],
                ),
                stack(
                    "feature-x",
                    vec![instance(
                        "nova-billing",
                        "feature-x",
                        "main",
                        InstanceState::Ready,
                    )],
                ),
            ],
        )]);
        snap.projects[0].stacks[0].instances[1].public_url =
            Some("https://nova-frontend.example.com".into());
        let roots = build(&snap);
        let mut fold = Fold {
            show: Show::Shared,
            ..Fold::default()
        };
        fold.expanded
            .insert(roots[0].children[0].id().clone(), true);
        assert_eq!(labels(&roots, &fold), ["nova", "  main", "    frontend"]);
    }

    #[test]
    fn names_what_holds_an_instance() {
        let mut web = instance("acme", "main", "main", InstanceState::Ready);
        web.holds = vec![
            Hold::Pinned,
            Hold::Connections { count: 1 },
            Hold::Exec { count: 1 },
            Hold::Exec { count: 2 },
        ];
        let snap = snapshot(vec![project(
            "acme",
            &["acme"],
            vec![stack("main", vec![web])],
        )]);
        assert_eq!(
            build(&snap)[0].status.held_by,
            ["1 connection", "1 exec", "2 execs"]
        );
    }

    #[test]
    fn aggregates_count_running_instances_and_surface_crashes() {
        let roots = big();
        assert_eq!(roots[0].status.running, 2);
        assert_eq!(roots[0].status.total, 4);
        assert_eq!(roots[0].status.state, State::Ready);
        assert!(roots[0].children[1].children[0].status.fallback);
        assert!(!roots[0].children[1].children[1].status.fallback);
    }
}
