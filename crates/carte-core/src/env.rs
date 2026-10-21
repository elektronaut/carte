//! A process's environment, assembled in layers with the source of every value.

mod file;
mod provider;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

pub(crate) use file::is_valid_key;
pub use file::{EnvFile, EnvFileEntry, EnvFileError, parse_env_file, read_env_files};
pub use provider::{
    CommandOutput, CommandRunner, CommandSpec, ProviderError, SystemRunner, detect_provider,
    is_mise_config_name, load_provider, provider_name,
};

use crate::dirs::InstanceDirs;
use crate::model::{App, Checkout, EnvProviderKind, EnvTemplate, ProcessSpec, Project, Stack};
use crate::template::{Context, Template, TemplateError, TemplateErrorKind, UrlResolver};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Provider,
    EnvFile,
    Template,
    Provided,
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
    /// A process's `provides`.
    Provided {
        process: String,
    },
    Injected,
}

impl Source {
    pub fn layer(&self) -> Layer {
        match self {
            Source::Daemon | Source::Provider { .. } => Layer::Provider,
            Source::EnvFile { .. } => Layer::EnvFile,
            Source::Template { .. } => Layer::Template,
            Source::Provided { .. } => Layer::Provided,
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
            Source::Provided { process } => write!(f, "provided by {process}"),
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
    /// Every process of the instance with its port, for their `provides`.
    pub processes: &'a [(ProcessSpec, u16)],
    pub checkout: &'a Checkout,
    pub stack: &'a Stack,
    /// The branch checked out in `checkout`; empty on a detached HEAD.
    pub branch: &'a str,
    pub port: u16,
    /// URLs for the instance's stack; also gives `CARTE_URL` and `CARTE_PUBLIC_URL`.
    pub urls: &'a dyn UrlResolver,
    pub tld: &'a str,
    pub share_domain: Option<&'a str>,
    /// The checkout's Vite reads its allowed-hosts variable as one host.
    pub vite_one_host: bool,
    pub tld_length: u32,
    /// System roots plus the carte CA.
    pub ca_bundle: &'a Path,
    /// The carte CA alone.
    pub ca_cert: &'a Path,
    pub pidfile: &'a Path,
    pub dirs: &'a InstanceDirs,
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
            state_dir: &self.dirs.state,
            run_dir: &self.dirs.run,
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
    #[error("process {process:?} provides {key:?}: {error}")]
    Provides {
        process: String,
        key: String,
        error: Box<TemplateError>,
    },
    #[error("{origin} template {key:?} rendered an empty variable name")]
    EmptyKey { origin: TemplateOrigin, key: String },
    #[error(
        "{origin} template {key:?} rendered the variable name {name:?}: use letters, digits and underscores"
    )]
    InvalidKey {
        origin: TemplateOrigin,
        key: String,
        name: String,
    },
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

/// Layers env files, templates, provided and injected variables over `provider`.
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

    for (process, port) in req.processes {
        let ctx = Context { port: *port, ..ctx };
        for entry in &process.provides {
            let value = Template::parse(&entry.value)
                .and_then(|value| value.render(&ctx))
                .map_err(|error| EnvError::Provides {
                    process: process.name.clone(),
                    key: entry.key.clone(),
                    error: Box::new(error),
                })?;
            env.set(
                &entry.key,
                value,
                Source::Provided {
                    process: process.name.clone(),
                },
            );
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
        let name = match key.render(&ctx) {
            Ok(name) => name,
            Err(TemplateError {
                kind: TemplateErrorKind::NotShared(_),
                ..
            }) => continue,
            Err(error) => return Err(err(error)),
        };
        if name.is_empty() {
            return Err(EnvError::EmptyKey {
                origin: origin.clone(),
                key: entry.key.clone(),
            });
        }
        if !is_valid_key(&name) {
            return Err(EnvError::InvalidKey {
                origin: origin.clone(),
                key: entry.key.clone(),
                name,
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
        ("CARTE_STATE_DIR", path(&req.dirs.state)),
        ("CARTE_RUN_DIR", path(&req.dirs.run)),
        ("CARTE_URL", req.urls.url(req.app)),
    ];
    if let Some(public) = req.urls.public_url(req.app) {
        vars.push(("CARTE_PUBLIC_URL", public));
    }
    vars.extend([
        ("CARTE_TLD_LENGTH", req.tld_length.to_string()),
        ("SSL_CERT_FILE", path(req.ca_bundle)),
        ("REQUESTS_CA_BUNDLE", path(req.ca_bundle)),
        ("NODE_EXTRA_CA_CERTS", path(req.ca_cert)),
        ("DISABLE_SPRING", "1".into()),
        ("RAILS_DEVELOPMENT_HOSTS", rails_hosts(req).join(",")),
        (
            "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS",
            vite_hosts(req).join(","),
        ),
        ("PIDFILE", path(req.pidfile)),
    ]);
    vars
}

fn served_tlds(tld: &str) -> Vec<&str> {
    if tld == "localhost" {
        vec![tld]
    } else {
        vec![tld, "localhost"]
    }
}

fn push_unique(hosts: &mut Vec<String>, host: String) {
    if !hosts.contains(&host) {
        hosts.push(host);
    }
}

/// Rails' `config.hosts` entries, each matching its domain and one label below it:
/// the project, the branch stack, the app's own host when it takes subdomains, and
/// each alias on main.
fn rails_hosts(req: &EnvRequest) -> Vec<String> {
    let tlds = served_tlds(req.tld);
    let mut domains = vec![req.project.name.clone()];
    if let Stack::Branch(slug) = req.stack {
        domains.push(format!("{slug}.{}", req.project.name));
    }
    if req.app.settings.subdomains {
        domains.push(crate::naming::local_labels(req.project, req.app, req.stack).join("."));
    }
    if *req.stack == Stack::Main {
        domains.extend(req.app.settings.aliases.iter().cloned());
    }
    let mut hosts = Vec::new();
    for domain in &domains {
        for tld in &tlds {
            push_unique(&mut hosts, format!(".{domain}.{tld}"));
        }
    }
    hosts.extend(share_host(req));
    hosts
}

/// Vite's `server.allowedHosts` entries: a leading-dot entry matches subdomains at
/// any depth and `.localhost` is always allowed.
fn vite_hosts(req: &EnvRequest) -> Vec<String> {
    let mut hosts = vec![format!(".{}.{}", req.project.name, req.tld)];
    if *req.stack == Stack::Main {
        for alias in &req.app.settings.aliases {
            push_unique(&mut hosts, format!(".{alias}.{}", req.tld));
        }
    }
    hosts.extend(share_host(req));
    if req.vite_one_host {
        let localhost = req.tld == "localhost";
        let one = match share_host(req) {
            Some(share) if localhost => share,
            _ => hosts.swap_remove(0),
        };
        return vec![one];
    }
    hosts
}

fn share_host(req: &EnvRequest) -> Option<String> {
    req.share_domain
        .map(|domain| format!(".{}", domain.trim_start_matches('.')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::provider::tests::{FakeRunner, ok};
    use crate::model::{Policy, Readiness, Restart, Scope, Worktree};
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
            kind: None,
            after: Vec::new(),
            provides: Vec::new(),
            restart: Restart::Never,
            paths: Vec::new(),
            preset: None,
        }
    }

    fn feature_checkout() -> Checkout {
        Checkout::Worktree(Worktree {
            branch: Some("feature/X".into()),
            slug: "feature-x".into(),
            path: "/Users/alex/work/nova-postcard-frontend-feature-x".into(),
        })
    }

    struct Fixture {
        project: Project,
        process: ProcessSpec,
        processes: Vec<(ProcessSpec, u16)>,
        checkout: Checkout,
        stack: Stack,
        urls: Urls,
        share_domain: Option<&'static str>,
        vite_one_host: bool,
        dirs: InstanceDirs,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                project: nova_project(),
                process: web(),
                processes: Vec::new(),
                checkout: feature_checkout(),
                stack: Stack::Branch("feature-x".into()),
                urls: Urls {
                    stack: "feature-x",
                    shared: vec![],
                },
                share_domain: None,
                vite_one_host: false,
                dirs: InstanceDirs {
                    state: "/state/data/nova/feature-x/nova-postcard-frontend".into(),
                    run: "/run/carte/abcdefgh".into(),
                },
            }
        }

        fn request(&self) -> EnvRequest<'_> {
            EnvRequest {
                project: &self.project,
                app: &self.project.apps[0],
                process: &self.process,
                processes: &self.processes,
                checkout: &self.checkout,
                stack: &self.stack,
                branch: "feature/X",
                port: 51234,
                urls: &self.urls,
                tld: "test",
                share_domain: self.share_domain,
                vite_one_host: self.vite_one_host,
                tld_length: 3,
                ca_bundle: Path::new("/Users/alex/.local/state/carte/bundle.pem"),
                ca_cert: Path::new("/Users/alex/.local/state/carte/ca.pem"),
                pidfile: Path::new("/tmp/carte/nova-postcard-frontend@feature-x.pid"),
                dirs: &self.dirs,
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
            (
                "CARTE_STATE_DIR",
                "/state/data/nova/feature-x/nova-postcard-frontend",
            ),
            ("CARTE_RUN_DIR", "/run/carte/abcdefgh"),
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
            (
                "RAILS_DEVELOPMENT_HOSTS",
                ".nova.test,.nova.localhost,.feature-x.nova.test,.feature-x.nova.localhost,.example.dev",
            ),
            (
                "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS",
                ".nova.test,.example.dev",
            ),
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
        assert_eq!(
            env.value("RAILS_DEVELOPMENT_HOSTS"),
            Some(".nova.test,.nova.localhost")
        );
        assert_eq!(env.value("CARTE_CHECKOUT"), Some("main"));
        assert_eq!(env.value("CARTE_STACK"), Some("main"));
    }

    /// `ActionDispatch::HostAuthorization`: a leading-dot entry becomes
    /// `\A(?:[a-z0-9-]+\.)?<domain>(:\d+)?\z`, anything else an exact match.
    fn rails_allows(entries: &str, host: &str) -> bool {
        entries
            .split(',')
            .any(|entry| match entry.strip_prefix('.') {
                Some(domain) => {
                    host == domain
                        || host.strip_suffix(domain).is_some_and(|rest| {
                            rest.strip_suffix('.').is_some_and(|label| {
                                !label.is_empty()
                                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                            })
                        })
                }
                None => host == entry,
            })
    }

    /// Vite's `isHostAllowed`: `.localhost` always, a leading-dot entry at any depth.
    fn vite_allows(entries: &str, host: &str) -> bool {
        host.ends_with(".localhost")
            || entries.split(',').any(|entry| {
                entry == host
                    || entry.starts_with('.') && (host == &entry[1..] || host.ends_with(entry))
            })
    }

    fn hosts_for(fx: &Fixture) -> (String, String) {
        let env = fx.render_empty();
        (
            env.value("RAILS_DEVELOPMENT_HOSTS").unwrap().to_string(),
            env.value("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS")
                .unwrap()
                .to_string(),
        )
    }

    fn assert_allowed(fx: &Fixture, hosts: &[&str]) {
        let (rails, vite) = hosts_for(fx);
        for host in hosts {
            assert!(
                rails_allows(&rails, host),
                "Rails blocks {host} with {rails}"
            );
            assert!(vite_allows(&vite, host), "Vite blocks {host} with {vite}");
        }
    }

    fn on_main(fx: &mut Fixture) {
        fx.checkout = Checkout::Main {
            path: "/Users/alex/work/nova-postcard-frontend".into(),
        };
        fx.stack = Stack::Main;
        fx.urls.stack = "main";
    }

    #[test]
    fn rails_matcher_allows_one_label_below_a_leading_dot() {
        assert!(rails_allows(".nova.test", "nova.test"));
        assert!(rails_allows(".nova.test", "postcard-frontend.nova.test"));
        assert!(!rails_allows(
            ".nova.test",
            "postcard-frontend.feature-x.nova.test"
        ));
        assert!(!rails_allows(".nova.test", "xnova.test"));
        assert!(rails_allows("nova.test", "nova.test"));
        assert!(!rails_allows("nova.test", "a.nova.test"));
    }

    #[test]
    fn hosts_on_main() {
        let mut fx = Fixture::new();
        on_main(&mut fx);
        let (rails, vite) = hosts_for(&fx);
        assert_eq!(rails, ".nova.test,.nova.localhost");
        assert_eq!(vite, ".nova.test");
        assert_allowed(
            &fx,
            &[
                "postcard-frontend.nova.test",
                "postcard-frontend.nova.localhost",
            ],
        );
    }

    #[test]
    fn hosts_in_a_branch_stack() {
        let fx = Fixture::new();
        let (rails, vite) = hosts_for(&fx);
        assert_eq!(
            rails,
            ".nova.test,.nova.localhost,.feature-x.nova.test,.feature-x.nova.localhost"
        );
        assert_eq!(vite, ".nova.test");
        assert_allowed(
            &fx,
            &[
                "postcard-frontend.feature-x.nova.test",
                "postcard-frontend.feature-x.nova.localhost",
                "postcard-api.nova.test",
            ],
        );
    }

    #[test]
    fn hosts_for_subdomains_on_main() {
        let mut fx = Fixture::new();
        on_main(&mut fx);
        fx.project.apps[0].settings.subdomains = true;
        let (rails, _) = hosts_for(&fx);
        assert_eq!(
            rails,
            ".nova.test,.nova.localhost,.postcard-frontend.nova.test,.postcard-frontend.nova.localhost"
        );
        assert_allowed(
            &fx,
            &[
                "postcard-frontend.nova.test",
                "www.postcard-frontend.nova.test",
                "www.postcard-frontend.nova.localhost",
            ],
        );
    }

    #[test]
    fn hosts_for_subdomains_in_a_branch_stack() {
        let mut fx = Fixture::new();
        fx.project.apps[0].settings.subdomains = true;
        let (rails, vite) = hosts_for(&fx);
        assert_eq!(
            rails,
            ".nova.test,.nova.localhost,.feature-x.nova.test,.feature-x.nova.localhost,\
             .postcard-frontend.feature-x.nova.test,.postcard-frontend.feature-x.nova.localhost"
        );
        assert_eq!(vite, ".nova.test");
        assert_allowed(
            &fx,
            &[
                "postcard-frontend.feature-x.nova.test",
                "www.postcard-frontend.feature-x.nova.test",
                "www.postcard-frontend.feature-x.nova.localhost",
            ],
        );
    }

    #[test]
    fn hosts_for_aliases_only_on_main() {
        let mut fx = Fixture::new();
        on_main(&mut fx);
        fx.project.apps[0].settings.aliases = vec!["nova-frontend".into()];
        fx.project.apps[0].settings.subdomains = true;
        let (rails, vite) = hosts_for(&fx);
        assert_eq!(
            rails,
            ".nova.test,.nova.localhost,.postcard-frontend.nova.test,.postcard-frontend.nova.localhost,\
             .nova-frontend.test,.nova-frontend.localhost"
        );
        assert_eq!(vite, ".nova.test,.nova-frontend.test");
        assert_allowed(
            &fx,
            &[
                "nova-frontend.test",
                "nova-frontend.localhost",
                "www.nova-frontend.test",
            ],
        );

        let mut fx = Fixture::new();
        fx.project.apps[0].settings.aliases = vec!["nova-frontend".into()];
        let (rails, vite) = hosts_for(&fx);
        assert!(!rails.contains("nova-frontend"), "{rails}");
        assert!(!vite.contains("nova-frontend"), "{vite}");
    }

    #[test]
    fn hosts_for_the_share_domain() {
        let mut fx = Fixture::new();
        fx.urls.shared = vec!["nova-postcard-frontend"];
        fx.share_domain = Some("example.dev");
        let (rails, vite) = hosts_for(&fx);
        assert!(rails.ends_with(",.example.dev"), "{rails}");
        assert_eq!(vite, ".nova.test,.example.dev");
        assert_allowed(
            &fx,
            &[
                "postcard-frontend--feature-x--nova.example.dev",
                "example.dev",
            ],
        );
    }

    #[test]
    fn older_vite_gets_one_host() {
        let mut fx = Fixture::new();
        fx.share_domain = Some("example.dev");
        fx.vite_one_host = true;
        assert_eq!(hosts_for(&fx).1, ".nova.test");
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

        fx.project.env = vec![tpl("{{app.short | upper}}_DIR", "{{state_dir}}")];
        let env = fx.render_empty();
        assert_eq!(
            env.value("BILLING_DIR"),
            Some("/state/data/nova/feature-x/nova-postcard-frontend")
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
    fn public_hosts_in_keys_skip_apps_that_are_not_shared() {
        let mut fx = Fixture::new();
        fx.urls.shared = vec!["nova-postcard-api"];
        fx.project.env = vec![tpl("{{public_host:app | snake | upper}}", "1")];
        let env = fx.render_empty();
        let names: Vec<_> = env
            .to_vars()
            .into_keys()
            .filter(|k| k.contains("EXAMPLE_DEV"))
            .collect();
        assert_eq!(names, ["POSTCARD_API__FEATURE_X__NOVA_EXAMPLE_DEV"]);
    }

    #[test]
    fn rendered_names_must_be_valid() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("{{host:app}}", "1")];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert!(matches!(err, EnvError::InvalidKey { .. }), "{err}");
    }

    #[test]
    fn key_templates_render_hosts_per_app() {
        let mut fx = Fixture::new();
        fx.urls.shared = vec!["nova-postcard-api"];
        fx.project.env = vec![
            tpl("{{app.short | upper}}_HOST", "{{host:app}}"),
            tpl("{{app.short | upper}}_PUBLIC_HOST", "{{public_host:app}}"),
        ];
        let env = fx.render_empty();
        assert_eq!(
            env.value("POSTCARD_API_HOST"),
            Some("postcard-api.feature-x.nova.test")
        );
        assert_eq!(
            env.value("BILLING_HOST"),
            Some("billing.feature-x.nova.test")
        );
        assert_eq!(
            env.value("POSTCARD_API_PUBLIC_HOST"),
            Some("postcard-api--feature-x--nova.example.dev")
        );
        assert_eq!(env.value("BILLING_PUBLIC_HOST"), None);
    }

    #[test]
    fn public_host_in_an_ordinary_entry_is_an_error_while_not_shared() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("API_HOST", "{{public_host:postcard-api}}")];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "project (/Users/alex/.config/carte/projects/nova.toml) template \"API_HOST\": nova-postcard-api has no public URL because it is not shared at column 3 in \"{{public_host:postcard-api}}\""
        );
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
        fx.project.env = vec![tpl("{{ app.name | slug | snake }}", "x")];
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

    fn redis(provides: &[(&str, &str)]) -> ProcessSpec {
        ProcessSpec {
            name: "redis".into(),
            command: "redis-server --port $PORT".into(),
            http: false,
            kind: Some(crate::model::ProcessKind::Service),
            provides: provides.iter().map(|(k, v)| tpl(k, v)).collect(),
            restart: Restart::OnFailure,
            ..web()
        }
    }

    #[test]
    fn provided_values_render_in_the_instance_with_the_providers_port() {
        let mut fx = Fixture::new();
        fx.processes = vec![
            (web(), 51234),
            (
                redis(&[
                    ("REDIS_URL", "unix://{{run_dir}}/redis.sock"),
                    ("REDIS_PORT", "{{port}}"),
                    ("REDIS_PREFIX", "{{app.short}}:{{stack}}"),
                ]),
                51300,
            ),
        ];
        let env = fx.render_empty();
        let url = env.get("REDIS_URL").unwrap();
        assert_eq!(url.value, "unix:///run/carte/abcdefgh/redis.sock");
        assert_eq!(
            url.source,
            Source::Provided {
                process: "redis".into()
            }
        );
        assert_eq!(url.source.to_string(), "provided by redis");
        assert_eq!(url.source.layer(), Layer::Provided);
        assert_eq!(env.value("REDIS_PORT"), Some("51300"));
        assert_eq!(env.value("PORT"), Some("51234"));
        assert_eq!(
            env.value("REDIS_PREFIX"),
            Some("postcard-frontend:feature-x")
        );
    }

    #[test]
    fn provided_values_beat_env_files_and_templates_but_not_carte() {
        let mut fx = Fixture::new();
        fx.project.env = vec![tpl("REDIS_URL", "redis://localhost:6379/0")];
        fx.process.env = vec![tpl("REDIS_URL", "redis://localhost:6379/1")];
        fx.processes = vec![(
            redis(&[
                ("REDIS_URL", "unix://{{run_dir}}/redis.sock"),
                ("PORT", "1"),
            ]),
            51300,
        )];
        let files = [EnvFile {
            path: "/w/.env".into(),
            entries: vec![EnvFileEntry {
                key: "REDIS_URL".into(),
                value: "redis://localhost:6379".into(),
                line: 2,
            }],
        }];
        let env = fx.render(Env::new(), &files);
        let url = env.get("REDIS_URL").unwrap();
        assert_eq!(url.value, "unix:///run/carte/abcdefgh/redis.sock");
        let history: Vec<String> = url.overrode.iter().map(|o| o.source.to_string()).collect();
        assert_eq!(
            history,
            [
                "/w/.env:2",
                "project (/Users/alex/.config/carte/projects/nova.toml) template \"REDIS_URL\"",
                "process nova-postcard-frontend/web template \"REDIS_URL\"",
            ]
        );
        let port = env.get("PORT").unwrap();
        assert_eq!(port.value, "51234");
        assert_eq!(port.source, Source::Injected);
        assert_eq!(port.overrode[0].source.to_string(), "provided by redis");
    }

    #[test]
    fn provides_errors_name_the_process() {
        let mut fx = Fixture::new();
        fx.processes = vec![(redis(&[("REDIS_URL", "unix://{{rundir}}/redis.sock")]), 1)];
        let err = render_env(&fx.request(), Env::new(), &[]).unwrap_err();
        assert!(
            err.to_string().starts_with(
                "process \"redis\" provides \"REDIS_URL\": unknown variable \"rundir\""
            ),
            "{err}"
        );
    }

    #[test]
    fn the_acme_config_gives_redis_to_the_apps_it_lists() {
        let file = crate::config::parse_project(
            Path::new("/Users/alex/.config/carte/projects/acme.toml"),
            r#"
            root = "~/work"
            include = "acme-*"
            strip_prefix = "acme-"

            [processes.redis]
            apps = [
              "acme-start", "acme-mailer-api", "acme-sms-api", "acme-mta-logger",
              "acme-sender-api", "acme-subscribers-api", "acme-integration-shop",
              "acme-calendar", "acme-mobile",
            ]
            command = "mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis --appendonly yes --save ''"
            kind = "service"

            [processes.redis.provides]
            REDIS_URL = "unix://{{run_dir}}/redis.sock"
            "#,
        )
        .unwrap();
        let discovered = [
            "acme-start",
            "acme-mailer-api",
            "acme-sms-api",
            "acme-mta-logger",
            "acme-sender-api",
            "acme-subscribers-api",
            "acme-integration-shop",
            "acme-calendar",
            "acme-mobile",
            "acme-admin",
        ]
        .into_iter()
        .map(|name| crate::config::DiscoveredApp {
            name: name.into(),
            naming: crate::config::Naming::Directory,
            path: Path::new("/Users/alex/work").join(name),
            repo: None,
            branch_only: false,
            repo_config: None,
        })
        .collect();
        let (project, problems) = crate::config::assemble_project(file, discovered);
        assert!(problems.is_empty(), "{problems:?}");

        let procfile = vec![
            ("web".to_string(), "bin/rails server -p $PORT".to_string()),
            ("worker".to_string(), "bundle exec sidekiq".to_string()),
        ];
        let dirs = InstanceDirs {
            state: "/state/data/acme/feature-x/acme-start".into(),
            run: "/run/carte/abcdefgh".into(),
        };
        let urls = Urls {
            stack: "feature-x",
            shared: vec![],
        };
        let render = |app_name: &str, process: &str| {
            let app = project.app(app_name).unwrap();
            let list = crate::procfile::build(
                crate::procfile::ProcessSource::Procfile,
                procfile.clone(),
                &app.settings,
            )
            .unwrap();
            let processes: Vec<(ProcessSpec, u16)> = list
                .processes
                .iter()
                .enumerate()
                .map(|(i, p)| (p.clone(), 51000 + i as u16))
                .collect();
            let (spec, port) = processes.iter().find(|(p, _)| p.name == process).unwrap();
            let checkout = Checkout::Main {
                path: app.path.clone(),
            };
            let req = EnvRequest {
                project: &project,
                app,
                process: spec,
                processes: &processes,
                checkout: &checkout,
                stack: &Stack::Branch("feature-x".into()),
                branch: "feature-x",
                port: *port,
                urls: &urls,
                tld: "test",
                share_domain: None,
                vite_one_host: false,
                tld_length: 3,
                ca_bundle: Path::new("/ca/bundle.pem"),
                ca_cert: Path::new("/ca/ca.pem"),
                pidfile: Path::new("/tmp/carte/app.pid"),
                dirs: &dirs,
            };
            let names: Vec<String> = list.processes.iter().map(|p| p.name.clone()).collect();
            (
                names,
                list.processes,
                render_env(&req, Env::new(), &[]).unwrap(),
            )
        };

        let (names, processes, worker) = render("acme-start", "worker");
        assert_eq!(names, ["web", "worker", "redis"]);
        let redis = processes.iter().find(|p| p.name == "redis").unwrap();
        assert!(redis.is_service());
        assert_eq!(redis.scope, Scope::Instance);
        assert!(!redis.http);
        assert!(
            redis
                .command
                .contains("--unixsocket $CARTE_RUN_DIR/redis.sock")
        );
        let url = worker.get("REDIS_URL").unwrap();
        assert_eq!(url.value, "unix:///run/carte/abcdefgh/redis.sock");
        assert_eq!(url.source.to_string(), "provided by redis");

        let (_, _, server) = render("acme-start", "redis");
        assert_eq!(
            server.value("CARTE_STATE_DIR"),
            Some("/state/data/acme/feature-x/acme-start")
        );
        assert_eq!(server.value("CARTE_RUN_DIR"), Some("/run/carte/abcdefgh"));

        let (names, _, admin) = render("acme-admin", "worker");
        assert_eq!(names, ["web", "worker"]);
        assert_eq!(admin.value("REDIS_URL"), None);
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
