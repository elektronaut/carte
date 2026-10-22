//! The settings carte resolved for a project, app or process, each with the value it
//! would have if the configuration left it out, so a view can tell which ones differ.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{
    App, AppRef, EnvProviderKind, MachineConfig, Policy, Preset, ProcessKind, ProcessSpec, Project,
    Readiness, Restart, Scheme, Scope,
};
use crate::procfile::{self, Notice, ProcessSource, ProcfileError};

/// A resolved setting, and its default: the built-in value, or the project's or the
/// machine's setting it overrides. A setting written out with its default value
/// doesn't differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub default: T,
}

impl<T: PartialEq> Setting<T> {
    pub fn new(value: T, default: T) -> Self {
        Setting { value, default }
    }

    pub fn differs(&self) -> bool {
        self.value != self.default
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> Setting<U> {
        Setting {
            value: f(self.value),
            default: f(self.default),
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
    ProjectSettings {
        idle_timeout: Setting::new(
            project.idle_timeout.unwrap_or(machine.idle_timeout),
            machine.idle_timeout,
        ),
        teardown_delay: Setting::new(
            project.teardown_delay.unwrap_or(machine.teardown_delay),
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
    AppSettings {
        idle_timeout: self::project(machine, project).idle_timeout,
        aliases: Setting::new(settings.aliases.clone(), Vec::new()),
        subdomains: Setting::new(settings.subdomains, false),
        policy: Setting::new(settings.policy.unwrap_or_default(), Policy::default()),
        port: Setting::new(settings.port, None),
        serve_files: Setting::new(settings.serve_files.clone(), project.serve_files.clone()),
        warm: Setting::new(!settings.never_warm, true),
        env_files: Setting::new(settings.env_files.clone(), Vec::new()),
        depends_on: Setting::new(depends_on, Vec::new()),
    }
}

/// A process's settings, over what its Procfile line and the app give it.
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
}

impl ProcessSettings {
    fn new(spec: &ProcessSpec, default: &ProcessSpec, notices: &[Notice]) -> Self {
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
            policy: Setting::new(spec.policy, default.policy),
        }
    }
}

/// An app's processes in a checkout, teardowns last, and where their list comes from.
pub fn processes(
    checkout: &Path,
    settings: &crate::model::AppSettings,
) -> Result<(ProcessSource, Vec<ProcessSettings>), ProcfileError> {
    let list = procfile::load(checkout, settings)?;
    let processes = list
        .processes
        .iter()
        .chain(&list.teardown)
        .map(|spec| {
            let default = procfile::unconfigured(checkout, settings, &spec.name);
            ProcessSettings::new(spec, default.as_ref().unwrap_or(spec), &list.notices)
        })
        .collect();
    Ok((list.source, processes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessOverride;
    use crate::test_support;

    fn nova() -> (MachineConfig, Project) {
        let machine = MachineConfig::default();
        (machine, test_support::nova().project)
    }

    #[test]
    fn a_project_setting_differs_from_the_machine_only_when_its_value_does() {
        let (machine, mut project) = nova();
        let settings = super::project(&machine, &project);
        assert!(!settings.idle_timeout.differs());
        assert_eq!(settings.idle_timeout.value, machine.idle_timeout);

        project.idle_timeout = Some(machine.idle_timeout);
        let settings = super::project(&machine, &project);
        assert!(
            !settings.idle_timeout.differs(),
            "set to the machine's value"
        );

        project.idle_timeout = Some(Duration::from_secs(60));
        project.scheme = Scheme::Http;
        let settings = super::project(&machine, &project);
        assert!(settings.idle_timeout.differs());
        assert_eq!(settings.idle_timeout.default, machine.idle_timeout);
        assert!(settings.scheme.differs());
        assert!(!settings.teardown_delay.differs());
    }

    #[test]
    fn an_app_setting_differs_from_its_project_and_the_built_in_default() {
        let (machine, mut project) = nova();
        project.serve_files = Some("public".into());
        let mut billing = project.app("nova-billing").unwrap().clone();
        billing.settings.serve_files = Some("public".into());
        let settings = app(&machine, &project, &billing);
        assert!(
            !settings.serve_files.differs(),
            "set to the project's value"
        );

        billing.settings.serve_files = Some("static".into());
        billing.settings.policy = Some(Policy::Auto);
        billing.settings.aliases = vec!["pay".into()];
        let settings = app(&machine, &project, &billing);
        assert!(settings.serve_files.differs());
        assert_eq!(settings.serve_files.default, Some("public".into()));
        assert!(!settings.policy.differs(), "set to the default");
        assert!(settings.aliases.differs());
        assert!(!settings.warm.differs());

        project.idle_timeout = Some(Duration::from_secs(60));
        let settings = app(&machine, &project, &billing);
        assert!(settings.idle_timeout.differs(), "the project's");
    }

    #[test]
    fn a_process_setting_differs_from_its_procfile_line_and_the_app() {
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
        settings.processes.entries.insert(
            "worker".into(),
            ProcessOverride {
                restart: Some(Restart::OnFailure),
                pty: Some(false),
                ..Default::default()
            },
        );
        settings.processes.entries.insert(
            "css".into(),
            ProcessOverride {
                policy: Some(Policy::Disabled),
                ..Default::default()
            },
        );
        settings.processes.entries.insert(
            "mail".into(),
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
        assert_eq!(web.policy.value, Policy::Pinned);
        assert!(!web.policy.differs(), "the app's policy is its default");

        let worker = by_name("worker");
        assert!(!worker.restart.differs(), "on-failure, set, is the default");
        assert!(worker.pty.differs());
        assert!(!worker.policy.differs());

        let css = by_name("css");
        assert_eq!(css.policy.value, Policy::Disabled);
        assert_eq!(css.policy.default, Policy::Pinned);
        assert!(css.policy.differs());

        let mail = by_name("mail");
        assert!(
            !mail.command.differs(),
            "a process only config has: its command is its own"
        );
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
