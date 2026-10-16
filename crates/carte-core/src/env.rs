//! A process's environment, assembled in layers with the source of every value.

mod file;
mod provider;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

pub use file::{EnvFile, EnvFileEntry, EnvFileError, parse_env_file, read_env_files};
pub use provider::{
    CommandOutput, CommandRunner, CommandSpec, ProviderError, SystemRunner, detect_provider,
    load_provider, provider_name,
};

use crate::model::{App, Checkout, EnvProviderKind, EnvTemplate, ProcessSpec, Project, Stack};
use crate::template::{Context, Template, TemplateError, TemplateErrorKind, UrlResolver};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Provider,
    EnvFile,
    Template,
    Injected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// The daemon's own environment, the base for every provider but `shell`.
    Daemon,
    Provider {
        provider: EnvProviderKind,
    },
    EnvFile {
        path: PathBuf,
        line: usize,
    },
    Template {
        origin: TemplateOrigin,
        /// The key as written, e.g. `{{app.name | upper}}_URL`.
        key: String,
        /// The iterated app, for a key template.
        for_app: Option<String>,
    },
    Injected,
}

impl Source {
    pub fn layer(&self) -> Layer {
        match self {
            Source::Daemon | Source::Provider { .. } => Layer::Provider,
            Source::EnvFile { .. } => Layer::EnvFile,
            Source::Template { .. } => Layer::Template,
            Source::Injected => Layer::Injected,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Daemon => f.write_str("daemon"),
            Source::Provider { provider } => f.write_str(provider_name(*provider)),
            Source::EnvFile { path, line } => write!(f, "{}:{line}", path.display()),
            Source::Template {
                origin,
                key,
                for_app,
            } => {
                write!(f, "{origin} template {key:?}")?;
                match for_app {
                    Some(app) => write!(f, " for {app}"),
                    None => Ok(()),
                }
            }
            Source::Injected => f.write_str("carte"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum TemplateOrigin {
    Project { config_path: PathBuf },
    App { app: String },
    Process { app: String, process: String },
}

impl fmt::Display for TemplateOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateOrigin::Project { config_path } => {
                write!(f, "project ({})", config_path.display())
            }
            TemplateOrigin::App { app } => write!(f, "app {app}"),
            TemplateOrigin::Process { app, process } => write!(f, "process {app}/{process}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvValue {
    pub value: String,
    pub source: Source,
    /// Earlier values for the same key, oldest first.
    pub overrode: Vec<Overridden>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overridden {
    pub value: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(transparent)]
pub struct Env {
    vars: BTreeMap<String, EnvValue>,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key`, recording the value it replaces.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>, source: Source) {
        let key = key.into();
        let mut overrode = Vec::new();
        if let Some(previous) = self.vars.remove(&key) {
            overrode = previous.overrode;
            overrode.push(Overridden {
                value: previous.value,
                source: previous.source,
            });
        }
        self.vars.insert(
            key,
            EnvValue {
                value: value.into(),
                source,
                overrode,
            },
        );
    }

    pub fn remove(&mut self, key: &str) -> Option<EnvValue> {
        self.vars.remove(key)
    }

    pub fn get(&self, key: &str) -> Option<&EnvValue> {
        self.vars.get(key)
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(|v| v.value.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &EnvValue)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// The plain variables to hand to a process.
    pub fn to_vars(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }
}

/// One process of one instance, plus the values carte injects that come
/// from outside this crate's knowledge (hostnames, certificates, pidfiles).
#[derive(Clone, Copy)]
pub struct EnvRequest<'a> {
    pub project: &'a Project,
    pub app: &'a App,
    pub process: &'a ProcessSpec,
    pub checkout: &'a Checkout,
    pub stack: &'a Stack,
    /// The branch checked out in `checkout`.
    pub branch: &'a str,
    pub port: u16,
    /// URLs for the instance's stack; also gives `CARTE_URL` and `CARTE_PUBLIC_URL`.
    pub urls: &'a dyn UrlResolver,
    pub tld: &'a str,
    pub share_domain: Option<&'a str>,
    pub tld_length: u32,
    /// System roots plus the carte CA.
    pub ca_bundle: &'a Path,
    /// The carte CA alone.
    pub ca_cert: &'a Path,
    pub pidfile: &'a Path,
}

impl<'a> EnvRequest<'a> {
    fn context(&self) -> Context<'a> {
        Context {
            project: self.project,
            app: self.app,
            checkout: self.checkout.slug(),
            stack: self.stack.slug(),
            branch: self.branch,
            port: self.port,
            urls: self.urls,
        }
    }
}

#[derive(Debug, Error)]
pub enum EnvError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    EnvFile(#[from] EnvFileError),
    #[error("{origin} template {key:?}: {error}")]
    Template {
        origin: TemplateOrigin,
        key: String,
        error: Box<TemplateError>,
    },
    #[error("{origin} template {key:?} rendered an empty variable name")]
    EmptyKey { origin: TemplateOrigin, key: String },
}

/// Runs the provider, reads env files and renders the rest.
pub fn build_env(
    req: &EnvRequest,
    daemon_env: &BTreeMap<String, String>,
    runner: &dyn CommandRunner,
) -> Result<Env, EnvError> {
    let checkout = req.checkout.path();
    let kind = detect_provider(req.project.env_provider, checkout, runner);
    let provider = load_provider(kind, checkout, daemon_env, runner)?;
    let files = read_env_files(checkout, &req.app.settings.env_files)?;
    render_env(req, provider, &files)
}

/// Layers env files, templates and injected variables over `provider`.
pub fn render_env(req: &EnvRequest, provider: Env, files: &[EnvFile]) -> Result<Env, EnvError> {
    let mut env = provider;
    for file in files {
        for entry in &file.entries {
            env.set(
                &entry.key,
                &entry.value,
                Source::EnvFile {
                    path: file.path.clone(),
                    line: entry.line,
                },
            );
        }
    }

    let layers = [
        (
            TemplateOrigin::Project {
                config_path: req.project.config_path.clone(),
            },
            &req.project.env,
        ),
        (
            TemplateOrigin::App {
                app: req.app.name.clone(),
            },
            &req.app.settings.env,
        ),
        (
            TemplateOrigin::Process {
                app: req.app.name.clone(),
                process: req.process.name.clone(),
            },
            &req.process.env,
        ),
    ];
    let ctx = req.context();
    for (origin, entries) in layers {
        for entry in entries {
            for (key, value, for_app) in render_entry(entry, &origin, &ctx)? {
                env.set(
                    key,
                    value,
                    Source::Template {
                        origin: origin.clone(),
                        key: entry.key.clone(),
                        for_app,
                    },
                );
            }
        }
    }

    for (key, value) in injected(req) {
        env.set(key, value, Source::Injected);
    }
    Ok(env)
}

/// An entry whose key contains `{{` renders once per app in the project.
/// Apps without a public URL are left out of a key template that asks for one.
fn render_entry(
    entry: &EnvTemplate,
    origin: &TemplateOrigin,
    ctx: &Context,
) -> Result<Vec<(String, String, Option<String>)>, EnvError> {
    let err = |error: TemplateError| EnvError::Template {
        origin: origin.clone(),
        key: entry.key.clone(),
        error: Box::new(error),
    };
    let value = Template::parse(&entry.value).map_err(err)?;
    if !entry.key.contains("{{") {
        return Ok(vec![(
            entry.key.clone(),
            value.render(ctx).map_err(err)?,
            None,
        )]);
    }
    let key = Template::parse(&entry.key).map_err(err)?;
    let mut rendered = Vec::new();
    for app in &ctx.project.apps {
        let ctx = ctx.with_app(app);
        let value = match value.render(&ctx) {
            Ok(value) => value,
            Err(TemplateError {
                kind: TemplateErrorKind::NotShared(_),
                ..
            }) => continue,
            Err(error) => return Err(err(error)),
        };
        let name = key.render(&ctx).map_err(err)?;
        if name.is_empty() {
            return Err(EnvError::EmptyKey {
                origin: origin.clone(),
                key: entry.key.clone(),
            });
        }
        rendered.push((name, value, Some(app.name.clone())));
    }
    Ok(rendered)
}

/// The variables carte sets on every process, in the design's order.
pub fn injected(req: &EnvRequest) -> Vec<(&'static str, String)> {
    let path = |p: &Path| p.display().to_string();
    let mut vars = vec![
        ("PORT", req.port.to_string()),
        ("CARTE_APP", req.app.name.clone()),
        ("CARTE_PROJECT", req.project.name.clone()),
        ("CARTE_CHECKOUT", req.checkout.slug().to_string()),
        ("CARTE_STACK", req.stack.slug().to_string()),
        ("CARTE_BRANCH", req.branch.to_string()),
        ("CARTE_WORKTREE", path(req.checkout.path())),
        ("CARTE_URL", req.urls.url(req.app)),
    ];
    if let Some(public) = req.urls.public_url(req.app) {
        vars.push(("CARTE_PUBLIC_URL", public));
    }
    let mut hosts = vec![format!(".{}.{}", req.project.name, req.tld)];
    if let Some(domain) = req.share_domain {
        hosts.push(format!(".{}", domain.trim_start_matches('.')));
    }
    vars.extend([
        ("CARTE_TLD_LENGTH", req.tld_length.to_string()),
        ("SSL_CERT_FILE", path(req.ca_bundle)),
        ("REQUESTS_CA_BUNDLE", path(req.ca_bundle)),
        ("NODE_EXTRA_CA_CERTS", path(req.ca_cert)),
        ("DISABLE_SPRING", "1".into()),
        ("RAILS_DEVELOPMENT_HOSTS", hosts.join(",")),
        ("PIDFILE", path(req.pidfile)),
    ]);
    vars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::provider::tests::{FakeRunner, ok};
    use crate::model::{Policy, Readiness, Scope, Worktree};
    use crate::template::tests::{Urls, nova_project};

    fn tpl(key: &str, value: &str) -> EnvTemplate {
        EnvTemplate {
            key: key.into(),
            value: value.into(),
        }
    }

    fn web() -> ProcessSpec {
        ProcessSpec {
            name: "web".into(),
            command: "bin/rails server".into(),
            cwd: None,
            env: Vec::new(),
            http: true,
            ready: Readiness::Port,
            scope: Scope::Instance,
            pty: true,
            policy: Policy::Auto,
        }
    }

    fn feature_checkout() -> Checkout {
        Checkout::Worktree(Worktree {
            branch: "feature/X".into(),
            slug: "feature-x".into(),
            path: "/Users/alex/work/nova-postcard-frontend-feature-x".into(),
        })
    }

    struct Fixture {
        project: Project,
        process: ProcessSpec,
        checkout: Checkout,
        stack: Stack,
        urls: Urls,
        share_domain: Option<&'static str>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                project: nova_project(),
                process: web(),
                checkout: feature_checkout(),
                stack: Stack::Branch("feature-x".into()),
                urls: Urls {
                    stack: "feature-x",
                    shared: vec![],
                },
                share_domain: None,
            }
        }

        fn request(&self) -> EnvRequest<'_> {
            EnvRequest {
                project: &self.project,
                app: &self.project.apps[0],
                process: &self.process,
                checkout: &self.checkout,
                stack: &self.stack,
                branch: "feature/X",
                port: 51234,
                urls: &self.urls,
                tld: "test",
                share_domain: self.share_domain,
                tld_length: 3,
                ca_bundle: Path::new("/Users/alex/.local/state/carte/bundle.pem"),
                ca_cert: Path::new("/Users/alex/.local/state/carte/ca.pem"),
                pidfile: Path::new("/tmp/carte/nova-postcard-frontend@feature-x.pid"),
            }
        }

        fn render(&self, provider: Env, files: &[EnvFile]) -> Env {
            render_env(&self.request(), provider, files).unwrap()
        }

        fn render_empty(&self) -> Env {
            self.render(Env::new(), &[])
        }
    }

    #[test]
    fn injects_every_documented_variable() {
        let mut fx = Fixture::new();
        fx.urls.shared = vec!["nova-postcard-frontend"];
        fx.share_domain = Some("example.dev");
        let env = fx.render_empty();
        let expected = [
            ("PORT", "51234"),
            ("CARTE_APP", "nova-postcard-frontend"),
            ("CARTE_PROJECT", "nova"),
            ("CARTE_CHECKOUT", "feature-x"),
            ("CARTE_STACK", "feature-x"),
            ("CARTE_BRANCH", "feature/X"),
            (
                "CARTE_WORKTREE",
                "/Users/alex/work/nova-postcard-frontend-feature-x",
            ),
            ("CARTE_URL", "https://postcard-frontend.feature-x.nova.test"),
            (
                "CARTE_PUBLIC_URL",
                "https://postcard-frontend--feature-x--nova.example.dev",
            ),
            ("CARTE_TLD_LENGTH", "3"),
            ("SSL_CERT_FILE", "/Users/alex/.local/state/carte/bundle.pem"),
            (
                "REQUESTS_CA_BUNDLE",
                "/Users/alex/.local/state/carte/bundle.pem",
            ),
            (
                "NODE_EXTRA_CA_CERTS",
                "/Users/alex/.local/state/carte/ca.pem",
            ),
            ("DISABLE_SPRING", "1"),
            ("RAILS_DEVELOPMENT_HOSTS", ".nova.test,.example.dev"),
            ("PIDFILE", "/tmp/carte/nova-postcard-frontend@feature-x.pid"),
        ];
        for (key, value) in expected {
            assert_eq!(env.value(key), Some(value), "{key}");
            assert_eq!(env.get(key).unwrap().source, Source::Injected);
        }
        assert_eq!(env.len(), expected.len());
    }

    #[test]
    fn public_url_and_share_host_only_while_configured() {
        let mut fx = Fixture::new();
        fx.checkout = Checkout::Main {
            path: "/Users/alex/work/nova-postcard-frontend".into(),
        };
        fx.stack = Stack::Main;
        fx.urls.stack = "main";
        let env = fx.render_empty();
        assert_eq!(env.value("CARTE_PUBLIC_URL"), None);
        assert_eq!(env.value("RAILS_DEVELOPMENT_HOSTS"), Some(".nova.test"));
        assert_eq!(env.value("CARTE_CHECKOUT"), Some("main"));
        assert_eq!(env.value("CARTE_STACK"), Some("main"));
    }

    #[test]
    fn fallback_instance_has_main_checkout_in_a_branch_stack() {
        let mut fx = Fixture::new();
        fx.checkout = Checkout::Main {
            path: "/Users/alex/work/nova-postcard-frontend".into(),
        };
        fx.project.env = vec![tpl(
            "DATABASE_URL",
            "postgres:///{{app.name | snake}}_{{checkout}}",
        )];
        let env = fx.render_empty();
        assert_eq!(env.value("CARTE_CHECKOUT"), Some("main"));
        assert_eq!(env.value("CARTE_STACK"), Some("feature-x"));
        assert_eq!(
            env.value("DATABASE_URL"),
            Some("postgres:///nova_postcard_frontend_main")
        );
    }

    #[test]
    fn key_templates_expand_once_per_app() {
        let mut fx = Fixture::new();
        fx.project.env = vec![
            tpl("{{app.name | upper}}_URL", "{{url:app}}"),
            tpl("DATABASE_URL", "postgres:///{{app.name}}_{{checkout}}"),
        ];
        let env = fx.render_empty();
        assert_eq!(
            env.value("NOVA_POSTCARD_FRONTEND_URL"),
            Some("https://postcard-frontend.feature-x.nova.test")
        );
        assert_eq!(
            env.value("NOVA_POSTCARD_API_URL"),
            Some("https://postcard-api.feature-x.nova.test")
        );
        assert_eq!(
            env.value("NOVA_BILLING_URL"),
            Some("https://billing.feature-x.nova.test")
        );
        assert_eq!(
            env.value("DATABASE_URL"),
            Some("postgres:///nova-postcard-frontend_feature-x")
        );
        assert_eq!(
            env.get("NOVA_BILLING_URL").unwrap().source,
            Source::Template {
                origin: TemplateOrigin::Project {
                    config_path: "/Users/alex/.config/carte/projects/nova.toml".into()
                },
                key: "{{app.name | upper}}_URL".into(),
                for_app: Some("nova-billing".into()),
            }
        );
    }

    #[test]
    fn key_templates_use_short_names_and_other_variables() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl(
            "{{app.short | upper}}_HOST",
            "{{app.short}}.{{stack}}:{{port}}",
        )];
        let env = fx.render_empty();
        assert_eq!(env.value("BILLING_HOST"), Some("billing.feature-x:51234"));
        assert_eq!(
            env.value("POSTCARD_API_HOST"),
            Some("postcard-api.feature-x:51234")
        );
    }

    #[test]
    fn key_templates_skip_apps_that_are_not_shared() {
        let mut fx = Fixture::new();
        fx.urls.shared = vec!["nova-postcard-api"];
        fx.project.env = vec![tpl("VITE_{{app.short | upper}}_URL", "{{public_url:app}}")];
        let env = fx.render_empty();
        assert_eq!(
            env.value("VITE_POSTCARD_API_URL"),
            Some("https://postcard-api--feature-x--nova.example.dev")
        );
        assert_eq!(env.value("VITE_BILLING_URL"), None);
        assert_eq!(env.value("VITE_POSTCARD_FRONTEND_URL"), None);
    }

    #[test]
    fn public_url_in_an_ordinary_entry_is_an_error_while_not_shared() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("VITE_API_URL", "{{public_url:postcard-api}}")];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "project (/Users/alex/.config/carte/projects/nova.toml) template \"VITE_API_URL\": nova-postcard-api has no public URL because it is not shared at column 3 in \"{{public_url:postcard-api}}\""
        );
    }

    #[test]
    fn template_errors_name_their_origin() {
        let mut fx = Fixture::new();
        fx.process.env = vec![tpl("VITE_PORT", "{{prot}}")];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert!(matches!(
            &err,
            EnvError::Template { origin: TemplateOrigin::Process { process, .. }, .. } if process == "web"
        ));
        assert!(err.to_string().starts_with(
            "process nova-postcard-frontend/web template \"VITE_PORT\": unknown variable \"prot\""
        ));

        fx.process.env = vec![tpl("{{app.name | shout}}_URL", "x")];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert!(err.to_string().contains("unknown filter \"shout\""));
    }

    #[test]
    fn key_template_rendering_empty_is_an_error() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("{{ app.name | slug }}", "x")];
        fx.project.apps[1].name = "--".into();
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert!(matches!(err, EnvError::EmptyKey { .. }));
    }

    #[test]
    fn layers_win_in_order_and_record_what_they_overrode() {
        let mut fx = Fixture::new();
        fx.project.env = vec![
            tpl("DATABASE_URL", "postgres:///{{app.name | snake}}"),
            tpl("PORT", "1"),
        ];
        fx.project.apps[0].settings.env =
            vec![tpl("DATABASE_URL", "postgres:///frontend_{{checkout}}")];
        fx.process.env = vec![tpl("WEB_CONCURRENCY", "2")];

        let mut provider = Env::new();
        provider.set("DATABASE_URL", "postgres:///daemon", Source::Daemon);
        provider.set(
            "DATABASE_URL",
            "postgres:///mise",
            Source::Provider {
                provider: EnvProviderKind::Mise,
            },
        );
        provider.set(
            "WEB_CONCURRENCY",
            "4",
            Source::Provider {
                provider: EnvProviderKind::Mise,
            },
        );
        let files = [EnvFile {
            path: "/w/.env".into(),
            entries: vec![EnvFileEntry {
                key: "DATABASE_URL".into(),
                value: "postgres:///dotenv".into(),
                line: 7,
            }],
        }];
        let env = fx.render(provider, &files);

        let db = env.get("DATABASE_URL").unwrap();
        assert_eq!(db.value, "postgres:///frontend_feature-x");
        assert_eq!(db.source.layer(), Layer::Template);
        let history: Vec<(&str, String)> = db
            .overrode
            .iter()
            .map(|o| (o.value.as_str(), o.source.to_string()))
            .collect();
        assert_eq!(
            history,
            [
                ("postgres:///daemon", "daemon".to_string()),
                ("postgres:///mise", "mise".to_string()),
                ("postgres:///dotenv", "/w/.env:7".to_string()),
                (
                    "postgres:///nova_postcard_frontend",
                    "project (/Users/alex/.config/carte/projects/nova.toml) template \"DATABASE_URL\"".to_string()
                ),
            ]
        );
        assert_eq!(
            db.source.to_string(),
            "app nova-postcard-frontend template \"DATABASE_URL\""
        );

        let port = env.get("PORT").unwrap();
        assert_eq!(port.value, "51234");
        assert_eq!(port.source, Source::Injected);
        assert_eq!(port.overrode[0].value, "1");

        let concurrency = env.get("WEB_CONCURRENCY").unwrap();
        assert_eq!(concurrency.value, "2");
        assert_eq!(concurrency.source.layer(), Layer::Template);
        assert_eq!(concurrency.overrode[0].source.layer(), Layer::Provider);
    }

    #[test]
    fn app_level_url_template_overrides_the_key_template() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("{{app.name | upper}}_URL", "{{url:app}}")];
        fx.project.apps[0].settings.env =
            vec![tpl("NOVA_POSTCARD_API_URL", "http://localhost:4000")];
        let env = fx.render_empty();
        let api = env.get("NOVA_POSTCARD_API_URL").unwrap();
        assert_eq!(api.value, "http://localhost:4000");
        assert_eq!(api.overrode.len(), 1);
        assert_eq!(
            api.overrode[0].source.to_string(),
            "project (/Users/alex/.config/carte/projects/nova.toml) template \"{{app.name | upper}}_URL\" for nova-postcard-api"
        );
    }

    #[test]
    fn later_env_files_win() {
        let fx = Fixture::new();
        let file = |path: &str, value: &str| EnvFile {
            path: path.into(),
            entries: vec![EnvFileEntry {
                key: "SECRET".into(),
                value: value.into(),
                line: 1,
            }],
        };
        let env = fx.render(Env::new(), &[file("/w/.env", "a"), file("/w/.powenv", "b")]);
        assert_eq!(env.value("SECRET"), Some("b"));
        assert_eq!(
            env.get("SECRET").unwrap().overrode[0].source.to_string(),
            "/w/.env:1"
        );
    }

    #[test]
    fn serializes_sources_for_json_output() {
        let mut env = Env::new();
        env.set("A", "1", Source::Daemon);
        env.set(
            "A",
            "2",
            Source::EnvFile {
                path: "/w/.env".into(),
                line: 3,
            },
        );
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "A": {
                    "value": "2",
                    "source": {"kind": "env_file", "path": "/w/.env", "line": 3},
                    "overrode": [{"value": "1", "source": {"kind": "daemon"}}],
                }
            })
        );
    }

    #[test]
    fn build_env_runs_the_detected_provider_and_reads_env_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[tools]\nruby = \"3.4\"\n").unwrap();
        std::fs::write(dir.path().join(".env"), "RUBY_YJIT_ENABLE=0\n").unwrap();
        let mut fx = Fixture::new();
        fx.checkout = Checkout::Main {
            path: dir.path().to_path_buf(),
        };
        fx.project.apps[0].settings.env_files = vec![".env".into(), ".powenv".into()];
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "mise",
            ok(r#"{"RUBY_YJIT_ENABLE":"1","PATH":"/mise/bin:/usr/bin"}"#),
        );
        let daemon: BTreeMap<String, String> =
            [("PATH".to_string(), "/usr/bin".to_string())].into();

        let env = build_env(&fx.request(), &daemon, &runner).unwrap();
        assert_eq!(env.value("PATH"), Some("/mise/bin:/usr/bin"));
        let yjit = env.get("RUBY_YJIT_ENABLE").unwrap();
        assert_eq!(yjit.value, "0");
        assert_eq!(yjit.overrode[0].source.to_string(), "mise");
        assert_eq!(runner.calls.borrow()[0].cwd, dir.path());
    }

    #[test]
    fn build_env_respects_the_configured_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "").unwrap();
        let mut fx = Fixture::new();
        fx.checkout = Checkout::Main {
            path: dir.path().to_path_buf(),
        };
        fx.project.env_provider = Some(EnvProviderKind::None);
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        let daemon: BTreeMap<String, String> =
            [("HOME".to_string(), "/Users/alex".to_string())].into();
        let env = build_env(&fx.request(), &daemon, &runner).unwrap();
        assert_eq!(env.get("HOME").unwrap().source, Source::Daemon);
        assert!(runner.calls.borrow().is_empty());
    }
}
