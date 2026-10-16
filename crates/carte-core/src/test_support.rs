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

/// `make`: three services with `make-` stripped, stacks `feature-x` and `claude-fix-thing-123`.
pub fn make() -> ProjectState {
    let mut frontend = app(
        "make-newsmail-frontend",
        "newsmail-frontend",
        "/Users/dev/work/make-newsmail-frontend",
    );
    frontend.settings.aliases = vec!["make-newsmail-frontend".into()];
    let api = app(
        "make-newsmail-api",
        "newsmail-api",
        "/Users/dev/work/make-newsmail-api",
    );
    let billing = app("make-billing", "billing", "/Users/dev/work/make-billing");
    state(
        project("make", Some("make-"), vec![frontend, api, billing]),
        vec![
            (
                "make-newsmail-frontend",
                vec![
                    worktree(
                        "feature-x",
                        "/Users/dev/work/make-newsmail-frontend-feature-x",
                    ),
                    worktree(
                        "claude/fix-thing-123",
                        "/Users/dev/work/make-newsmail-frontend/.claude/worktrees/fix-thing-123",
                    ),
                ],
            ),
            (
                "make-newsmail-api",
                vec![worktree(
                    "feature-x",
                    "/Users/dev/work/make-newsmail-api-feature-x",
                )],
            ),
        ],
    )
}

/// `delegatbase`: a single collapsed app that serves tenant subdomains.
pub fn delegatbase() -> ProjectState {
    let mut app = app("delegatbase", "delegatbase", "/Users/dev/code/delegatbase");
    app.settings.subdomains = true;
    state(
        project("delegatbase", None, vec![app]),
        vec![(
            "delegatbase",
            vec![worktree(
                "feature-x",
                "/Users/dev/code/delegatbase-feature-x",
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
                domain: "inge.carte.dev".into(),
            }),
            ..MachineConfig::default()
        },
        projects: vec![make(), delegatbase(), acme()],
        problems: Vec::new(),
    }
}
