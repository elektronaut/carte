use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{anyhow, bail};
use carte_api::{EnvResponse, EnvVar};
use carte_core::address::{self, Address};
use carte_core::env::{self as core_env, EnvRequest, Source, SystemRunner, TemplateOrigin};
use carte_core::model::Checkout;
use carte_core::procfile::{self, ProcessSource};
use carte_core::{discovery, naming};

use super::EnvArgs;
use super::prompt::tilde;
use crate::paths::Paths;
use crate::urls::StackUrls;

pub fn run(paths: &Paths, args: EnvArgs, json: bool) -> anyhow::Result<ExitCode> {
    let world = discovery::load_world(&paths.config_dir);
    let typed = args.target.address.as_deref().unwrap_or("");
    let mut address: Address = typed.parse()?;
    if args.process.is_some() {
        address.process = args.process;
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let target = match &args.target.project {
        Some(project) => address::resolve_in_project(&world, project, &address, Some(&cwd))?,
        None => address::resolve(&world, &address, Some(&cwd))?,
    };
    let Some(key) = target.instance() else {
        bail!("{typed:?} names a whole project or stack; give an app, as in `carte env <app>`");
    };
    let state = world
        .project(&key.project)
        .ok_or_else(|| anyhow!("unknown project {}", key.project))?;
    let app = state
        .project
        .app(&key.app)
        .ok_or_else(|| anyhow!("unknown app {}", key.app))?;
    let checkout = state.checkout(app, &key.stack);
    let branch = match &checkout {
        Checkout::Worktree(wt) => wt.branch.clone(),
        Checkout::Main { path } => current_branch(path),
    };

    let list = procfile::load(checkout.path(), &app.settings)?;
    if list.source == ProcessSource::RouteOnly {
        bail!(
            "{} only routes to a fixed port; it runs no processes",
            app.name
        );
    }
    let names = || {
        list.processes
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let process = match &target.process {
        Some(name) => list
            .processes
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| {
                anyhow!(
                    "{} has no process {name:?}; choose from {}",
                    app.name,
                    names()
                )
            })?,
        None => list.processes.iter().find(|p| p.http).ok_or_else(|| {
            anyhow!(
                "no process of {} serves HTTP; choose one with --process: {}",
                app.name,
                names()
            )
        })?,
    };

    let machine = &world.machine;
    let shared = BTreeSet::new();
    let urls = StackUrls {
        machine,
        state,
        stack: &key.stack,
        shared: &shared,
    };
    let ca_bundle = paths.ca_bundle();
    let ca_cert = paths.ca_cert();
    let pidfile = paths.pidfile(&key);
    let tld_length = naming::tld_length(&machine.tld, &state.project, app, &key.stack);
    let daemon = daemon_view(paths, &key, &process.name);
    let port = daemon.as_ref().and_then(|d| d.port).unwrap_or(0);
    let req = EnvRequest {
        project: &state.project,
        app,
        process,
        checkout: &checkout,
        stack: &key.stack,
        branch: &branch,
        port,
        urls: &urls,
        tld: &machine.tld,
        share_domain: machine.share.as_ref().map(|s| s.domain.as_str()),
        tld_length: tld_length as u32,
        ca_bundle: &ca_bundle,
        ca_cert: &ca_cert,
        pidfile: &pidfile,
    };
    let daemon_env = match &daemon {
        Some(view) => view.env.clone(),
        None => crate::supervisor::base_env(),
    };
    let runner = SystemRunner::from_env();
    let env = core_env::build_env(&req, &daemon_env, &runner)?;

    let instance = format!("{}@{}/{}", app.name, key.stack.slug(), process.name);
    let shown = |key: &str, value: &str, source: &Source| {
        if key == "PORT" && *source == Source::Injected && port == 0 {
            "<assigned at start>".to_string()
        } else {
            value.to_string()
        }
    };

    if json {
        let response = EnvResponse {
            instance,
            vars: env
                .iter()
                .map(|(key, v)| EnvVar {
                    key: key.to_string(),
                    value: shown(key, &v.value, &v.source),
                    source: v.source.to_string(),
                    overrides: v.overrode.iter().map(|o| o.source.to_string()).collect(),
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(ExitCode::SUCCESS);
    }

    let provider = core_env::detect_provider(state.project.env_provider, checkout.path(), &runner);
    println!(
        "{instance}  {}  (env from {}{})",
        tilde(checkout.path()),
        core_env::provider_name(provider),
        if daemon.is_some() {
            ", as the daemon builds it"
        } else {
            "; the daemon isn't running, so this shell's environment stands in for it"
        }
    );
    println!();
    let label = |source: &Source| source_label(source, checkout.path());
    let key_width = env.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let source_width = env
        .iter()
        .map(|(_, v)| label(&v.source).len())
        .max()
        .unwrap_or(0);
    for (key, v) in env.iter() {
        println!(
            "{key:key_width$}  {:source_width$}  {}",
            label(&v.source),
            shown(key, &v.value, &v.source)
        );
        for old in v.overrode.iter().rev() {
            println!(
                "{:key_width$}  ↳ overrode {}: {}",
                "",
                label(&old.source),
                old.value
            );
        }
    }
    if !list.notices.is_empty() {
        println!();
    }
    for notice in &list.notices {
        println!("note: {notice}");
    }
    Ok(ExitCode::SUCCESS)
}

struct DaemonView {
    env: std::collections::BTreeMap<String, String>,
    /// The process's port while it runs.
    port: Option<u16>,
}

/// The daemon's base env and the process's assigned port, when the daemon runs.
fn daemon_view(
    paths: &Paths,
    key: &carte_core::model::InstanceKey,
    process: &str,
) -> Option<DaemonView> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let client = crate::client::Client::new(&paths.socket());
    runtime.block_on(async {
        let env: carte_api::DaemonEnv = client.get(carte_api::paths::ENV).await.ok()?;
        let snapshot: carte_api::Snapshot = client.get(carte_api::paths::SNAPSHOT).await.ok()?;
        let port = snapshot
            .projects
            .iter()
            .filter(|p| p.name == key.project)
            .flat_map(|p| &p.stacks)
            .filter(|s| s.name == key.stack.slug())
            .flat_map(|s| &s.instances)
            .filter(|i| i.app == key.app)
            .flat_map(|i| &i.processes)
            .find(|p| p.name == process && p.pid.is_some())
            .and_then(|p| p.port);
        Some(DaemonView {
            env: env.vars,
            port,
        })
    })
}

/// A short name for a source; the JSON output has the full one.
fn source_label(source: &Source, checkout: &Path) -> String {
    match source {
        Source::EnvFile { path, line } => {
            let path = path.strip_prefix(checkout).unwrap_or(path);
            format!("{}:{line}", path.display())
        }
        Source::Template { origin, .. } => match origin {
            TemplateOrigin::Project { .. } => "project template".into(),
            TemplateOrigin::App { .. } => "app template".into(),
            TemplateOrigin::Process { .. } => "process template".into(),
        },
        other => other.to_string(),
    }
}

/// The branch checked out in a primary checkout; empty when detached.
fn current_branch(checkout: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}
