//! The settings carte resolved for a project, app or process, each with its built-in
//! default and the value it inherits, so a view can tell which ones differ.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{
    App, AppRef, EnvProviderKind, EnvTemplate, MachineConfig, Policy, Preset, ProcessKind,
    ProcessSpec, Project, Readiness, Restart, Scheme, Scope,
};
use crate::procfile::{self, Notice, ProcessSource, ProcfileError};

/// A resolved setting, with carte's built-in default and what it inherits from the
/// layer above: the machine for a project, the project or the machine for an app,
/// the app for a process. Without such a layer, it inherits the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub default: T,
    pub inherited: T,
}

impl<T: PartialEq + Clone> Setting<T> {
    /// A setting with nothing to inherit but its default.
    pub fn new(value: T, default: T) -> Self {
        Setting {
            value,
            inherited: default.clone(),
            default,
        }
    }

    pub fn inheriting(value: T, default: T, inherited: T) -> Self {
        Setting {
            value,
            default,
            inherited,
        }
    }

    /// Someone configured it: it isn't carte's default, or it isn't what the layer
    /// above gives it.
    pub fn differs(&self) -> bool {
        self.value != self.default || self.value != self.inherited
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> Setting<U> {
        Setting {
            value: f(self.value),
            default: f(self.default),
            inherited: f(self.inherited),
        }
    }
}

/// A project's own settings, over the machine's.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSettings {
    pub idle_timeout: Setting<Duration>,
    pub teardown_delay: Setting<Duration>,
    pub scheme: Setting<Scheme>,
    /// `None` detects the provider in each checkout.
    pub env_provider: Setting<Option<EnvProviderKind>>,
    /// The default `serve_files` of its apps.
    pub serve_files: Setting<Option<PathBuf>>,
}

pub fn project(machine: &MachineConfig, project: &Project) -> ProjectSettings {
    let builtin = MachineConfig::default();
    ProjectSettings {
        idle_timeout: Setting::inheriting(
            project.idle_timeout.unwrap_or(machine.idle_timeout),
            builtin.idle_timeout,
            machine.idle_timeout,
        ),
        teardown_delay: Setting::inheriting(
            project.teardown_delay.unwrap_or(machine.teardown_delay),
            builtin.teardown_delay,
            machine.teardown_delay,
        ),
        scheme: Setting::new(project.scheme, Scheme::default()),
        env_provider: Setting::new(project.env_provider, None),
        serve_files: Setting::new(project.serve_files.clone(), None),
    }
}

/// An app's settings, over its project's and the machine's.
#[derive(Debug, Clone, PartialEq)]
pub struct AppSettings {
    pub idle_timeout: Setting<Duration>,
    pub aliases: Setting<Vec<String>>,
    pub subdomains: Setting<bool>,
    /// The policy of its processes, unless a process sets its own.
    pub policy: Setting<Policy>,
    /// The fixed port of an app that only routes.
    pub port: Setting<Option<u16>>,
    pub serve_files: Setting<Option<PathBuf>>,
    /// May start ahead of a request.
    pub warm: Setting<bool>,
    pub env_files: Setting<Vec<String>>,
    /// Apps of its own project first, then of other projects.
    pub depends_on: Setting<Vec<AppRef>>,
    /// The project's env templates, then the app's, one per key.
    pub env: Vec<EnvTemplate>,
}

pub fn app(machine: &MachineConfig, project: &Project, app: &App) -> AppSettings {
    let settings = &app.settings;
    let depends_on = settings
        .depends_on
        .iter()
        .map(|name| AppRef {
            project: project.name.clone(),
            app: name.clone(),
        })
        .chain(settings.depends_on_elsewhere.iter().cloned())
        .collect();
    let mut env: Vec<EnvTemplate> = Vec::new();
    for template in project.env.iter().chain(&settings.env) {
        match env.iter_mut().find(|t| t.key == template.key) {
            Some(existing) => existing.value = template.value.clone(),
            None => env.push(template.clone()),
        }
    }
    AppSettings {
        idle_timeout: Setting::inheriting(
            project.idle_timeout.unwrap_or(machine.idle_timeout),
            MachineConfig::default().idle_timeout,
            machine.idle_timeout,
        ),
        aliases: Setting::new(settings.aliases.clone(), Vec::new()),
        subdomains: Setting::new(settings.subdomains, false),
        policy: Setting::new(settings.policy.unwrap_or_default(), Policy::default()),
        port: Setting::new(settings.port, None),
        serve_files: Setting::inheriting(
            settings.serve_files.clone(),
            None,
            project.serve_files.clone(),
        ),
        warm: Setting::new(!settings.never_warm, true),
        env_files: Setting::new(settings.env_files.clone(), Vec::new()),
        depends_on: Setting::new(depends_on, Vec::new()),
        env,
    }
}

/// A process's settings, over what its Procfile line gives it and its app's policy.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSettings {
    pub name: String,
    pub command: Setting<String>,
    /// The command as written, when carte replaced a hardcoded port in it with `$PORT`.
    pub rewritten_from: Option<String>,
    /// Relative to the checkout unless absolute.
    pub cwd: Setting<Option<PathBuf>>,
    pub http: Setting<bool>,
    pub ready: Setting<Readiness>,
    pub paths: Setting<Vec<String>>,
    pub scope: Setting<Scope>,
    pub kind: Setting<Option<ProcessKind>>,
    pub pty: Setting<bool>,
    pub after: Setting<Vec<String>>,
    pub preset: Setting<Option<Preset>>,
    pub restart: Setting<Restart>,
    pub policy: Setting<Policy>,
    /// Its own env templates, over the app's.
    pub env: Vec<EnvTemplate>,
    /// Env templates it sets on every process of the instance.
    pub provides: Vec<EnvTemplate>,
    /// The supervisor waits for it to become ready, or for a task to finish, within
    /// the boot timeout.
    pub timed: bool,
    /// Why its defaults are unknown; each setting then has its value as defaults.
    pub defaults_error: Option<String>,
}

impl ProcessSettings {
    fn new(
        spec: &ProcessSpec,
        defaults: Result<ProcessSpec, String>,
        app_policy: Policy,
        notices: &[Notice],
    ) -> Self {
        let (default, inherited_policy, defaults_error) = match defaults {
            Ok(default) => (default, app_policy, None),
            Err(err) => (spec.clone(), spec.policy, Some(err)),
        };
        ProcessSettings {
            name: spec.name.clone(),
            command: Setting::new(spec.command.clone(), default.command.clone()),
            rewritten_from: notices.iter().find_map(|notice| match notice {
                Notice::PortRewritten { process, from, .. } if *process == spec.name => {
                    Some(from.clone())
                }
                _ => None,
            }),
            cwd: Setting::new(spec.cwd.clone(), default.cwd.clone()),
            http: Setting::new(spec.http, default.http),
            ready: Setting::new(spec.ready.clone(), default.ready.clone()),
            paths: Setting::new(spec.paths.clone(), default.paths.clone()),
            scope: Setting::new(spec.scope, default.scope),
            kind: Setting::new(spec.kind, default.kind),
            pty: Setting::new(spec.pty, default.pty),
            after: Setting::new(spec.after.clone(), default.after.clone()),
            preset: Setting::new(spec.preset, default.preset),
            restart: Setting::new(spec.restart, default.restart),
            policy: Setting::inheriting(spec.policy, default.policy, inherited_policy),
            env: spec.env.clone(),
            provides: spec.provides.clone(),
            timed: spec.is_checked() || spec.is_task(),
            defaults_error,
        }
    }
}

/// An app's processes in a checkout, teardowns last, and where their list comes from.
pub fn processes(
    checkout: &Path,
    settings: &crate::model::AppSettings,
) -> Result<(ProcessSource, Vec<ProcessSettings>), ProcfileError> {
    let list = procfile::load(checkout, settings)?;
    let app_policy = settings.policy.unwrap_or_default();
    let processes = list
        .processes
        .iter()
        .chain(&list.teardown)
        .map(|spec| {
            let defaults = procfile::unconfigured(checkout, settings, &spec.name);
            ProcessSettings::new(spec, defaults, app_policy, &list.notices)
        })
        .collect();
    Ok((list.source, processes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessOverride;
    use crate::test_support;

    fn nova() -> Project {
        test_support::nova().project
    }

    /// A machine whose idle timeout isn't carte's default.
    fn machine_30m() -> MachineConfig {
        MachineConfig {
            idle_timeout: Duration::from_secs(30 * 60),
            ..MachineConfig::default()
        }
    }

    #[test]
    fn a_project_setting_differs_from_the_default_or_the_machine() {
        let builtin = MachineConfig::default().idle_timeout;
        let idle = |machine: &MachineConfig, project: Option<Duration>| {
            let mut nova = nova();
            nova.idle_timeout = project;
            super::project(machine, &nova).idle_timeout
        };
        let default = MachineConfig::default();
        assert!(!idle(&default, None).differs(), "unset everywhere");
        assert!(
            !idle(&default, Some(builtin)).differs(),
            "set to the default, which the machine keeps"
        );
        let machine = machine_30m();
        let inherited = idle(&machine, None);
        assert_eq!(inherited.value, machine.idle_timeout);
        assert!(inherited.differs(), "the machine's own value");
        assert!(idle(&machine, Some(machine.idle_timeout)).differs());
        assert!(
            idle(&machine, Some(builtin)).differs(),
            "the default, over the machine's"
        );
        assert!(idle(&default, Some(Duration::from_secs(60))).differs());

        let mut nova = nova();
        nova.scheme = Scheme::Http;
        let settings = super::project(&default, &nova);
        assert!(settings.scheme.differs());
        assert!(!settings.teardown_delay.differs());
    }

    #[test]
    fn an_app_setting_differs_from_the_default_or_its_project() {
        let machine = MachineConfig::default();
        let mut project = nova();
        let mut billing = project.app("nova-billing").unwrap().clone();
        billing.settings.policy = Some(Policy::Auto);
        billing.settings.aliases = vec!["pay".into()];
        let settings = app(&machine, &project, &billing);
        assert!(!settings.policy.differs(), "set to the default");
        assert!(!settings.serve_files.differs(), "unset");
        assert!(settings.aliases.differs());
        assert!(!settings.warm.differs());
        assert!(!settings.idle_timeout.differs());

        project.serve_files = Some("public".into());
        billing.settings.serve_files = Some("public".into());
        let settings = app(&machine, &project, &billing);
        assert!(
            settings.serve_files.differs(),
            "the project's, which isn't the default"
        );
        billing.settings.serve_files = None;
        let settings = app(&machine, &project, &billing);
        assert!(
            settings.serve_files.differs(),
            "nothing, under a project that serves files"
        );

        let settings = app(&machine_30m(), &project, &billing);
        assert!(settings.idle_timeout.differs(), "the machine's");
    }

    #[test]
    fn templates_are_one_per_key_the_apps_last() {
        let machine = MachineConfig::default();
        let mut project = nova();
        let template = |key: &str, value: &str| EnvTemplate {
            key: key.into(),
            value: value.into(),
        };
        project.env = vec![template("A", "1"), template("B", "2")];
        let mut billing = project.app("nova-billing").unwrap().clone();
        billing.settings.env = vec![template("B", "3"), template("C", "4")];
        let env = app(&machine, &project, &billing).env;
        assert_eq!(
            env,
            [template("A", "1"), template("B", "3"), template("C", "4")]
        );
    }

    #[test]
    fn a_process_setting_differs_from_its_procfile_line_or_its_app() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Procfile.dev"),
            "web: bin/rails server -p 3000\nworker: bin/jobs\ncss: bin/rails tailwindcss:watch\n",
        )
        .unwrap();
        let mut settings = crate::model::AppSettings {
            policy: Some(Policy::Pinned),
            ..Default::default()
        };
        let mut configure = |name: &str, ov: ProcessOverride| {
            settings.processes.entries.insert(name.into(), ov);
        };
        configure(
            "worker",
            ProcessOverride {
                restart: Some(Restart::OnFailure),
                pty: Some(false),
                ..Default::default()
            },
        );
        configure(
            "css",
            ProcessOverride {
                policy: Some(Policy::Auto),
                ..Default::default()
            },
        );
        configure(
            "mail",
            ProcessOverride {
                command: Some("bin/mailcatcher".into()),
                ..Default::default()
            },
        );
        let (source, processes) = processes(dir.path(), &settings).unwrap();
        assert_eq!(source, ProcessSource::ProcfileDev);
        let by_name = |name: &str| processes.iter().find(|p| p.name == name).unwrap();

        let web = by_name("web");
        assert_eq!(web.command.value, "bin/rails server -p $PORT");
        assert_eq!(
            web.rewritten_from.as_deref(),
            Some("bin/rails server -p 3000")
        );
        assert!(!web.command.differs());
        assert!(web.http.value && !web.http.differs());
        assert!(web.timed);
        assert_eq!(web.defaults_error, None);
        assert_eq!(web.policy.value, Policy::Pinned);
        assert!(
            web.policy.differs(),
            "the app's pin, which isn't the default"
        );

        let worker = by_name("worker");
        assert!(!worker.restart.differs(), "on-failure, set, is the default");
        assert!(worker.pty.differs());
        assert!(!worker.timed);

        let css = by_name("css");
        assert_eq!(css.policy.value, Policy::Auto);
        assert!(css.policy.differs(), "the default, over the app's pin");

        let mail = by_name("mail");
        assert!(
            !mail.command.differs(),
            "a process only config has: its command is its own"
        );
    }

    #[test]
    fn a_process_whose_defaults_cant_load_says_why() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Procfile.dev"),
            "web: bin/server\nvite: bin/vite dev\n",
        )
        .unwrap();
        let mut settings = crate::model::AppSettings::default();
        settings.processes.entries.insert(
            "vite".into(),
            ProcessOverride {
                preset: Some(Preset::None),
                pty: Some(false),
                ..Default::default()
            },
        );
        settings.processes.entries.insert(
            "assets".into(),
            ProcessOverride {
                command: Some("bin/assets".into()),
                paths: Some(vec!["/vite-dev/".into()]),
                ..Default::default()
            },
        );
        let (_, processes) = processes(dir.path(), &settings).unwrap();
        let vite = processes.iter().find(|p| p.name == "vite").unwrap();
        assert!(
            vite.defaults_error
                .as_deref()
                .is_some_and(|e| e.contains("is served by both")),
            "{vite:?}"
        );
        assert!(!vite.pty.differs(), "unknown, not different");
    }

    #[test]
    fn substitutes_the_port_the_way_the_shell_does() {
        let run = |command| procfile::substitute_port(command, 41207);
        assert_eq!(run("puma -p $PORT"), "puma -p 41207");
        assert_eq!(run("vite --port=${PORT}"), "vite --port=41207");
        assert_eq!(run("x ${PORT:-3000} $PORTS $$"), "x 41207 $PORTS $$");
        assert_eq!(run("echo $HOME"), "echo $HOME");
    }
}
