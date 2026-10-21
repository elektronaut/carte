use serde::Serialize;
use thiserror::Error;

use crate::model::{
    App, InstanceKey, MachineConfig, Problem, Project, ProjectState, Scheme, ShareConfig, Stack,
    World,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NamingError {
    #[error("project {project} has no stack {stack}")]
    UnknownStack { project: String, stack: String },
    #[error(
        "public hostname label {label:?} is longer than 63 characters even with the stack name shortened"
    )]
    PublicLabelTooLong { label: String },
}

/// An app's own hostname label, or `None` for a collapsed app.
fn app_label<'a>(project: &Project, app: &'a App) -> Option<&'a str> {
    (!app.is_collapsed(&project.name)).then_some(app.short.as_str())
}

/// The app whose label is `label`. Collapsed apps have no label and never match.
fn labelled_app<'a>(project: &'a Project, label: &str) -> Option<&'a App> {
    project
        .apps
        .iter()
        .find(|a| a.short == label && !a.is_collapsed(&project.name))
}

/// The labels of an instance's local hostname without the tld, left to right.
pub fn local_labels<'a>(project: &'a Project, app: &'a App, stack: &'a Stack) -> Vec<&'a str> {
    let mut labels = Vec::with_capacity(3);
    labels.extend(app_label(project, app));
    if let Stack::Branch(slug) = stack {
        labels.push(slug.as_str());
    }
    labels.push(project.name.as_str());
    labels
}

/// `app.stack.project.<tld>`, with the stack omitted on main and the app omitted when collapsed.
pub fn local_host(tld: &str, project: &Project, app: &App, stack: &Stack) -> String {
    let mut host = local_labels(project, app, stack).join(".");
    host.push('.');
    host.push_str(tld);
    host
}

/// The instance's own URL, with the port only when it isn't the scheme's default.
pub fn instance_url(
    machine: &MachineConfig,
    project: &Project,
    app: &App,
    stack: &Stack,
) -> String {
    let host = local_host(&machine.tld, project, app, stack);
    match project.scheme {
        Scheme::Https if machine.https_port == 443 => format!("https://{host}"),
        Scheme::Https => format!("https://{host}:{}", machine.https_port),
        Scheme::Http if machine.http_port == 80 => format!("http://{host}"),
        Scheme::Http => format!("http://{host}:{}", machine.http_port),
    }
}

/// `CARTE_TLD_LENGTH`: how many labels follow the app's own label, so that Rails'
/// `request.subdomain` is the same on main and in every stack.
pub fn tld_length(tld: &str, project: &Project, app: &App, stack: &Stack) -> usize {
    local_labels(project, app, stack).len() + tld.split('.').count() - 1
}

/// The branch a stack's worktrees have checked out, when they all have the same one.
/// `None` when they differ or any of them has a detached HEAD.
pub fn stack_branch<'a>(state: &'a ProjectState, slug: &str) -> Option<&'a str> {
    let mut branches = state
        .worktrees
        .values()
        .flatten()
        .filter(|wt| wt.slug == slug)
        .map(|wt| wt.branch.as_deref());
    let first = branches.next()??;
    branches.all(|b| b == Some(first)).then_some(first)
}

/// The single DNS label used under the share domain: local labels joined with `--`.
/// When it exceeds 63 characters the stack name is cut and gets a hash of the full name.
pub fn public_label(state: &ProjectState, app: &App, stack: &Stack) -> Result<String, NamingError> {
    let project = &state.project;
    let full = local_labels(project, app, stack).join("--");
    if full.len() <= 63 {
        return Ok(full);
    }
    let Stack::Branch(slug) = stack else {
        return Err(NamingError::PublicLabelTooLong { label: full });
    };
    if !state.has_stack(slug) {
        return Err(NamingError::UnknownStack {
            project: project.name.clone(),
            stack: slug.clone(),
        });
    }
    let budget = 63usize.saturating_sub(full.len() - slug.len());
    let Some(short_slug) = crate::slug::shorten(slug, budget) else {
        return Err(NamingError::PublicLabelTooLong { label: full });
    };
    let mut labels = Vec::with_capacity(3);
    labels.extend(app_label(project, app));
    labels.push(short_slug.as_str());
    labels.push(project.name.as_str());
    Ok(labels.join("--"))
}

pub fn public_host(
    share: &ShareConfig,
    state: &ProjectState,
    app: &App,
    stack: &Stack,
) -> Result<String, NamingError> {
    if share.single_host() {
        return Ok(share.domain.clone());
    }
    Ok(format!(
        "{}.{}",
        public_label(state, app, stack)?,
        share.domain
    ))
}

/// Public URLs are always `https://`: the tunnel terminates TLS.
pub fn public_url(
    share: &ShareConfig,
    state: &ProjectState,
    app: &App,
    stack: &Stack,
) -> Result<String, NamingError> {
    Ok(format!(
        "https://{}",
        public_host(share, state, app, stack)?
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub key: InstanceKey,
    /// Labels left of the instance's own hostname, left to right.
    pub subdomains: Vec<String>,
    /// The request arrived through one of the app's `aliases`.
    pub via_alias: bool,
}

/// Why a hostname doesn't lead to an instance, for the proxy's error page.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NotFound {
    #[error("{host:?} is not a valid hostname")]
    InvalidHost { host: String },
    #[error("{host} is not under .{tld}")]
    UnknownTld { host: String, tld: String },
    #[error("no project or alias named {label}")]
    UnknownProject { label: String },
    #[error("{}", unknown_name_message(project, stack.as_deref(), label))]
    UnknownName {
        project: String,
        stack: Option<String>,
        label: String,
    },
    #[error("{}", no_app_message(project, stack.as_deref()))]
    NoApp {
        project: String,
        stack: Option<String>,
    },
    #[error("app {} doesn't allow subdomains ({})", key.app, subdomains.join("."))]
    SubdomainsNotAllowed {
        key: InstanceKey,
        subdomains: Vec<String>,
    },
    #[error("nothing is shared as {label}")]
    UnknownPublicLabel { label: String },
    #[error(
        "kube project {project} has no subdomains ({}): a host is service.namespace.{project}",
        labels.join(".")
    )]
    KubeSubdomains {
        project: String,
        labels: Vec<String>,
    },
    #[error("namespace {namespace} isn't one of the namespaces of kube project {project}")]
    KubeNamespace { project: String, namespace: String },
}

fn unknown_name_message(project: &str, stack: Option<&str>, label: &str) -> String {
    match stack {
        Some(stack) => format!("no app named {label} in stack {stack} of project {project}"),
        None => format!("no app or stack named {label} in project {project}"),
    }
}

fn no_app_message(project: &str, stack: Option<&str>) -> String {
    match stack {
        Some(stack) => format!("stack {stack} of project {project} needs an app name"),
        None => format!("project {project} needs an app name"),
    }
}

/// Lowercased labels of a `Host` header without port or trailing dot.
fn host_labels(host: &str) -> Result<Vec<String>, NotFound> {
    let invalid = || NotFound::InvalidHost {
        host: host.to_string(),
    };
    let trimmed = host.trim();
    if trimmed.starts_with('[') {
        return Err(invalid());
    }
    let name = trimmed.split(':').next().unwrap_or_default();
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    let labels: Vec<String> = name.split('.').map(Into::into).collect();
    if labels.iter().any(|l| l.is_empty()) {
        return Err(invalid());
    }
    Ok(labels)
}

/// Resolves an incoming `Host` against the world, reading labels from the right:
/// tld, project (or an alias), an optional stack, then the app unless it is collapsed.
/// Anything left over is a subdomain for apps that allow them.
pub fn resolve_host(world: &World, host: &str) -> Result<Resolved, NotFound> {
    let rest = labels_under_tld(world, host)?;
    let (top, left) = rest.split_last().expect("at least one label under the tld");

    if let Some(state) = world.project(top) {
        return resolve_in_project(state, left);
    }
    let alias = world.projects.iter().find_map(|state| {
        state
            .project
            .apps
            .iter()
            .find(|app| app.settings.aliases.iter().any(|a| a == top))
            .map(|app| (state, app))
    });
    let Some((state, app)) = alias else {
        return Err(NotFound::UnknownProject { label: top.clone() });
    };
    let key = InstanceKey {
        project: state.project.name.clone(),
        app: app.name.clone(),
        stack: Stack::Main,
    };
    with_subdomains(app, key, left, true)
}

/// The labels of a `Host` left of one of the served TLDs; never empty.
pub(crate) fn labels_under_tld(world: &World, host: &str) -> Result<Vec<String>, NotFound> {
    let mut labels = host_labels(host)?;
    let tlds = world.machine.served_tlds();
    let matched = tlds.iter().find_map(|tld| {
        let tld_labels: Vec<&str> = tld.split('.').collect();
        let under = labels.len() > tld_labels.len()
            && labels[labels.len() - tld_labels.len()..]
                .iter()
                .zip(&tld_labels)
                .all(|(a, b)| a == b);
        under.then_some(tld_labels.len())
    });
    let Some(tld_len) = matched else {
        return Err(NotFound::UnknownTld {
            host: labels.join("."),
            tld: tlds.join(" or ."),
        });
    };
    labels.truncate(labels.len() - tld_len);
    Ok(labels)
}

fn resolve_in_project(state: &ProjectState, labels: &[String]) -> Result<Resolved, NotFound> {
    let project = &state.project;
    let mut labels = labels;
    let mut stack = Stack::Main;
    if let Some((label, left)) = labels.split_last()
        && labelled_app(project, label).is_none()
        && state.has_stack(label)
    {
        stack = Stack::Branch(label.clone());
        labels = left;
    }
    let stack_name = match &stack {
        Stack::Main => None,
        Stack::Branch(slug) => Some(slug.clone()),
    };

    let (app, subdomains, collapsed) = match labels
        .split_last()
        .and_then(|(l, left)| Some((labelled_app(project, l)?, left)))
    {
        Some((app, left)) => (app, left, false),
        None => {
            let collapsed = project.apps.iter().find(|a| a.is_collapsed(&project.name));
            match (collapsed, labels.last()) {
                (Some(app), _) => (app, labels, true),
                (None, Some(label)) => {
                    return Err(NotFound::UnknownName {
                        project: project.name.clone(),
                        stack: stack_name,
                        label: label.clone(),
                    });
                }
                (None, None) => {
                    return Err(NotFound::NoApp {
                        project: project.name.clone(),
                        stack: stack_name,
                    });
                }
            }
        }
    };

    if collapsed
        && !app.settings.subdomains
        && let Some(label) = subdomains.last()
    {
        return Err(NotFound::UnknownName {
            project: project.name.clone(),
            stack: stack_name,
            label: label.clone(),
        });
    }
    let key = InstanceKey {
        project: project.name.clone(),
        app: app.name.clone(),
        stack,
    };
    with_subdomains(app, key, subdomains, false)
}

fn with_subdomains(
    app: &App,
    key: InstanceKey,
    subdomains: &[String],
    via_alias: bool,
) -> Result<Resolved, NotFound> {
    if !subdomains.is_empty() && !app.settings.subdomains {
        return Err(NotFound::SubdomainsNotAllowed {
            key,
            subdomains: subdomains.to_vec(),
        });
    }
    Ok(Resolved {
        key,
        subdomains: subdomains.to_vec(),
        via_alias,
    })
}

/// Resolves a host under the machine's share domain. Subdomains are never reachable there.
pub fn resolve_public_host(world: &World, host: &str) -> Result<InstanceKey, NotFound> {
    let labels = host_labels(host)?;
    let name = labels.join(".");
    let Some(share) = &world.machine.share else {
        return Err(NotFound::UnknownPublicLabel { label: name });
    };
    let domain = share.domain.to_ascii_lowercase();
    let label = name
        .strip_suffix(&domain)
        .and_then(|l| l.strip_suffix('.'))
        .ok_or_else(|| NotFound::UnknownTld {
            host: name.clone(),
            tld: domain.clone(),
        })?;
    if label.contains('.') {
        return Err(NotFound::UnknownPublicLabel {
            label: label.to_string(),
        });
    }
    resolve_public_label(world, label).ok_or_else(|| NotFound::UnknownPublicLabel {
        label: label.to_string(),
    })
}

/// Finds the instance whose `public_label` is `label`, including shortened stack names.
pub fn resolve_public_label(world: &World, label: &str) -> Option<InstanceKey> {
    let project_name = label.rsplit("--").next()?;
    let state = world.project(project_name)?;
    let stacks: Vec<Stack> = std::iter::once(Stack::Main)
        .chain(state.stacks().into_iter().map(Stack::Branch))
        .collect();
    state.project.apps.iter().find_map(|app| {
        stacks.iter().find_map(|stack| {
            (public_label(state, app, stack).ok()? == label).then(|| InstanceKey {
                project: state.project.name.clone(),
                app: app.name.clone(),
                stack: stack.clone(),
            })
        })
    })
}

/// Stacks whose slug equals an app's label: the app wins, so the stack is unreachable there.
pub fn collisions(state: &ProjectState) -> Vec<Problem> {
    let project = &state.project;
    state
        .stacks()
        .into_iter()
        .filter_map(|slug| {
            let app = labelled_app(project, &slug)?;
            Some(Problem {
                project: Some(project.name.clone()),
                app: Some(app.name.clone()),
                stack: Some(slug.clone()),
                message: format!(
                    "stack {slug} has the same name as app {}: {slug}.{} goes to the app, \
                     so the stack is unreachable by hostname; rename its worktree folders",
                    app.name, project.name
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Worktree;
    use crate::test_support::{self, app, project, state, worktree, world};
    use proptest::prelude::*;
    use sha2::{Digest, Sha256};

    fn branch(slug: &str) -> Stack {
        Stack::Branch(slug.into())
    }

    fn key(project: &str, app: &str, stack: Stack) -> InstanceKey {
        InstanceKey {
            project: project.into(),
            app: app.into(),
            stack,
        }
    }

    fn resolved(project: &str, app: &str, stack: Stack, subdomains: &[&str]) -> Resolved {
        Resolved {
            key: key(project, app, stack),
            subdomains: subdomains.iter().map(|s| s.to_string()).collect(),
            via_alias: false,
        }
    }

    fn host(state: &ProjectState, app: &str, stack: &Stack) -> String {
        local_host(
            "test",
            &state.project,
            state.project.app(app).unwrap(),
            stack,
        )
    }

    fn with_branch(branch_name: &str) -> ProjectState {
        let mut nova = test_support::nova();
        nova.worktrees
            .get_mut("nova-postcard-api")
            .unwrap()
            .push(worktree(
                branch_name,
                "/Users/dev/work/long/nova-postcard-api",
            ));
        nova
    }

    #[test]
    fn local_hosts_follow_the_hierarchy() {
        let nova = test_support::nova();
        assert_eq!(
            host(&nova, "nova-postcard-frontend", &Stack::Main),
            "postcard-frontend.nova.test"
        );
        assert_eq!(
            host(&nova, "nova-postcard-frontend", &branch("feature-x")),
            "postcard-frontend.feature-x.nova.test"
        );
        assert_eq!(
            host(&nova, "nova-billing", &branch("claude-fix-thing-123")),
            "billing.claude-fix-thing-123.nova.test"
        );
    }

    #[test]
    fn collapsed_apps_omit_their_label() {
        let acme = test_support::acme();
        assert_eq!(host(&acme, "acme", &Stack::Main), "acme.test");
        assert_eq!(
            host(&acme, "acme", &branch("feature-x")),
            "feature-x.acme.test"
        );
        assert_eq!(host(&acme, "acme-api", &Stack::Main), "api.acme.test");
        assert_eq!(
            host(&acme, "acme-api", &branch("feature-x")),
            "api.feature-x.acme.test"
        );
    }

    #[test]
    fn urls_include_only_non_default_ports() {
        let nova = test_support::nova();
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        let mut machine = MachineConfig {
            tld: "test".into(),
            ..MachineConfig::default()
        };
        assert_eq!(
            instance_url(&machine, &nova.project, frontend, &branch("feature-x")),
            "https://postcard-frontend.feature-x.nova.test"
        );
        machine.https_port = 8443;
        machine.http_port = 8080;
        machine.tld = "localhost".into();
        assert_eq!(
            instance_url(&machine, &nova.project, frontend, &Stack::Main),
            "https://postcard-frontend.nova.localhost:8443"
        );
        let mut http = nova.project.clone();
        http.scheme = Scheme::Http;
        assert_eq!(
            instance_url(&machine, &http, frontend, &Stack::Main),
            "http://postcard-frontend.nova.localhost:8080"
        );
        machine.http_port = 80;
        assert_eq!(
            instance_url(&machine, &http, frontend, &Stack::Main),
            "http://postcard-frontend.nova.localhost"
        );
    }

    #[test]
    fn tld_length_counts_labels_after_the_app() {
        let nova = test_support::nova();
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        assert_eq!(tld_length("test", &nova.project, frontend, &Stack::Main), 2);
        assert_eq!(
            tld_length("test", &nova.project, frontend, &branch("feature-x")),
            3
        );
        let eventportal = test_support::eventportal();
        let app = &eventportal.project.apps[0];
        assert_eq!(
            tld_length("test", &eventportal.project, app, &Stack::Main),
            1
        );
        assert_eq!(
            tld_length("test", &eventportal.project, app, &branch("feature-x")),
            2
        );
        assert_eq!(
            tld_length("dev.local", &eventportal.project, app, &Stack::Main),
            2
        );
    }

    #[test]
    fn public_labels_join_local_labels() {
        let nova = test_support::nova();
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        assert_eq!(
            public_label(&nova, frontend, &branch("feature-x")).unwrap(),
            "postcard-frontend--feature-x--nova"
        );
        assert_eq!(
            public_label(&nova, frontend, &Stack::Main).unwrap(),
            "postcard-frontend--nova"
        );
        let acme = test_support::acme();
        let collapsed = acme.project.app("acme").unwrap();
        assert_eq!(
            public_label(&acme, collapsed, &Stack::Main).unwrap(),
            "acme"
        );
        assert_eq!(
            public_label(&acme, collapsed, &branch("feature-x")).unwrap(),
            "feature-x--acme"
        );
        let share = world().machine.share.unwrap();
        assert_eq!(
            public_url(&share, &nova, frontend, &branch("feature-x")).unwrap(),
            "https://postcard-frontend--feature-x--nova.alex.carte.dev"
        );
    }

    #[test]
    fn long_public_labels_shorten_the_stack_name() {
        let long = "claude/implement-the-entire-newsletter-subscription-flow-with-tests";
        let nova = with_branch(long);
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        let slug = crate::slug::slugify(long);
        let label = public_label(&nova, frontend, &branch(&slug)).unwrap();
        assert_eq!(label.len(), 63);
        assert!(label.starts_with("postcard-frontend--claude-implement-the-entire-"));
        assert!(label.ends_with("--nova"));
        assert_eq!(label.matches("--").count(), 2);
        let hash = Sha256::digest(slug.as_bytes());
        let hex: String = hash[..3].iter().map(|b| format!("{b:02x}")).collect();
        assert!(label.contains(&format!("-{hex}--")), "{label}");
        assert_eq!(
            public_label(&nova, frontend, &branch(&slug)).unwrap(),
            label
        );
    }

    #[test]
    fn shortened_slugs_never_end_in_a_dash_before_the_hash() {
        // postcard-frontend--<slug>--nova leaves 38 characters for the slug, 31 before the hash;
        // the cut lands on the dash after the a's.
        let long = format!("{}-{}", "a".repeat(30), "b".repeat(40));
        let nova = with_branch(&long);
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        let label = public_label(&nova, frontend, &branch(&long)).unwrap();
        assert_eq!(label.matches("--").count(), 2, "{label}");
        assert!(label.len() < 63);
    }

    #[test]
    fn public_labels_that_cannot_fit_are_errors() {
        let long_app = "x".repeat(57);
        let nova = state(
            project("nova", None, vec![app(&long_app, &long_app, "/x")]),
            vec![(long_app.as_str(), vec![worktree("feature-x", "/y")])],
        );
        let long = &nova.project.apps[0];
        assert!(public_label(&nova, long, &Stack::Main).is_ok());
        assert!(matches!(
            public_label(&nova, long, &branch("feature-x")),
            Err(NamingError::PublicLabelTooLong { .. })
        ));
        let longer = "y".repeat(63);
        let nova = state(
            project("nova", None, vec![app(&longer, &longer, "/x")]),
            Vec::new(),
        );
        assert!(matches!(
            public_label(&nova, &nova.project.apps[0], &Stack::Main),
            Err(NamingError::PublicLabelTooLong { .. })
        ));
    }

    #[test]
    fn shortening_needs_a_known_stack() {
        let nova = test_support::nova();
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        let slug = "z".repeat(50);
        assert_eq!(
            public_label(&nova, frontend, &branch(&slug)),
            Err(NamingError::UnknownStack {
                project: "nova".into(),
                stack: slug
            })
        );
    }

    #[test]
    fn localhost_works_like_the_tld() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "postcard-frontend.nova.localhost"),
            resolve_host(&world, "postcard-frontend.nova.test")
        );
        assert_eq!(
            resolve_host(&world, "oslo.feature-x.eventportal.localhost:443"),
            resolve_host(&world, "oslo.feature-x.eventportal.test")
        );
        assert!(matches!(
            resolve_host(&world, "nova.example.com"),
            Err(NotFound::UnknownTld { .. })
        ));
    }

    #[test]
    fn resolves_main_and_stack_hosts() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "postcard-frontend.nova.test"),
            Ok(resolved("nova", "nova-postcard-frontend", Stack::Main, &[]))
        );
        assert_eq!(
            resolve_host(&world, "postcard-frontend.feature-x.nova.test"),
            Ok(resolved(
                "nova",
                "nova-postcard-frontend",
                branch("feature-x"),
                &[]
            ))
        );
        assert_eq!(
            resolve_host(&world, "billing.claude-fix-thing-123.nova.test"),
            Ok(resolved(
                "nova",
                "nova-billing",
                branch("claude-fix-thing-123"),
                &[]
            ))
        );
    }

    #[test]
    fn normalises_the_host_header() {
        let world = world();
        let expected = Ok(resolved("nova", "nova-postcard-frontend", Stack::Main, &[]));
        assert_eq!(
            resolve_host(&world, "Postcard-Frontend.NOVA.test:443"),
            expected
        );
        assert_eq!(
            resolve_host(&world, "postcard-frontend.nova.test."),
            expected
        );
        assert_eq!(
            resolve_host(&world, "postcard-frontend.nova.test.:8443"),
            expected
        );
    }

    #[test]
    fn rejects_malformed_and_foreign_hosts() {
        let world = world();
        assert!(matches!(
            resolve_host(&world, "[::1]:443"),
            Err(NotFound::InvalidHost { .. })
        ));
        assert!(matches!(
            resolve_host(&world, "a..nova.test"),
            Err(NotFound::InvalidHost { .. })
        ));
        assert!(matches!(
            resolve_host(&world, "localhost"),
            Err(NotFound::UnknownTld { .. })
        ));
        assert!(matches!(
            resolve_host(&world, "test"),
            Err(NotFound::UnknownTld { .. })
        ));
        assert!(matches!(
            resolve_host(&world, "nova.example.com"),
            Err(NotFound::UnknownTld { .. })
        ));
        assert_eq!(
            resolve_host(&world, "frontend.nope.test"),
            Err(NotFound::UnknownProject {
                label: "nope".into()
            })
        );
    }

    #[test]
    fn reports_unknown_apps_and_stacks() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "nova.test"),
            Err(NotFound::NoApp {
                project: "nova".into(),
                stack: None
            })
        );
        assert_eq!(
            resolve_host(&world, "feature-x.nova.test"),
            Err(NotFound::NoApp {
                project: "nova".into(),
                stack: Some("feature-x".into())
            })
        );
        assert_eq!(
            resolve_host(&world, "frontend.nova.test"),
            Err(NotFound::UnknownName {
                project: "nova".into(),
                stack: None,
                label: "frontend".into()
            })
        );
        assert_eq!(
            resolve_host(&world, "frontend.feature-x.nova.test"),
            Err(NotFound::UnknownName {
                project: "nova".into(),
                stack: Some("feature-x".into()),
                label: "frontend".into()
            })
        );
        assert_eq!(
            resolve_host(&world, "postcard-frontend.feature-y.nova.test"),
            Err(NotFound::UnknownName {
                project: "nova".into(),
                stack: None,
                label: "feature-y".into()
            })
        );
    }

    #[test]
    fn subdomains_need_opt_in() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "www.postcard-frontend.nova.test"),
            Err(NotFound::SubdomainsNotAllowed {
                key: key("nova", "nova-postcard-frontend", Stack::Main),
                subdomains: vec!["www".into()],
            })
        );
        assert_eq!(
            resolve_host(&world, "admin.api.feature-x.acme.test"),
            Err(NotFound::SubdomainsNotAllowed {
                key: key("acme", "acme-api", branch("feature-x")),
                subdomains: vec!["admin".into()],
            })
        );
    }

    #[test]
    fn collapsed_single_app_with_subdomains() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "eventportal.test"),
            Ok(resolved("eventportal", "eventportal", Stack::Main, &[]))
        );
        assert_eq!(
            resolve_host(&world, "oslo.eventportal.test"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                Stack::Main,
                &["oslo"]
            ))
        );
        assert_eq!(
            resolve_host(&world, "feature-x.eventportal.test"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                branch("feature-x"),
                &[]
            ))
        );
        assert_eq!(
            resolve_host(&world, "oslo.feature-x.eventportal.test"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                branch("feature-x"),
                &["oslo"]
            ))
        );
        assert_eq!(
            resolve_host(&world, "admin.oslo.eventportal.test"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                Stack::Main,
                &["admin", "oslo"]
            ))
        );
        assert_eq!(
            resolve_host(&world, "feature-y.eventportal.test"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                Stack::Main,
                &["feature-y"]
            ))
        );
    }

    #[test]
    fn collapsed_app_next_to_labelled_apps() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "acme.test"),
            Ok(resolved("acme", "acme", Stack::Main, &[]))
        );
        assert_eq!(
            resolve_host(&world, "api.acme.test"),
            Ok(resolved("acme", "acme-api", Stack::Main, &[]))
        );
        assert_eq!(
            resolve_host(&world, "feature-x.acme.test"),
            Ok(resolved("acme", "acme", branch("feature-x"), &[]))
        );
        assert_eq!(
            resolve_host(&world, "api.feature-x.acme.test"),
            Ok(resolved("acme", "acme-api", branch("feature-x"), &[]))
        );
        assert_eq!(
            resolve_host(&world, "feature-y.acme.test"),
            Err(NotFound::UnknownName {
                project: "acme".into(),
                stack: None,
                label: "feature-y".into()
            })
        );
        assert_eq!(
            resolve_host(&world, "web.feature-x.acme.test"),
            Err(NotFound::UnknownName {
                project: "acme".into(),
                stack: Some("feature-x".into()),
                label: "web".into()
            })
        );
        assert_eq!(
            resolve_host(&world, "acme.acme.test"),
            Err(NotFound::UnknownName {
                project: "acme".into(),
                stack: None,
                label: "acme".into()
            })
        );
    }

    #[test]
    fn app_label_wins_over_a_stack_of_the_same_name() {
        let mut nova = test_support::nova();
        nova.worktrees
            .get_mut("nova-postcard-api")
            .unwrap()
            .push(worktree(
                "billing",
                "/Users/dev/work/billing/nova-postcard-api",
            ));
        let world = World {
            projects: vec![nova],
            ..test_support::world()
        };
        assert_eq!(
            resolve_host(&world, "billing.nova.test"),
            Ok(resolved("nova", "nova-billing", Stack::Main, &[]))
        );
        assert!(matches!(
            resolve_host(&world, "postcard-api.billing.nova.test"),
            Err(NotFound::SubdomainsNotAllowed { .. })
        ));
    }

    #[test]
    fn aliases_route_to_main() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "nova-postcard-frontend.test"),
            Ok(Resolved {
                via_alias: true,
                ..resolved("nova", "nova-postcard-frontend", Stack::Main, &[])
            })
        );
        assert_eq!(
            resolve_host(&world, "www.nova-postcard-frontend.test"),
            Err(NotFound::SubdomainsNotAllowed {
                key: key("nova", "nova-postcard-frontend", Stack::Main),
                subdomains: vec!["www".into()],
            })
        );
    }

    #[test]
    fn resolves_with_a_custom_tld() {
        let mut world = world();
        world.machine.tld = "dev.local".into();
        assert_eq!(
            resolve_host(&world, "oslo.eventportal.dev.local"),
            Ok(resolved(
                "eventportal",
                "eventportal",
                Stack::Main,
                &["oslo"]
            ))
        );
        assert!(matches!(
            resolve_host(&world, "eventportal.test"),
            Err(NotFound::UnknownTld { .. })
        ));
    }

    #[test]
    fn every_local_host_resolves_back() {
        let world = world();
        for state in &world.projects {
            let stacks =
                std::iter::once(Stack::Main).chain(state.stacks().into_iter().map(Stack::Branch));
            for stack in stacks {
                for app in &state.project.apps {
                    let host = local_host("test", &state.project, app, &stack);
                    let resolved = resolve_host(&world, &host).unwrap();
                    assert_eq!(
                        resolved.key,
                        key(&state.project.name, &app.name, stack.clone())
                    );
                    let label = public_label(state, app, &stack).unwrap();
                    assert_eq!(resolve_public_label(&world, &label), Some(resolved.key));
                }
            }
        }
    }

    #[test]
    fn resolves_public_hosts() {
        let world = world();
        assert_eq!(
            resolve_public_host(&world, "postcard-frontend--feature-x--nova.alex.carte.dev"),
            Ok(key("nova", "nova-postcard-frontend", branch("feature-x")))
        );
        assert_eq!(
            resolve_public_host(&world, "acme.alex.carte.dev:443"),
            Ok(key("acme", "acme", Stack::Main))
        );
        assert_eq!(
            resolve_public_label(&world, "feature-x--acme"),
            Some(key("acme", "acme", branch("feature-x")))
        );
        assert_eq!(
            resolve_public_label(&world, "billing--claude-fix-thing-123--nova"),
            Some(key("nova", "nova-billing", branch("claude-fix-thing-123")))
        );
        assert_eq!(resolve_public_label(&world, "frontend--nova"), None);
        assert_eq!(resolve_public_label(&world, "nope"), None);
        assert!(matches!(
            resolve_public_host(&world, "oslo.eventportal.alex.carte.dev"),
            Err(NotFound::UnknownPublicLabel { .. })
        ));
        assert!(matches!(
            resolve_public_host(&world, "acme.test"),
            Err(NotFound::UnknownTld { .. })
        ));
    }

    #[test]
    fn resolves_shortened_public_labels() {
        let long = "claude/implement-the-entire-newsletter-subscription-flow-with-tests";
        let mut world = world();
        world.projects[0] = with_branch(long);
        let slug = crate::slug::slugify(long);
        let nova = &world.projects[0];
        let frontend = nova.project.app("nova-postcard-frontend").unwrap();
        let label = public_label(nova, frontend, &branch(&slug)).unwrap();
        assert_ne!(label, format!("postcard-frontend--{slug}--nova"));
        assert_eq!(
            resolve_public_label(&world, &label),
            Some(key("nova", "nova-postcard-frontend", branch(&slug)))
        );
    }

    #[test]
    fn a_single_host_provider_shares_on_its_domain() {
        let mut world = world();
        let share = world.machine.share.as_mut().unwrap();
        share.provider = "ngrok".into();
        share.domain = "alex.ngrok-free.app".into();
        let share = share.clone();
        let nova = &world.projects[0];
        let billing = nova.project.app("nova-billing").unwrap();
        assert_eq!(
            public_url(&share, nova, billing, &branch("feature-x")),
            Ok("https://alex.ngrok-free.app".to_string())
        );
    }

    #[test]
    fn public_resolution_needs_a_share_domain() {
        let mut world = world();
        world.machine.share = None;
        assert!(resolve_public_host(&world, "acme.alex.carte.dev").is_err());
    }

    #[test]
    fn a_stack_has_a_branch_only_when_its_worktrees_agree() {
        let mut nova = test_support::nova();
        assert_eq!(stack_branch(&nova, "feature-x"), Some("feature-x"));
        assert_eq!(stack_branch(&nova, "nope"), None);
        nova.worktrees.get_mut("nova-postcard-api").unwrap()[0].branch = Some("other".into());
        assert_eq!(stack_branch(&nova, "feature-x"), None);
        nova.worktrees.get_mut("nova-postcard-api").unwrap()[0].branch = None;
        assert_eq!(stack_branch(&nova, "feature-x"), None);
    }

    #[test]
    fn finds_stacks_named_like_apps() {
        assert!(collisions(&test_support::nova()).is_empty());
        let mut nova = test_support::nova();
        nova.worktrees
            .get_mut("nova-postcard-api")
            .unwrap()
            .push(worktree(
                "Billing",
                "/Users/dev/work/billing/nova-postcard-api",
            ));
        let problems = collisions(&nova);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].project.as_deref(), Some("nova"));
        assert_eq!(problems[0].app.as_deref(), Some("nova-billing"));
        assert!(
            problems[0].message.contains("stack billing"),
            "{}",
            problems[0].message
        );
    }

    #[test]
    fn a_stack_named_after_a_collapsed_app_is_no_collision() {
        let mut acme = test_support::acme();
        acme.worktrees
            .get_mut("acme-api")
            .unwrap()
            .push(worktree("acme", "/Users/dev/clients/acme/acme-api"));
        assert!(collisions(&acme).is_empty());
    }

    #[test]
    fn not_found_messages_read_well() {
        assert_eq!(
            resolve_host(&world(), "frontend.nova.test")
                .unwrap_err()
                .to_string(),
            "no app or stack named frontend in project nova"
        );
        assert_eq!(
            resolve_host(&world(), "www.postcard-frontend.nova.test")
                .unwrap_err()
                .to_string(),
            "app nova-postcard-frontend doesn't allow subdomains (www)"
        );
    }

    /// A project with distinct app labels, maybe a collapsed app, and stacks that
    /// don't collide with an app.
    fn generated_project() -> impl Strategy<Value = ProjectState> {
        (
            "[a-z]{3,8}",
            any::<bool>(),
            proptest::collection::btree_set("[a-z]{1,6}(-[a-z0-9]{1,4})?", 1..4),
            proptest::collection::vec("[a-zA-Z0-9/_.-]{1,90}", 0..4),
        )
            .prop_map(|(name, collapsed, shorts, branches)| {
                let mut apps: Vec<App> = shorts
                    .iter()
                    .filter(|short| **short != name)
                    .map(|short| app(&format!("{name}-{short}"), short, "/w/app"))
                    .collect();
                if collapsed || apps.is_empty() {
                    apps.push(app(&name, &name, "/w/app"));
                }
                let mut worktrees: Vec<(&str, Vec<Worktree>)> =
                    apps.iter().map(|a| (a.name.as_str(), Vec::new())).collect();
                for (i, branch) in branches.iter().enumerate() {
                    let wt = worktree(branch, "/w/stack");
                    if !wt.slug.is_empty() && !shorts.contains(&wt.slug) {
                        let n = worktrees.len();
                        worktrees[i % n].1.push(wt);
                    }
                }
                state(
                    project(&name, Some(&format!("{name}-")), apps.clone()),
                    worktrees,
                )
            })
    }

    fn generated_world() -> impl Strategy<Value = World> {
        proptest::collection::vec(generated_project(), 1..4).prop_map(|projects| {
            let mut world = world();
            world.projects = Vec::new();
            for state in projects {
                if world.project(&state.project.name).is_none() {
                    world.projects.push(state);
                }
            }
            world
        })
    }

    fn every_instance(world: &World) -> Vec<(&ProjectState, &App, Stack)> {
        world
            .projects
            .iter()
            .flat_map(|state| {
                let stacks: Vec<Stack> = std::iter::once(Stack::Main)
                    .chain(state.stacks().into_iter().map(Stack::Branch))
                    .collect();
                state.project.apps.iter().flat_map(move |app| {
                    stacks
                        .clone()
                        .into_iter()
                        .map(move |stack| (state, app, stack))
                })
            })
            .collect()
    }

    /// `labels_under_tld`, with an invalid host's text left out.
    fn labels_or_reason(world: &World, host: &str) -> Result<Vec<String>, Option<NotFound>> {
        labels_under_tld(world, host).map_err(|e| match e {
            NotFound::InvalidHost { .. } => None,
            other => Some(other),
        })
    }

    proptest! {
        #[test]
        fn local_hosts_resolve_to_their_instance(world in generated_world()) {
            for (state, app, stack) in every_instance(&world) {
                let host = local_host(&world.machine.tld, &state.project, app, &stack);
                let expected = resolved(&state.project.name, &app.name, stack, &[]);
                prop_assert_eq!(resolve_host(&world, &host), Ok(expected), "{}", host);
            }
        }

        #[test]
        fn public_labels_resolve_to_their_instance(world in generated_world()) {
            for (state, app, stack) in every_instance(&world) {
                let Ok(label) = public_label(state, app, &stack) else {
                    continue;
                };
                prop_assert!(label.len() <= 63, "{}", label);
                prop_assert!(!label.contains("---"), "{}", label);
                let expected = key(&state.project.name, &app.name, stack.clone());
                prop_assert_eq!(resolve_public_label(&world, &label), Some(expected), "{}", label);
            }
        }

        #[test]
        fn any_host_is_handled(host in "\\PC{0,30}|[a-zA-Z0-9.:\\[\\]-]{0,30}") {
            let world = world();
            let _ = host_labels(&host);
            let _ = labels_under_tld(&world, &host);
            let _ = resolve_host(&world, &host);
            let _ = resolve_public_host(&world, &host);
        }

        #[test]
        fn case_a_trailing_dot_and_a_port_dont_matter(
            labels in proptest::collection::vec("[a-zA-Z0-9-]{0,6}|test|localhost|nova", 1..5),
            dot in any::<bool>(),
            port in proptest::option::of(any::<u16>()),
        ) {
            let world = world();
            let host = labels.join(".");
            prop_assume!(!host.ends_with('.'));
            let mut variant = host.to_ascii_uppercase();
            if dot {
                variant.push('.');
            }
            if let Some(port) = port {
                variant.push_str(&format!(":{port}"));
            }
            prop_assert_eq!(
                host_labels(&host).ok(),
                host_labels(&variant).ok(),
                "{} vs {}", host, variant
            );
            prop_assert_eq!(labels_or_reason(&world, &host), labels_or_reason(&world, &variant));
        }
    }
}
