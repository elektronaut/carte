use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::{
    App, AppSettings, MachineConfig, Project, ProjectState, ShareConfig, Worktree, World,
};
use crate::slug::branch_slug;

pub fn app(name: &str, short: &str, path: &str) -> App {
    App {
        name: name.into(),
        short: short.into(),
        path: PathBuf::from(path),
        settings: AppSettings::default(),
    }
}

pub fn worktree(branch: &str, path: &str) -> Worktree {
    Worktree {
        branch: branch.into(),
        slug: branch_slug(branch),
        path: PathBuf::from(path),
    }
}

pub fn project(name: &str, strip_prefix: Option<&str>, apps: Vec<App>) -> Project {
    Project {
        name: name.into(),
        config_path: PathBuf::from(format!("/Users/dev/.config/carte/projects/{name}.toml")),
        strip_prefix: strip_prefix.map(Into::into),
        env_provider: None,
        env: Vec::new(),
        requires: Vec::new(),
        scheme: Default::default(),
        idle_timeout: None,
        apps,
    }
}

pub fn state(project: Project, worktrees: Vec<(&str, Vec<Worktree>)>) -> ProjectState {
    ProjectState {
        project,
        worktrees: worktrees
            .into_iter()
            .map(|(app, wts)| (app.to_string(), wts))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// `nova`: three services with `nova-` stripped, stacks `feature-x` and `claude-fix-thing-123`.
pub fn nova() -> ProjectState {
    let mut frontend = app(
        "nova-postcard-frontend",
        "postcard-frontend",
        "/Users/dev/work/nova-postcard-frontend",
    );
    frontend.settings.aliases = vec!["nova-postcard-frontend".into()];
    let api = app(
        "nova-postcard-api",
        "postcard-api",
        "/Users/dev/work/nova-postcard-api",
    );
    let billing = app("nova-billing", "billing", "/Users/dev/work/nova-billing");
    state(
        project("nova", Some("nova-"), vec![frontend, api, billing]),
        vec![
            (
                "nova-postcard-frontend",
                vec![
                    worktree(
                        "feature-x",
                        "/Users/dev/work/nova-postcard-frontend-feature-x",
                    ),
                    worktree(
                        "claude/fix-thing-123",
                        "/Users/dev/work/nova-postcard-frontend/.claude/worktrees/fix-thing-123",
                    ),
                ],
            ),
            (
                "nova-postcard-api",
                vec![worktree(
                    "feature-x",
                    "/Users/dev/work/nova-postcard-api-feature-x",
                )],
            ),
        ],
    )
}

/// `eventportal`: a single collapsed app that serves tenant subdomains.
pub fn eventportal() -> ProjectState {
    let mut app = app("eventportal", "eventportal", "/Users/dev/code/eventportal");
    app.settings.subdomains = true;
    state(
        project("eventportal", None, vec![app]),
        vec![(
            "eventportal",
            vec![worktree(
                "feature-x",
                "/Users/dev/code/eventportal-feature-x",
            )],
        )],
    )
}

/// `acme`: a collapsed `acme` app next to `acme-api`, with a `feature-x` worktree of the api.
pub fn acme() -> ProjectState {
    state(
        project(
            "acme",
            Some("acme-"),
            vec![
                app("acme", "acme", "/Users/dev/clients/acme"),
                app("acme-api", "api", "/Users/dev/clients/acme-api"),
            ],
        ),
        vec![(
            "acme-api",
            vec![worktree(
                "feature-x",
                "/Users/dev/clients/acme-api-feature-x",
            )],
        )],
    )
}

pub fn world() -> World {
    World {
        machine: MachineConfig {
            share: Some(ShareConfig {
                provider: "cloudflare".into(),
                domain: "alex.carte.dev".into(),
            }),
            ..MachineConfig::default()
        },
        projects: vec![nova(), eventportal(), acme()],
        problems: Vec::new(),
    }
}
