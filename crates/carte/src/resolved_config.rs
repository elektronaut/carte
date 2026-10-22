//! What carte resolved for an address, as the daemon's config request shows it.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail};
use carte_api::{
    Address, Dependency, KubeForward, ProcessStatus, ResolvedApp, ResolvedConfig, ResolvedEnv,
    ResolvedHost, ResolvedKube, ResolvedProcess, ResolvedProject, ResolvedStack, Setting, Snapshot,
    StackCheckout,
};
use carte_core::address;
use carte_core::env::{self as core_env, SystemRunner};
use carte_core::kube::{self, KubeAddress};
use carte_core::model::{
    App, EnvTemplate, InstanceKey, Policy, ProjectState, Readiness, Stack, World,
};
use carte_core::naming;
use carte_core::procfile::{self, ProcessSource};
use carte_core::resolved::{self, ProcessSettings};
use serde::Serialize;

/// What the supervisor does with every process, which configuration doesn't change.
#[derive(Debug, Clone, Copy)]
pub struct ProcessLimits {
    pub boot_timeout: Duration,
    pub stop_grace: Duration,
    pub crash_window: Duration,
}

/// The configuration carte resolved for `target`, with the ports and runtime pins of
/// the running processes the snapshot shows.
pub fn for_address(
    world: &World,
    snapshot: &Snapshot,
    limits: ProcessLimits,
    target: &Address,
) -> anyhow::Result<ResolvedConfig> {
    let parsed: address::Address = target.address.parse()?;
    if let Some(kube) = kube::resolve_address(world, target.project.as_deref(), &parsed) {
        return Ok(ResolvedConfig::Kube(kube_project(
            world,
            &kube.map_err(|err| anyhow!("{err}"))?,
        )?));
    }
    let cwd = target.cwd.as_deref();
    let resolved = match &target.project {
        Some(project) => address::resolve_in_project(world, project, &parsed, cwd),
        None => address::resolve(world, &parsed, cwd),
    }
    .map_err(|err| anyhow!("{err}"))?;
    let state = world
        .project(&resolved.project)
        .ok_or_else(|| anyhow!("no project named {}", resolved.project))?;
    let Some(app_name) = &resolved.app else {
        let stack = resolved.stack.as_ref().map(|s| s.slug().to_string());
        return Ok(ResolvedConfig::Project(project(world, state, stack)));
    };
    let key = InstanceKey {
        project: resolved.project.clone(),
        app: app_name.clone(),
        stack: resolved.stack.clone().unwrap_or(Stack::Main),
    };
    let mut app = self::app(world, snapshot, limits, state, &key)?;
    match &resolved.process {
        None => Ok(ResolvedConfig::App(app)),
        Some(name) => {
            if let Some(error) = app.process_error {
                bail!(error);
            }
            let at = app.processes.iter().position(|p| p.name == *name);
            match at {
                Some(at) => Ok(ResolvedConfig::Process(app.processes.swap_remove(at))),
                None => bail!("{} has no process {name}", app.app),
            }
        }
    }
}

fn project(world: &World, state: &ProjectState, stack: Option<String>) -> ResolvedProject {
    let project = &state.project;
    let settings = resolved::project(&world.machine, project);
    let stacks = std::iter::once(Stack::Main)
        .chain(state.stacks().into_iter().map(Stack::Branch))
        .filter(|s| stack.as_deref().is_none_or(|name| s.slug() == name))
        .map(|s| ResolvedStack {
            name: s.slug().to_string(),
            branch: naming::stack_branch(state, s.slug()).map(str::to_string),
            checkouts: project
                .apps
                .iter()
                .filter_map(|app| {
                    let checkout = state.checkout(app, &s)?;
                    Some(StackCheckout {
                        app: app.name.clone(),
                        checkout: checkout.slug().to_string(),
                        path: checkout.path().clone(),
                    })
                })
                .collect(),
        })
        .collect();
    ResolvedProject {
        name: project.name.clone(),
        config_path: project.config_path.clone(),
        from_template: project.from_template,
        stack,
        stacks,
        strip_prefix: project.strip_prefix.clone(),
        env_provider: api(settings.env_provider.map(|p| p.map(|p| name(&p)))),
        env: templates(&project.env),
        requires: project.requires.clone(),
        scheme: api(settings.scheme.map(|s| name(&s))),
        idle_timeout_secs: api(settings.idle_timeout.map(|d| d.as_secs())),
        teardown_delay_secs: api(settings.teardown_delay.map(|d| d.as_secs())),
        serve_files: api(settings.serve_files),
    }
}

fn app(
    world: &World,
    snapshot: &Snapshot,
    limits: ProcessLimits,
    state: &ProjectState,
    key: &InstanceKey,
) -> anyhow::Result<ResolvedApp> {
    let project = &state.project;
    let configured = project
        .app(&key.app)
        .ok_or_else(|| anyhow!("no app named {} in {}", key.app, key.project))?;
    let checkout = state
        .checkout(configured, &key.stack)
        .ok_or_else(|| anyhow!(state.not_in_stack(configured, &key.stack)))?;
    let app = state.app_at(configured, &key.stack);
    let settings = resolved::app(&world.machine, project, app);
    let runner = SystemRunner::from_env();
    let provider = |configured| core_env::detect_provider(configured, checkout.path(), &runner);
    let env = ResolvedEnv {
        provider: Setting {
            value: name(&provider(project.env_provider)),
            default: name(&provider(None)),
        },
        files: api(settings.env_files),
        templates: templates(project.env.iter().chain(&app.settings.env)),
    };
    let statuses = statuses(snapshot, key);
    let (process_source, processes, process_error) =
        match resolved::processes(checkout.path(), &app.settings) {
            Ok((source, processes)) => {
                let processes = processes
                    .into_iter()
                    .map(|p| {
                        let status = statuses.iter().find(|s| s.name == p.name).copied();
                        process(p, checkout.path(), status, limits)
                    })
                    .collect();
                (Some(source_name(source).to_string()), processes, None)
            }
            Err(err) => (None, Vec::new(), Some(err.to_string())),
        };
    Ok(ResolvedApp {
        project: project.name.clone(),
        app: app.name.clone(),
        short: app.short.clone(),
        stack: key.stack.slug().to_string(),
        checkout: checkout.slug().to_string(),
        checkout_path: checkout.path().clone(),
        hostnames: hostnames(world, state, app, &key.stack),
        aliases: api(settings.aliases),
        subdomains: api(settings.subdomains),
        env,
        idle_timeout_secs: api(settings.idle_timeout.map(|d| d.as_secs())),
        depends_on: api(settings.depends_on.map(|deps| {
            deps.into_iter()
                .map(|dep| Dependency {
                    project: dep.project,
                    app: dep.app,
                })
                .collect()
        })),
        serve_files: api(settings.serve_files),
        policy: api(settings.policy.map(policy)),
        port: api(settings.port),
        warm: api(settings.warm),
        process_source,
        processes,
        process_error,
    })
}

/// The instance's own hostnames on each served TLD, its aliases, and main's
/// hostnames when it is in another stack.
fn hostnames(world: &World, state: &ProjectState, app: &App, stack: &Stack) -> Vec<ResolvedHost> {
    let tlds = world.machine.served_tlds();
    let host = |tld: &str, stack: &Stack| ResolvedHost {
        host: naming::local_host(tld, &state.project, app, stack),
        stack: stack.slug().to_string(),
        alias: false,
    };
    let mut hosts: Vec<ResolvedHost> = tlds.iter().map(|tld| host(tld, stack)).collect();
    if *stack != Stack::Main && state.runs_in(app, &Stack::Main) {
        hosts.extend(tlds.iter().map(|tld| host(tld, &Stack::Main)));
    }
    hosts.extend(app.settings.aliases.iter().flat_map(|alias| {
        tlds.iter().map(move |tld| ResolvedHost {
            host: format!("{alias}.{tld}"),
            stack: "main".into(),
            alias: true,
        })
    }));
    hosts
}

fn process(
    settings: ProcessSettings,
    checkout: &Path,
    status: Option<&ProcessStatus>,
    limits: ProcessLimits,
) -> ResolvedProcess {
    let port = status.filter(|s| s.pid.is_some()).and_then(|s| s.port);
    let always_restarted = settings.http.value || !settings.paths.value.is_empty();
    ResolvedProcess {
        runs_as: port.map(|port| procfile::substitute_port(&settings.command.value, port)),
        dir: settings
            .cwd
            .value
            .as_ref()
            .map_or_else(|| checkout.to_path_buf(), |cwd| checkout.join(cwd)),
        name: settings.name,
        command: api(settings.command),
        rewritten_from: settings.rewritten_from,
        cwd: api(settings.cwd),
        port,
        http: api(settings.http),
        ready: api(settings.ready.map(|ready| match ready {
            Readiness::Port => carte_api::Readiness::Port,
            Readiness::Http(path) => carte_api::Readiness::Http { path },
        })),
        boot_timeout_secs: limits.boot_timeout.as_secs(),
        paths: api(settings.paths),
        scope: api(settings.scope.map(|s| name(&s))),
        kind: api(settings.kind.map(|k| k.map(|k| name(&k)))),
        pty: api(settings.pty),
        after: api(settings.after),
        preset: api(settings.preset.map(|p| p.map(|p| p.name().to_string()))),
        restart: api(settings.restart.map(|r| name(&r))),
        always_restarted,
        gives_up_after: 5,
        crash_window_secs: limits.crash_window.as_secs(),
        stop_signal: "TERM".into(),
        stop_grace_secs: limits.stop_grace.as_secs(),
        policy: api(settings.policy.map(policy)),
        runtime_policy: status.filter(|s| s.temporary).map(|s| s.policy),
    }
}

fn kube_project(world: &World, address: &KubeAddress) -> anyhow::Result<ResolvedKube> {
    let project = world
        .kube_project(&address.project)
        .ok_or_else(|| anyhow!("no kube project {}", address.project))?;
    let idle = project.idle_timeout.unwrap_or(world.machine.idle_timeout);
    let forwards = project
        .services
        .iter()
        .filter(|(name, _)| address.service.as_ref().is_none_or(|s| s == *name))
        .map(|(name, service)| KubeForward {
            service: name.clone(),
            aliases: Setting {
                value: service.aliases.clone(),
                default: Vec::new(),
            },
            namespace: Setting {
                value: service.namespace.clone(),
                default: None,
            },
            service_port: Setting {
                value: service.service_port.as_ref().map(ToString::to_string),
                default: None,
            },
            policy: Setting {
                value: policy(project.policy(name)),
                default: carte_api::Policy::Auto,
            },
        })
        .collect();
    Ok(ResolvedKube {
        name: project.name.clone(),
        config_path: project.config_path.clone(),
        context: project.context.clone(),
        kubeconfig: Setting {
            value: project.kubeconfig.clone(),
            default: None,
        },
        namespaces: Setting {
            value: project.namespaces.clone(),
            default: Vec::new(),
        },
        namespace: address.namespace.clone(),
        service: address.service.clone(),
        env_provider: Setting {
            value: project.env_provider.map(|p| name(&p)),
            default: None,
        },
        idle_timeout_secs: Setting {
            value: idle.as_secs(),
            default: world.machine.idle_timeout.as_secs(),
        },
        forwards,
    })
}

/// The processes the snapshot shows for an instance.
fn statuses<'a>(snapshot: &'a Snapshot, key: &InstanceKey) -> Vec<&'a ProcessStatus> {
    snapshot
        .projects
        .iter()
        .filter(|p| p.name == key.project)
        .flat_map(|p| &p.stacks)
        .filter(|s| s.name == key.stack.slug())
        .flat_map(|s| &s.instances)
        .filter(|i| i.app == key.app)
        .flat_map(|i| &i.processes)
        .collect()
}

fn api<T>(setting: resolved::Setting<T>) -> Setting<T> {
    Setting {
        value: setting.value,
        default: setting.default,
    }
}

fn templates<'a>(env: impl IntoIterator<Item = &'a EnvTemplate>) -> Vec<carte_api::EnvTemplate> {
    env.into_iter()
        .map(|t| carte_api::EnvTemplate {
            key: t.key.clone(),
            value: t.value.clone(),
        })
        .collect()
}

fn policy(policy: Policy) -> carte_api::Policy {
    match policy {
        Policy::Pinned => carte_api::Policy::Pinned,
        Policy::Auto => carte_api::Policy::Auto,
        Policy::Disabled => carte_api::Policy::Disabled,
    }
}

/// A unit enum's name as configuration spells it.
fn name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn source_name(source: ProcessSource) -> &'static str {
    match source {
        ProcessSource::ProcfileDev => "Procfile.dev",
        ProcessSource::Procfile => "Procfile",
        ProcessSource::ConfigRu => "config.ru",
        ProcessSource::PackageJson => "package.json",
        ProcessSource::Config => "config",
        ProcessSource::RouteOnly => "port",
        ProcessSource::ServeFiles => "serve_files",
    }
}
