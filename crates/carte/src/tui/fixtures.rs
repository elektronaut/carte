//! Snapshot builders for the TUI's tests.

use std::path::PathBuf;

use carte_api::{
    AppInfo, InstanceState, InstanceStatus, Policy, ProcessState, ProcessStatus, ProjectStatus,
    Snapshot, StackStatus,
};

pub fn snapshot(projects: Vec<ProjectStatus>) -> Snapshot {
    Snapshot {
        projects,
        problems: Vec::new(),
        running_instances: 0,
        memory_bytes: 0,
        shares: Vec::new(),
        tunnel: None,
    }
}

pub fn project(name: &str, apps: &[&str], stacks: Vec<StackStatus>) -> ProjectStatus {
    let prefix = format!("{name}-");
    ProjectStatus {
        folder: None,
        name: name.into(),
        apps: apps
            .iter()
            .map(|app| AppInfo {
                name: app.to_string(),
                short: app.strip_prefix(&prefix).unwrap_or(app).to_string(),
                path: PathBuf::from(format!("/work/{app}")),
                runnable: true,
            })
            .collect(),
        stacks,
    }
}

pub fn stack(name: &str, instances: Vec<InstanceStatus>) -> StackStatus {
    StackStatus {
        name: name.into(),
        branch: (name != "main").then(|| name.to_string()),
        instances,
    }
}

pub fn instance(app: &str, stack: &str, checkout: &str, state: InstanceState) -> InstanceStatus {
    InstanceStatus {
        app: app.into(),
        stack: stack.into(),
        checkout: checkout.into(),
        checkout_path: PathBuf::from(format!("/work/{app}")),
        url: format!("https://{app}.test"),
        public_url: None,
        state,
        processes: Vec::new(),
        holds: Vec::new(),
        idle_in_secs: None,
        memory_bytes: 0,
        cpu_percent: 0.0,
        last_error: None,
        env_changed: false,
        last_event: None,
        boot: None,
    }
}

pub fn process(name: &str, state: ProcessState) -> ProcessStatus {
    ProcessStatus {
        name: name.into(),
        state,
        policy: Policy::Auto,
        temporary: false,
        pid: None,
        port: None,
        http: name == "web",
        restarts: 0,
        memory_bytes: 0,
        cpu_percent: 0.0,
    }
}

/// nova: billing (two processes) running on main; feature-x with frontend from its
/// worktree and billing falling back to main.
pub fn sample() -> Snapshot {
    let mut billing = instance("nova-billing", "main", "main", InstanceState::Ready);
    billing.processes = vec![
        process("web", ProcessState::Running),
        process("worker", ProcessState::Running),
    ];
    snapshot(vec![project(
        "nova",
        &["nova-billing", "nova-frontend"],
        vec![
            stack(
                "main",
                vec![
                    billing,
                    instance("nova-frontend", "main", "main", InstanceState::Stopped),
                ],
            ),
            stack(
                "feature-x",
                vec![
                    instance("nova-billing", "feature-x", "main", InstanceState::Stopped),
                    instance(
                        "nova-frontend",
                        "feature-x",
                        "feature-x",
                        InstanceState::Stopped,
                    ),
                ],
            ),
        ],
    )])
}

/// `sample()` with nova-billing@main shared, as the daemon reports it.
pub fn shared_sample() -> Snapshot {
    let mut snapshot = sample();
    snapshot.projects[0].stacks[0].instances[0].public_url =
        Some("https://nova-billing--main--nova.demo.dev".into());
    snapshot.shares.push(carte_api::ShareStatus {
        project: "nova".into(),
        stack: "main".into(),
        apps: vec![carte_api::SharedApp {
            app: "nova-billing".into(),
            url: "https://nova-billing--main--nova.demo.dev".into(),
        }],
        auth: true,
        pinned: false,
        shared_secs: 120,
        expires_in_secs: None,
        requests: Vec::new(),
    });
    snapshot.tunnel = Some(carte_api::TunnelStatus {
        provider: "cloudflare".into(),
        single_host: false,
        state: carte_api::TunnelState::Connected,
        message: None,
    });
    snapshot
}
