//! The config tab: what carte resolved for the selection. Settings that differ from
//! carte's default or what they inherit are bold.

use std::time::Duration;

use carte_api::{
    Policy, Readiness, ResolvedApp, ResolvedConfig, ResolvedHost, ResolvedKube, ResolvedProcess,
    ResolvedProject, Setting,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::app::App;
use super::tabs::ConfigTab;
use super::theme::Theme;

pub fn render(theme: &Theme, frame: &mut Frame, inner: Rect, app: &mut App) {
    let width = usize::from(inner.width).max(1);
    let lines: Vec<Line<'static>> = lines(theme, &app.config)
        .into_iter()
        .flat_map(|line| {
            let parts = line
                .spans
                .iter()
                .map(|span| (span.content.to_string(), line.style.patch(span.style)))
                .collect();
            super::view::wrap(parts, width, 10.min(width / 2))
        })
        .collect();
    let view = &mut app.config;
    view.height = usize::from(inner.height);
    view.scroll = view.scroll.min(lines.len().saturating_sub(view.height));
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(view.scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
}

pub fn lines(theme: &Theme, tab: &ConfigTab) -> Vec<Line<'static>> {
    let mut rows = Rows {
        theme,
        lines: Vec::new(),
    };
    if tab.subject.is_none() {
        rows.note("Select a project, stack, app or process to see its config.");
        return rows.lines;
    }
    let config = match &tab.config {
        None => {
            rows.note("Resolving…");
            return rows.lines;
        }
        Some(Err(message)) => {
            rows.lines.push(Line::from(message.clone()).fg(theme.error));
            return rows.lines;
        }
        Some(Ok(config)) => config,
    };
    match config.as_ref() {
        ResolvedConfig::Project(project) => rows.project(project),
        ResolvedConfig::App(app) => rows.app(app),
        ResolvedConfig::Process(process) => rows.process(process),
        ResolvedConfig::Kube(kube) => rows.kube(kube),
        ResolvedConfig::Unknown => rows.note("This daemon sends a config this carte doesn't know."),
    }
    rows.lines
}

/// A value, bold when it differs from its default or what it inherits.
fn value(text: impl Into<String>, differs: bool) -> Span<'static> {
    let span = Span::raw(text.into());
    if differs { span.bold() } else { span }
}

fn setting<T>(setting: &Setting<T>, text: impl Into<String>) -> Span<'static> {
    value(text, setting.differs)
}

fn secs(secs: u64) -> String {
    carte_core::duration::format(Duration::from_secs(secs))
}

fn policy_words(policy: Policy) -> &'static str {
    match policy {
        Policy::Pinned => "pinned",
        Policy::Auto => "on demand",
        Policy::Disabled => "disabled",
        Policy::Unknown => "unknown",
    }
}

fn list(items: &[String]) -> String {
    items.join(", ")
}

struct Rows<'t> {
    theme: &'t Theme,
    lines: Vec<Line<'static>>,
}

impl Rows<'_> {
    fn row(&mut self, label: &str, value: Vec<Span<'static>>) {
        let mut spans = vec![Span::raw(format!("{label:<10}")).fg(self.theme.muted)];
        spans.extend(value);
        self.lines.push(Line::from(spans));
    }

    fn muted(&self, text: impl Into<String>) -> Span<'static> {
        Span::raw(text.into()).fg(self.theme.muted)
    }

    fn note(&mut self, text: &str) {
        self.lines
            .push(Line::from(text.to_string()).fg(self.theme.muted));
    }

    fn gap(&mut self) {
        self.lines.push(Line::default());
    }

    fn project(&mut self, project: &ResolvedProject) {
        let mut file = vec![Span::raw(project.config_path.display().to_string())];
        if project.from_template {
            file.push(self.muted(" (from a template)"));
        }
        self.row("file", file);
        if let Some(prefix) = &project.strip_prefix {
            self.row("prefix", vec![value(format!("{prefix} stripped"), true)]);
        }
        let mut env = vec![setting(
            &project.env_provider,
            project
                .env_provider
                .value
                .clone()
                .unwrap_or_else(|| "detected".into()),
        )];
        if !project.env.is_empty() {
            env.push(Span::raw(format!(" · {} templates", project.env.len())));
        }
        self.row("env", env);
        if !project.requires.is_empty() {
            self.row("requires", vec![value(list(&project.requires), true)]);
        }
        self.row(
            "scheme",
            vec![setting(&project.scheme, project.scheme.value.clone())],
        );
        self.row(
            "idle",
            vec![setting(
                &project.idle_timeout_secs,
                secs(project.idle_timeout_secs.value),
            )],
        );
        self.row(
            "teardown",
            vec![setting(
                &project.teardown_delay_secs,
                format!(
                    "{} after a stack's worktrees are gone",
                    secs(project.teardown_delay_secs.value)
                ),
            )],
        );
        if project.serve_files.value.is_some() || project.serve_files.differs {
            self.row("files", files(&project.serve_files));
        }
        for stack in &project.stacks {
            self.gap();
            let mut first = vec![Span::raw(stack.name.clone()).bold()];
            if let Some(branch) = &stack.branch {
                first.push(self.muted(format!(" · branch {branch}")));
            }
            self.lines.push(Line::from(first));
            for checkout in &stack.checkouts {
                let mut spans = vec![Span::raw(checkout.app.clone())];
                if checkout.checkout != stack.name {
                    spans.push(self.muted(format!(" from {}", checkout.checkout)));
                }
                spans.push(self.muted(format!("  {}", checkout.path.display())));
                self.row("", spans);
            }
        }
    }

    fn app(&mut self, app: &ResolvedApp) {
        self.row("project", vec![Span::raw(app.project.clone())]);
        let checkout = if app.checkout != app.stack {
            format!("{} (no worktree for {})", app.checkout, app.stack)
        } else {
            app.checkout.clone()
        };
        self.row(
            "checkout",
            vec![
                Span::raw(checkout),
                self.muted(format!("  {}", app.checkout_path.display())),
            ],
        );
        let mut label = "hostnames";
        let own: Vec<&ResolvedHost> = app.hostnames.iter().filter(|h| !h.alias).collect();
        for (name, host) in merged(&own) {
            let mut spans = vec![Span::raw(name)];
            if host.stack != app.stack {
                spans.push(self.muted(format!(" → {} (fallback for other stacks)", host.stack)));
            }
            self.row(label, spans);
            label = "";
        }
        if app.subdomains.value || app.subdomains.differs {
            let text = if app.subdomains.value {
                "and every name under them"
            } else {
                "none under them"
            };
            self.row("", vec![setting(&app.subdomains, text)]);
        }
        let aliases: Vec<&ResolvedHost> = app.hostnames.iter().filter(|h| h.alias).collect();
        let aliases: Vec<String> = merged(&aliases).into_iter().map(|(name, _)| name).collect();
        if !aliases.is_empty() || app.aliases.differs {
            self.row("aliases", vec![setting(&app.aliases, list(&aliases))]);
        }
        let env = &app.env;
        let mut spans = vec![setting(
            &env.provider,
            env.provider.value.clone().unwrap_or(env.detected.clone()),
        )];
        if !env.files.value.is_empty() {
            spans.push(Span::raw(" · "));
            spans.push(setting(&env.files, list(&env.files.value)));
        }
        if !env.templates.is_empty() {
            spans.push(Span::raw(format!(" · {} templates", env.templates.len())));
        }
        self.row("env", spans);
        self.row(
            "idle",
            vec![setting(
                &app.idle_timeout_secs,
                secs(app.idle_timeout_secs.value),
            )],
        );
        if !app.depends_on.value.is_empty() {
            let names: Vec<String> = app
                .depends_on
                .value
                .iter()
                .map(|dep| {
                    if dep.project == app.project {
                        dep.app.clone()
                    } else {
                        format!("{} ({})", dep.app, dep.project)
                    }
                })
                .collect();
            self.row("depends", vec![setting(&app.depends_on, list(&names))]);
        }
        if app.serve_files.value.is_some() || app.serve_files.differs {
            self.row("files", files(&app.serve_files));
        }
        if app.policy.differs {
            self.row(
                "policy",
                vec![setting(&app.policy, policy_words(app.policy.value))],
            );
        }
        if let Some(port) = app.port.value {
            self.row("port", vec![setting(&app.port, format!("{port}, fixed"))]);
        }
        if app.warm.differs {
            let text = if app.warm.value {
                "ahead of a request"
            } else {
                "never"
            };
            self.row("warm", vec![setting(&app.warm, text)]);
        }
        if let Some(error) = &app.process_error {
            self.gap();
            self.lines
                .push(Line::from(error.clone()).fg(self.theme.error));
            return;
        }
        if !app.processes.is_empty() {
            self.gap();
        }
        for process in &app.processes {
            self.row(
                &process.name,
                vec![setting(&process.command, process.command.value.clone())],
            );
            let mut facts: Vec<Span<'static>> = Vec::new();
            if let Some(port) = process.port {
                facts.push(Span::raw(format!("port {port}")));
            }
            if let Readiness::Http { path } = &process.ready.value {
                facts.push(setting(&process.ready, format!("ready at GET {path}")));
            }
            if process.policy.value != Policy::Auto || process.policy.differs {
                facts.push(setting(&process.policy, policy_words(process.policy.value)));
            }
            if let Some(runtime) = process.runtime_policy {
                facts.push(
                    Span::raw(format!(
                        "{} until the daemon restarts",
                        policy_words(runtime)
                    ))
                    .fg(self.theme.warn),
                );
            }
            if !facts.is_empty() {
                let mut spans = Vec::new();
                for (i, fact) in facts.into_iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw(" · "));
                    }
                    spans.push(fact);
                }
                self.row("", spans);
            }
        }
    }

    fn process(&mut self, process: &ResolvedProcess) {
        self.row(
            "command",
            vec![setting(&process.command, process.command.value.clone())],
        );
        if let Some(runs_as) = &process.runs_as {
            self.row("runs as", vec![Span::raw(runs_as.clone())]);
        }
        self.row(
            "cwd",
            vec![setting(&process.cwd, process.dir.display().to_string())],
        );
        match (process.port, process.rewritten_from_port) {
            (Some(port), Some(from)) => self.row(
                "port",
                vec![Span::raw(format!("{port} (PORT, rewritten from {from})"))],
            ),
            (Some(port), None) => self.row("port", vec![Span::raw(format!("{port} (PORT)"))]),
            (None, Some(from)) => self.row(
                "port",
                vec![self.muted(format!(
                    "given as PORT when it starts, rewritten from {from}"
                ))],
            ),
            (None, None) => {}
        }
        if let Some(timeout) = process.boot_timeout_secs {
            let check = match &process.ready.value {
                Readiness::Port => "its port is open".to_string(),
                Readiness::Http { path } => format!("GET {path}"),
                Readiness::Unknown => "unknown".to_string(),
            };
            let within = Span::raw(format!(" within {}", secs(timeout)));
            if process.kind.value.as_deref() == Some("task") {
                self.row("ready", vec![Span::raw("finishes"), within]);
            } else {
                self.row("ready", vec![setting(&process.ready, check), within]);
            }
        }
        if process.http.value {
            self.row(
                "paths",
                vec![setting(
                    &process.http,
                    "/ (everything not claimed by another process)",
                )],
            );
        } else if !process.paths.value.is_empty() || process.paths.differs {
            self.row(
                "paths",
                vec![setting(&process.paths, list(&process.paths.value))],
            );
        }
        if let Some(kind) = &process.kind.value {
            self.row("kind", vec![setting(&process.kind, kind.clone())]);
        }
        if process.scope.value != "instance" || process.scope.differs {
            let text = if process.scope.value == "checkout" {
                "once per checkout".to_string()
            } else {
                format!("once per {}", process.scope.value)
            };
            self.row("scope", vec![setting(&process.scope, text)]);
        }
        if !process.after.value.is_empty() {
            self.row(
                "after",
                vec![setting(&process.after, list(&process.after.value))],
            );
        }
        if let Some(preset) = &process.preset.value {
            self.row("preset", vec![setting(&process.preset, preset.clone())]);
        }
        if !process.pty.value || process.pty.differs {
            let text = if process.pty.value {
                "on"
            } else {
                "off, pipes"
            };
            self.row("pty", vec![setting(&process.pty, text)]);
        }
        let keys = |templates: &[carte_api::EnvTemplate]| {
            templates
                .iter()
                .map(|t| t.key.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !process.env.is_empty() {
            self.row("env", vec![value(keys(&process.env), true)]);
        }
        if !process.provides.is_empty() {
            self.row("provides", vec![value(keys(&process.provides), true)]);
        }
        self.row(
            "stop",
            vec![Span::raw(format!(
                "{}, KILL after {}",
                process.stop_signal,
                secs(process.stop_grace_secs)
            ))],
        );
        let gives_up = format!(
            "gives up after {} in {}",
            process.gives_up_after,
            secs(process.crash_window_secs)
        );
        let restart = if process.always_restarted {
            Span::raw(format!("whenever it exits, {gives_up}"))
        } else if process.restart.value == "on-failure" {
            setting(&process.restart, format!("on crash, {gives_up}"))
        } else {
            setting(&process.restart, "never")
        };
        self.row("restart", vec![restart]);
        let mut policy = vec![setting(&process.policy, policy_words(process.policy.value))];
        if let Some(runtime) = process.runtime_policy {
            policy.push(Span::raw(" · "));
            policy.push(
                Span::raw(format!(
                    "{} until the daemon restarts",
                    policy_words(runtime)
                ))
                .fg(self.theme.warn),
            );
        }
        self.row("policy", policy);
        if let Some(error) = &process.defaults_error {
            self.gap();
            self.note(&format!("Its defaults are unknown: {error}"));
        }
    }

    fn kube(&mut self, kube: &ResolvedKube) {
        self.row(
            "file",
            vec![Span::raw(kube.config_path.display().to_string())],
        );
        self.row("context", vec![Span::raw(kube.context.clone())]);
        if let Some(path) = &kube.kubeconfig.value {
            self.row(
                "config",
                vec![setting(&kube.kubeconfig, path.display().to_string())],
            );
        }
        let namespaces = if kube.namespaces.value.is_empty() {
            "every namespace".to_string()
        } else {
            list(&kube.namespaces.value)
        };
        self.row("looks in", vec![setting(&kube.namespaces, namespaces)]);
        if let Some(namespace) = &kube.namespace {
            self.row("namespace", vec![Span::raw(namespace.clone())]);
        }
        if let Some(provider) = &kube.env_provider.value {
            self.row("env", vec![setting(&kube.env_provider, provider.clone())]);
        }
        self.row(
            "idle",
            vec![setting(
                &kube.idle_timeout_secs,
                secs(kube.idle_timeout_secs.value),
            )],
        );
        if !kube.forwards.is_empty() {
            self.gap();
        }
        for forward in &kube.forwards {
            let mut facts = Vec::new();
            if let Some(namespace) = &forward.namespace.value {
                facts.push(setting(&forward.namespace, format!("in {namespace}")));
            }
            if let Some(port) = &forward.service_port.value {
                facts.push(setting(&forward.service_port, format!("port {port}")));
            }
            if !forward.aliases.value.is_empty() {
                facts.push(setting(
                    &forward.aliases,
                    format!("aliases {}", list(&forward.aliases.value)),
                ));
            }
            if forward.policy.differs {
                facts.push(setting(&forward.policy, policy_words(forward.policy.value)));
            }
            let mut spans = Vec::new();
            for (i, fact) in facts.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" · "));
                }
                spans.push(fact);
            }
            self.row(&forward.service, spans);
        }
    }
}

/// Each host once, its `localhost` twin on the same line: `billing.nova.test · .localhost`.
fn merged<'a>(hosts: &[&'a ResolvedHost]) -> Vec<(String, &'a ResolvedHost)> {
    let twin = |host: &ResolvedHost, other: &ResolvedHost| {
        other.stack == host.stack
            && other
                .host
                .strip_suffix(".localhost")
                .is_some_and(|stem| host.host.starts_with(&format!("{stem}.")))
    };
    let mut out = Vec::new();
    for &host in hosts {
        let others = || hosts.iter().filter(|other| !std::ptr::eq(**other, host));
        if others().any(|other| twin(other, host)) {
            continue;
        }
        let mut name = host.host.clone();
        if others().any(|other| twin(host, other)) {
            name.push_str(" · .localhost");
        }
        out.push((name, host));
    }
    out
}

fn files(setting: &Setting<Option<std::path::PathBuf>>) -> Vec<Span<'static>> {
    let text = match &setting.value {
        Some(dir) => format!("{}/ served before the app", dir.display()),
        None => "none".to_string(),
    };
    vec![value(text, setting.differs)]
}

#[cfg(test)]
mod tests {
    use carte_api::{
        Dependency, EnvTemplate, KubeForward, ResolvedEnv, ResolvedHost, ResolvedStack,
        StackCheckout,
    };

    use super::*;

    fn plain<T: Clone>(value: T) -> Setting<T> {
        Setting {
            value: value.clone(),
            default: value.clone(),
            inherited: value,
            differs: false,
        }
    }

    fn set<T: Clone>(value: T, default: T) -> Setting<T> {
        Setting {
            value,
            inherited: default.clone(),
            default,
            differs: true,
        }
    }

    fn tab(config: ResolvedConfig) -> ConfigTab {
        ConfigTab {
            subject: Some(super::super::tree::NodeId::Project {
                project: "nova".into(),
            }),
            config: Some(Ok(Box::new(config))),
            ..ConfigTab::default()
        }
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The span holding `needle`, on the line that has it.
    fn span<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| {
                panic!(
                    "no {needle:?} in {:#?}",
                    lines.iter().map(text).collect::<Vec<_>>()
                )
            })
    }

    fn bold(span: &Span) -> bool {
        span.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    }

    fn process(name: &str) -> ResolvedProcess {
        ResolvedProcess {
            name: name.into(),
            command: plain("bundle exec puma -p $PORT".into()),
            rewritten_from: Some("bundle exec puma -p 3000".into()),
            rewritten_from_port: Some(3000),
            runs_as: Some("bundle exec puma -p 41207".into()),
            cwd: plain(None),
            dir: "/work/nova-billing.feature-x".into(),
            port: Some(41207),
            http: plain(true),
            ready: set(Readiness::Http { path: "/up".into() }, Readiness::Port),
            boot_timeout_secs: Some(60),
            paths: plain(Vec::new()),
            scope: plain("instance".into()),
            kind: plain(None),
            pty: plain(true),
            after: plain(Vec::new()),
            preset: plain(None),
            restart: plain("never".into()),
            always_restarted: true,
            gives_up_after: 5,
            crash_window_secs: 60,
            stop_signal: "TERM".into(),
            stop_grace_secs: 10,
            policy: plain(Policy::Auto),
            runtime_policy: None,
            env: Vec::new(),
            provides: Vec::new(),
            defaults_error: None,
        }
    }

    fn app() -> ResolvedApp {
        let mut worker = process("worker");
        worker.command = plain("bundle exec sidekiq".into());
        worker.http = plain(false);
        worker.ready = plain(Readiness::Port);
        worker.port = None;
        worker.policy = Setting {
            value: Policy::Pinned,
            default: Policy::Auto,
            inherited: Policy::Auto,
            differs: true,
        };
        let mut css = process("css");
        css.command = plain("bin/rails tailwindcss:watch".into());
        css.http = plain(false);
        css.ready = plain(Readiness::Port);
        css.port = None;
        css.runtime_policy = Some(Policy::Disabled);
        ResolvedApp {
            project: "nova".into(),
            app: "nova-billing".into(),
            short: "billing".into(),
            stack: "feature-x".into(),
            checkout: "feature-x".into(),
            checkout_path: "/work/nova-billing.feature-x".into(),
            hostnames: vec![
                ResolvedHost {
                    host: "billing.feature-x.nova.test".into(),
                    stack: "feature-x".into(),
                    alias: false,
                },
                ResolvedHost {
                    host: "billing.nova.test".into(),
                    stack: "main".into(),
                    alias: false,
                },
                ResolvedHost {
                    host: "pay.test".into(),
                    stack: "main".into(),
                    alias: true,
                },
            ],
            aliases: set(vec!["pay".into()], Vec::new()),
            subdomains: plain(false),
            env: ResolvedEnv {
                provider: plain(None),
                detected: "mise".into(),
                files: set(vec![".env.development".into()], Vec::new()),
                templates: (0..4)
                    .map(|i| EnvTemplate {
                        key: format!("K{i}"),
                        value: "v".into(),
                    })
                    .collect(),
            },
            idle_timeout_secs: plain(900),
            depends_on: set(
                vec![Dependency {
                    project: "nova".into(),
                    app: "frontend".into(),
                }],
                Vec::new(),
            ),
            serve_files: Setting {
                value: Some("public".into()),
                default: None,
                inherited: Some("public".into()),
                differs: true,
            },
            policy: plain(Policy::Auto),
            port: plain(None),
            warm: plain(true),
            process_source: Some("Procfile.dev".into()),
            processes: vec![process("web"), worker, css],
            process_error: None,
        }
    }

    #[test]
    fn an_app_reads_like_the_issue() {
        let theme = Theme::default();
        let lines = lines(&theme, &tab(ResolvedConfig::App(Box::new(app()))));
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(texts[0], "project   nova");
        assert!(texts[1].starts_with("checkout  feature-x  /work/nova-billing.feature-x"));
        assert_eq!(texts[2], "hostnames billing.feature-x.nova.test");
        assert_eq!(
            texts[3],
            "          billing.nova.test → main (fallback for other stacks)"
        );
        assert!(
            texts.contains(&"aliases   pay.test".to_string()),
            "{texts:#?}"
        );
        assert!(
            texts.contains(&"env       mise · .env.development · 4 templates".to_string()),
            "{texts:#?}"
        );
        assert!(texts.contains(&"idle      15m".to_string()), "{texts:#?}");
        assert!(
            texts.contains(&"depends   frontend".to_string()),
            "{texts:#?}"
        );
        assert!(
            texts.contains(&"files     public/ served before the app".to_string()),
            "{texts:#?}"
        );
        assert!(texts.contains(&"web       bundle exec puma -p $PORT".to_string()));
        assert!(
            texts.contains(&"          port 41207 · ready at GET /up".to_string()),
            "{texts:#?}"
        );
        assert!(
            texts.contains(&"          pinned".to_string()),
            "{texts:#?}"
        );
        assert!(
            texts.contains(&"          disabled until the daemon restarts".to_string()),
            "{texts:#?}"
        );

        assert!(bold(span(&lines, "pay.test")));
        assert!(!bold(span(&lines, "mise")), "detected, not configured");
        assert!(bold(span(&lines, ".env.development")));
        assert!(!bold(span(&lines, "15m")));
        assert!(
            bold(span(&lines, "public/")),
            "the project's, not carte's default"
        );
        assert!(bold(span(&lines, "pinned")));
        assert!(!bold(span(&lines, "bundle exec sidekiq")));
        assert!(bold(span(&lines, "ready at GET /up")));
        assert!(!bold(span(&lines, "disabled until")), "runtime, not config");
    }

    #[test]
    fn a_process_reads_like_the_issue() {
        let theme = Theme::default();
        let lines = lines(
            &theme,
            &tab(ResolvedConfig::Process(Box::new(process("web")))),
        );
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(
            texts,
            [
                "command   bundle exec puma -p $PORT",
                "runs as   bundle exec puma -p 41207",
                "cwd       /work/nova-billing.feature-x",
                "port      41207 (PORT, rewritten from 3000)",
                "ready     GET /up within 1m",
                "paths     / (everything not claimed by another process)",
                "stop      TERM, KILL after 10s",
                "restart   whenever it exits, gives up after 5 in 1m",
                "policy    on demand",
            ]
        );
        assert!(bold(span(&lines, "GET /up")));
        assert!(
            !bold(span(&lines, " within 1m")),
            "the timeout isn't configurable"
        );
        assert!(!bold(span(&lines, "on demand")));
        assert!(!bold(span(&lines, "bundle exec puma -p $PORT")));
    }

    #[test]
    fn a_worker_has_no_readiness_and_its_runtime_pin_is_apart() {
        let theme = Theme::default();
        let mut worker = process("worker");
        worker.http = plain(false);
        worker.boot_timeout_secs = None;
        worker.port = None;
        worker.runs_as = None;
        worker.rewritten_from = None;
        worker.rewritten_from_port = None;
        worker.restart = plain("on-failure".into());
        worker.always_restarted = false;
        worker.runtime_policy = Some(Policy::Pinned);
        let lines = lines(&theme, &tab(ResolvedConfig::Process(Box::new(worker))));
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert!(!texts.iter().any(|t| t.starts_with("ready")), "{texts:#?}");
        assert!(!texts.iter().any(|t| t.starts_with("port")), "{texts:#?}");
        assert!(texts.contains(&"restart   on crash, gives up after 5 in 1m".to_string()));
        assert!(
            texts.contains(&"policy    on demand · pinned until the daemon restarts".to_string())
        );
    }

    #[test]
    fn a_project_lists_its_stacks_and_settings() {
        let theme = Theme::default();
        let project = ResolvedProject {
            name: "nova".into(),
            config_path: "/c/projects/nova.toml".into(),
            from_template: false,
            stack: None,
            stacks: vec![
                ResolvedStack {
                    name: "main".into(),
                    branch: None,
                    checkouts: vec![StackCheckout {
                        app: "nova-billing".into(),
                        checkout: "main".into(),
                        path: "/work/nova-billing".into(),
                    }],
                },
                ResolvedStack {
                    name: "feature-x".into(),
                    branch: Some("feature-x".into()),
                    checkouts: vec![StackCheckout {
                        app: "nova-billing".into(),
                        checkout: "main".into(),
                        path: "/work/nova-billing".into(),
                    }],
                },
            ],
            strip_prefix: Some("nova-".into()),
            env_provider: plain(None),
            env: Vec::new(),
            requires: Vec::new(),
            scheme: plain("https".into()),
            idle_timeout_secs: Setting {
                value: 1800,
                default: 900,
                inherited: 1800,
                differs: true,
            },
            teardown_delay_secs: plain(600),
            serve_files: plain(None),
        };
        let lines = lines(&theme, &tab(ResolvedConfig::Project(project)));
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(texts[0], "file      /c/projects/nova.toml");
        assert!(texts.contains(&"idle      30m".to_string()), "{texts:#?}");
        assert!(
            bold(span(&lines, "30m")),
            "the machine's, not carte's default"
        );
        assert!(!bold(span(&lines, "https")));
        assert!(texts.contains(&"feature-x · branch feature-x".to_string()));
        assert!(
            texts.contains(&"          nova-billing from main  /work/nova-billing".to_string()),
            "{texts:#?}"
        );
    }

    #[test]
    fn a_kube_project_shows_its_context_and_forwards() {
        let theme = Theme::default();
        let kube = ResolvedKube {
            name: "prod".into(),
            config_path: "/c/projects/prod.toml".into(),
            context: "prod".into(),
            kubeconfig: plain(None),
            namespaces: set(vec!["monitoring".into()], Vec::new()),
            namespace: None,
            service: None,
            env_provider: plain(None),
            idle_timeout_secs: plain(900),
            forwards: vec![KubeForward {
                service: "grafana".into(),
                aliases: set(vec!["dash".into()], Vec::new()),
                namespace: plain(None),
                service_port: plain(None),
                policy: plain(Policy::Auto),
            }],
        };
        let lines = lines(&theme, &tab(ResolvedConfig::Kube(kube)));
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert!(texts.contains(&"context   prod".to_string()), "{texts:#?}");
        assert!(
            texts.contains(&"looks in  monitoring".to_string()),
            "{texts:#?}"
        );
        assert!(
            texts.contains(&"grafana   aliases dash".to_string()),
            "{texts:#?}"
        );
        assert!(bold(span(&lines, "monitoring")));
    }

    #[test]
    fn bold_follows_the_setting_the_text_comes_from() {
        let theme = Theme::default();
        let mut web = process("web");
        web.rewritten_from = Some("unset PORT && bin/rails s".into());
        web.rewritten_from_port = None;
        web.restart = set("never".into(), "never".into());
        let mut task = process("migrate");
        task.http = plain(false);
        task.always_restarted = false;
        task.kind = set(Some("task".into()), None);
        task.port = None;
        let web_lines = lines(&theme, &tab(ResolvedConfig::Process(Box::new(web))));
        let texts: Vec<String> = web_lines.iter().map(text).collect();
        assert!(
            texts.contains(&"port      41207 (PORT)".to_string()),
            "{texts:#?}"
        );
        assert!(
            !bold(span(&web_lines, "whenever it exits")),
            "carte restarts web whatever restart says"
        );
        let task_lines = lines(&theme, &tab(ResolvedConfig::Process(Box::new(task))));
        assert!(!bold(span(&task_lines, "finishes")));
    }

    #[test]
    fn a_host_and_its_localhost_twin_share_a_line() {
        let host = |host: &str, stack: &str| ResolvedHost {
            host: host.into(),
            stack: stack.into(),
            alias: false,
        };
        let hosts = [
            host("billing.feature-x.nova.test", "feature-x"),
            host("billing.feature-x.nova.localhost", "feature-x"),
            host("billing.nova.test", "main"),
            host("billing.nova.localhost", "main"),
        ];
        let refs: Vec<&ResolvedHost> = hosts.iter().collect();
        let names: Vec<String> = merged(&refs).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "billing.feature-x.nova.test · .localhost",
                "billing.nova.test · .localhost"
            ]
        );
        let only = [host("billing.nova.localhost", "main")];
        let refs: Vec<&ResolvedHost> = only.iter().collect();
        assert_eq!(merged(&refs)[0].0, "billing.nova.localhost");
    }

    #[test]
    fn says_what_it_waits_for() {
        let theme = Theme::default();
        let texts = |tab: &ConfigTab| lines(&theme, tab).iter().map(text).collect::<Vec<_>>();
        assert_eq!(
            texts(&ConfigTab::default()),
            ["Select a project, stack, app or process to see its config."]
        );
        let waiting = ConfigTab {
            subject: Some(super::super::tree::NodeId::Project {
                project: "nova".into(),
            }),
            ..ConfigTab::default()
        };
        assert_eq!(texts(&waiting), ["Resolving…"]);
    }
}
