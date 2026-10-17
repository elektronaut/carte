//! The env a process starts with, as `carte env` and the daemon's env request show it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail};
use carte_api::{EnvResponse, EnvVar, Snapshot};
use carte_core::address;
use carte_core::env::{self as core_env, Env, EnvRequest, Source, SystemRunner};
use carte_core::model::{Checkout, InstanceKey, World};
use carte_core::naming;
use carte_core::procfile::{self, ProcessSource};

use crate::exit::{Exit, Failure};
use crate::paths::Paths;
use crate::urls::StackUrls;

/// What the daemon knows that a rendering depends on.
pub struct DaemonState {
    /// The daemon's own environment, which providers start from.
    pub env: BTreeMap<String, String>,
    /// Ports of the instance's running processes, by name.
    pub ports: BTreeMap<String, u16>,
    /// Apps of the instance's stack that are shared.
    pub shared: BTreeSet<String>,
}

impl DaemonState {
    /// The ports and shares of `key`'s instance as a snapshot reports them.
    pub fn new(env: BTreeMap<String, String>, snapshot: &Snapshot, key: &InstanceKey) -> Self {
        let stack = key.stack.slug();
        let ports = snapshot
            .projects
            .iter()
            .filter(|p| p.name == key.project)
            .flat_map(|p| &p.stacks)
            .filter(|s| s.name == stack)
            .flat_map(|s| &s.instances)
            .filter(|i| i.app == key.app)
            .flat_map(|i| &i.processes)
            .filter(|p| p.pid.is_some())
            .filter_map(|p| Some((p.name.clone(), p.port?)))
            .collect();
        let shared = snapshot
            .shares
            .iter()
            .filter(|s| s.project == key.project && s.stack == stack)
            .flat_map(|s| s.apps.iter().map(|a| a.app.clone()))
            .collect();
        DaemonState { env, ports, shared }
    }
}

pub struct Rendered {
    /// `app@stack/process`.
    pub instance: String,
    pub checkout: PathBuf,
    pub provider: &'static str,
    pub env: Env,
    /// The process's port, 0 until it runs.
    pub port: u16,
    pub notices: Vec<String>,
}

impl Rendered {
    /// The value shown for a variable: `PORT` reads as unassigned until the process runs.
    pub fn shown(&self, key: &str, value: &str, source: &Source) -> String {
        if key == "PORT" && *source == Source::Injected && self.port == 0 {
            "<assigned at start>".to_string()
        } else {
            value.to_string()
        }
    }

    /// A variable as the API reports it; with `mask`, values carte didn't render itself are left out.
    pub fn var(&self, key: &str, mask: bool) -> Option<EnvVar> {
        let v = self.env.get(key)?;
        let value = (!mask || revealed(&v.source)).then(|| self.shown(key, &v.value, &v.source));
        Some(EnvVar {
            key: key.to_string(),
            masked: value.is_none(),
            value,
            source: v.source.to_string(),
            overrides: v.overrode.iter().map(|o| o.source.to_string()).collect(),
        })
    }

    pub fn response(&self, mask: bool) -> EnvResponse {
        EnvResponse {
            instance: self.instance.clone(),
            vars: self
                .env
                .iter()
                .filter_map(|(key, _)| self.var(key, mask))
                .collect(),
        }
    }
}

/// Values carte renders itself, which can't hold secrets from the environment.
pub fn revealed(source: &Source) -> bool {
    matches!(
        source,
        Source::Injected | Source::Template { .. } | Source::Provided { .. }
    )
}

/// Renders the env of `process` of an instance, or of its HTTP process.
pub fn render(
    paths: &Paths,
    world: &World,
    key: &InstanceKey,
    process: Option<&str>,
    daemon: &DaemonState,
) -> anyhow::Result<Rendered> {
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
    let spec = match process {
        Some(name) => list
            .processes
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                Failure::new(
                    Exit::NotFound,
                    format!(
                        "{} has no process {name:?}; choose from {}",
                        app.name,
                        names()
                    ),
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
    let urls = StackUrls {
        machine,
        state,
        stack: &key.stack,
        shared: &daemon.shared,
    };
    let ca_bundle = paths.ca_bundle();
    let ca_cert = paths.ca_cert();
    let pidfile = paths.pidfile(key);
    let dirs = paths.dir_roots().instance(key);
    let tld_length = naming::tld_length(&machine.tld, &state.project, app, &key.stack);
    let port = daemon.ports.get(&spec.name).copied().unwrap_or(0);
    let processes: Vec<_> = list
        .processes
        .iter()
        .map(|p| (p.clone(), daemon.ports.get(&p.name).copied().unwrap_or(0)))
        .collect();
    let req = EnvRequest {
        project: &state.project,
        app,
        process: spec,
        processes: &processes,
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
        dirs: &dirs,
    };
    let runner = SystemRunner::from_env();
    let env = core_env::build_env(&req, &daemon.env, &runner)?;
    let provider = core_env::detect_provider(state.project.env_provider, checkout.path(), &runner);
    Ok(Rendered {
        instance: format!("{}@{}/{}", app.name, key.stack.slug(), spec.name),
        checkout: checkout.path().to_path_buf(),
        provider: core_env::provider_name(provider),
        env,
        port,
        notices: list.notices.iter().map(ToString::to_string).collect(),
    })
}

/// The env for an address the daemon was sent, rendered with the daemon's own env.
pub fn for_address(
    paths: &Paths,
    world: &World,
    snapshot: &Snapshot,
    target: &carte_api::Address,
) -> anyhow::Result<EnvResponse> {
    let parsed: address::Address = target.address.parse()?;
    let cwd = target.cwd.as_deref();
    let resolved = match &target.project {
        Some(project) => address::resolve_in_project(world, project, &parsed, cwd)?,
        None => address::resolve(world, &parsed, cwd)?,
    };
    let Some(key) = resolved.instance() else {
        bail!("select an app or a process to see its env");
    };
    let daemon = DaemonState::new(crate::supervisor::base_env(), snapshot, &key);
    let rendered = render(paths, world, &key, resolved.process.as_deref(), &daemon)?;
    Ok(rendered.response(false))
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
