use std::fmt;
use std::path::Path;
use std::str::FromStr;

use thiserror::Error;

use crate::model::{App, InstanceKey, ProjectState, Stack, World};
use crate::slug::branch_slug;

/// A CLI address `app@branch/process` as typed, every part optional.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Address {
    pub app: Option<String>,
    pub branch: Option<String>,
    pub process: Option<String>,
}

impl FromStr for Address {
    type Err = AddressError;

    /// Branch names may contain `/`, so after `@` the process is whatever follows the
    /// last `/`. `resolve` reads `@claude/fix-thing-123` as a branch when that stack exists.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| AddressError::Invalid {
            input: input.to_string(),
            reason,
        };
        let part = |s: &str, reason| {
            if s.is_empty() {
                Err(invalid(reason))
            } else {
                Ok(Some(s.to_string()))
            }
        };
        let input_trimmed = input.trim();
        if input_trimmed.is_empty() {
            return Ok(Address::default());
        }
        let (app, rest) = match input_trimmed.split_once('@') {
            Some((app, rest)) => (app, Some(rest)),
            None => (input_trimmed, None),
        };
        let mut address = Address::default();
        match rest {
            Some(rest) => {
                if !app.is_empty() {
                    address.app = part(app, "empty app")?;
                }
                if rest.contains('@') {
                    return Err(invalid("more than one @"));
                }
                match rest.rsplit_once('/') {
                    Some((branch, process)) => {
                        address.branch = part(branch, "empty branch after @")?;
                        address.process = part(process, "empty process after /")?;
                    }
                    None => address.branch = part(rest, "empty branch after @")?,
                }
            }
            None => match app.split_once('/') {
                Some((app, process)) => {
                    if !app.is_empty() {
                        address.app = part(app, "empty app")?;
                    }
                    if process.contains('/') {
                        return Err(invalid("more than one / without @branch"));
                    }
                    address.process = part(process, "empty process after /")?;
                }
                None => address.app = part(app, "empty app")?,
            },
        }
        Ok(address)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(app) = &self.app {
            f.write_str(app)?;
        }
        if let Some(branch) = &self.branch {
            write!(f, "@{branch}")?;
        }
        if let Some(process) = &self.process {
            write!(f, "/{process}")?;
        }
        Ok(())
    }
}

/// What an address points at. `app: None` addresses every app (of `stack`, or the whole
/// project when `stack` is also `None`); `process: None` addresses every process.
/// When `app` is set, `stack` is always set too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub project: String,
    pub app: Option<String>,
    pub stack: Option<Stack>,
    pub process: Option<String>,
}

impl Target {
    pub fn instance(&self) -> Option<InstanceKey> {
        Some(InstanceKey {
            project: self.project.clone(),
            app: self.app.clone()?,
            stack: self.stack.clone()?,
        })
    }
}

/// The checkout a directory belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub project: String,
    pub app: String,
    pub stack: Stack,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AddressError {
    #[error("invalid address {input:?}: {reason}")]
    Invalid { input: String, reason: &'static str },
    #[error("no {kind} matches {input:?}{}", candidate_list(candidates))]
    NoMatch {
        kind: &'static str,
        input: String,
        candidates: Vec<String>,
    },
    #[error("{input:?} matches more than one {kind}: {}", candidates.join(", "))]
    Ambiguous {
        kind: &'static str,
        input: String,
        candidates: Vec<String>,
    },
    #[error("no address given, and the current directory is not inside an app's checkout")]
    NoContext,
}

fn candidate_list(candidates: &[String]) -> String {
    if candidates.is_empty() {
        String::new()
    } else {
        format!("; choose from {}", candidates.join(", "))
    }
}

/// Finds the app checkout (main or a worktree) containing `dir`, preferring the deepest,
/// since worktrees can live inside the main checkout.
pub fn locate(world: &World, dir: &Path) -> Option<Location> {
    world
        .projects
        .iter()
        .flat_map(|state| {
            state.project.apps.iter().flat_map(move |app| {
                let worktrees = state.worktrees.get(&app.name).into_iter().flatten();
                std::iter::once((app.path.as_path(), Stack::Main))
                    .chain(worktrees.map(|wt| (wt.path.as_path(), Stack::Branch(wt.slug.clone()))))
                    .filter(|(path, _)| dir.starts_with(path))
                    .map(move |(path, stack)| {
                        let location = Location {
                            project: state.project.name.clone(),
                            app: app.name.clone(),
                            stack,
                        };
                        (path.components().count(), location)
                    })
            })
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, location)| location)
}

/// Resolves an address against the world, filling omitted parts from `cwd`:
/// the project always, the stack when no branch is given, and the app only when
/// neither app nor branch is given (so `@feature-x` means the whole stack).
pub fn resolve(
    world: &World,
    address: &Address,
    cwd: Option<&Path>,
) -> Result<Target, AddressError> {
    let here = cwd.and_then(|dir| locate(world, dir));

    let (state, app) = match (&address.app, &here) {
        (Some(query), _) => match find_app(world, query, here.as_ref())? {
            Found::App(state, app) => (Some(state), Some(app)),
            Found::Project(state) => (Some(state), None),
        },
        (None, Some(here)) => {
            let state = world
                .project(&here.project)
                .expect("located in a known project");
            let app = address
                .branch
                .is_none()
                .then(|| state.project.app(&here.app))
                .flatten();
            (Some(state), app)
        }
        (None, None) if address.branch.is_some() => (None, None),
        (None, None) => return Err(AddressError::NoContext),
    };

    let (state, stack, process) = match (&address.branch, state) {
        (Some(branch), Some(state)) => {
            let (stack, process) = find_stack(state, branch, address.process.as_deref())?;
            (state, Some(stack), process)
        }
        (Some(branch), None) => find_stack_anywhere(world, branch, address.process.as_deref())?,
        (None, Some(state)) => {
            let stack = app.map(|_| match &here {
                Some(here) if here.project == state.project.name => here.stack.clone(),
                _ => Stack::Main,
            });
            (state, stack, address.process.clone())
        }
        (None, None) => unreachable!("no branch and no project returns NoContext"),
    };

    Ok(Target {
        project: state.project.name.clone(),
        app: app.map(|a| a.name.clone()),
        stack,
        process,
    })
}

enum Found<'a> {
    App(&'a ProjectState, &'a App),
    Project(&'a ProjectState),
}

/// Exact app names win over a project name, which wins over substrings. Matches in the
/// current directory's project are preferred over matches elsewhere.
fn find_app<'a>(
    world: &'a World,
    query: &str,
    here: Option<&Location>,
) -> Result<Found<'a>, AddressError> {
    let query_lower = query.to_lowercase();
    let all: Vec<(&ProjectState, &App)> = world
        .projects
        .iter()
        .flat_map(|state| state.project.apps.iter().map(move |app| (state, app)))
        .collect();
    let scoped = |pred: &dyn Fn(&App) -> bool| -> Vec<(&'a ProjectState, &'a App)> {
        let matches: Vec<_> = all.iter().copied().filter(|(_, app)| pred(app)).collect();
        let local: Vec<_> = matches
            .iter()
            .copied()
            .filter(|(state, _)| here.is_some_and(|h| h.project == state.project.name))
            .collect();
        if local.is_empty() { matches } else { local }
    };
    let one = |matches: Vec<(&'a ProjectState, &'a App)>| match matches.as_slice() {
        [(state, app)] => Ok(Some(Found::App(state, app))),
        [] => Ok(None),
        _ => Err(AddressError::Ambiguous {
            kind: "app",
            input: query.to_string(),
            candidates: app_names(&matches),
        }),
    };

    let exact = scoped(&|app| {
        app.name.to_lowercase() == query_lower || app.short.to_lowercase() == query_lower
    });
    if let Some(found) = one(exact)? {
        return Ok(found);
    }
    if let Some(state) = world.project(&query_lower) {
        return Ok(Found::Project(state));
    }
    let partial = scoped(&|app| {
        app.name.to_lowercase().contains(&query_lower)
            || app.short.to_lowercase().contains(&query_lower)
    });
    if let Some(found) = one(partial)? {
        return Ok(found);
    }
    let candidates: Vec<_> = all
        .iter()
        .copied()
        .filter(|(state, _)| here.is_none_or(|h| h.project == state.project.name))
        .collect();
    Err(AddressError::NoMatch {
        kind: "app",
        input: query.to_string(),
        candidates: app_names(&candidates),
    })
}

fn app_names(matches: &[(&ProjectState, &App)]) -> Vec<String> {
    let one_project = matches
        .windows(2)
        .all(|w| w[0].0.project.name == w[1].0.project.name);
    matches
        .iter()
        .map(|(state, app)| {
            if one_project {
                app.name.clone()
            } else {
                format!("{} ({})", app.name, state.project.name)
            }
        })
        .collect()
}

/// `main` is the main checkout; anything else is matched as a slug, exactly or as an
/// unambiguous substring. A process part that completes a branch name containing `/`
/// is folded back into the branch.
fn find_stack(
    state: &ProjectState,
    branch: &str,
    process: Option<&str>,
) -> Result<(Stack, Option<String>), AddressError> {
    let stacks = state.stacks();
    if let Some(process) = process {
        let joined = branch_slug(&format!("{branch}/{process}"));
        if stacks.contains(&joined) {
            return Ok((Stack::Branch(joined), None));
        }
    }
    let process = process.map(Into::into);
    if branch.eq_ignore_ascii_case("main") {
        return Ok((Stack::Main, process));
    }
    let slug = branch_slug(branch);
    if stacks.contains(&slug) {
        return Ok((Stack::Branch(slug), process));
    }
    let partial: Vec<String> = stacks
        .iter()
        .filter(|s| !slug.is_empty() && s.contains(&slug))
        .cloned()
        .collect();
    match partial.as_slice() {
        [one] => Ok((Stack::Branch(one.clone()), process)),
        [] => Err(AddressError::NoMatch {
            kind: "stack",
            input: branch.to_string(),
            candidates: stacks.into_iter().collect(),
        }),
        _ => Err(AddressError::Ambiguous {
            kind: "stack",
            input: branch.to_string(),
            candidates: partial,
        }),
    }
}

fn find_stack_anywhere<'a>(
    world: &'a World,
    branch: &str,
    process: Option<&str>,
) -> Result<(&'a ProjectState, Option<Stack>, Option<String>), AddressError> {
    let found: Vec<_> = world
        .projects
        .iter()
        .filter_map(|state| {
            let (stack, process) = find_stack(state, branch, process).ok()?;
            Some((state, Some(stack), process))
        })
        .collect();
    match found.len() {
        1 => Ok(found.into_iter().next().expect("one element")),
        0 => Err(AddressError::NoMatch {
            kind: "stack",
            input: branch.to_string(),
            candidates: world
                .projects
                .iter()
                .flat_map(|state| {
                    state
                        .stacks()
                        .into_iter()
                        .map(|s| format!("{s} ({})", state.project.name))
                })
                .collect(),
        }),
        _ => Err(AddressError::Ambiguous {
            kind: "stack",
            input: branch.to_string(),
            candidates: found
                .iter()
                .map(|(state, stack, _)| {
                    let slug = stack.as_ref().map_or("main", Stack::slug);
                    format!("{slug} ({})", state.project.name)
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::test_support::{worktree, world};

    fn parse(input: &str) -> Address {
        input.parse().unwrap()
    }

    fn address(app: Option<&str>, branch: Option<&str>, process: Option<&str>) -> Address {
        Address {
            app: app.map(Into::into),
            branch: branch.map(Into::into),
            process: process.map(Into::into),
        }
    }

    fn target(
        project: &str,
        app: Option<&str>,
        stack: Option<Stack>,
        process: Option<&str>,
    ) -> Target {
        Target {
            project: project.into(),
            app: app.map(Into::into),
            stack,
            process: process.map(Into::into),
        }
    }

    fn branch(slug: &str) -> Stack {
        Stack::Branch(slug.into())
    }

    fn resolve_str(input: &str, cwd: Option<&str>) -> Result<Target, AddressError> {
        let cwd = cwd.map(PathBuf::from);
        resolve(&world(), &parse(input), cwd.as_deref())
    }

    const FRONTEND_MAIN: &str = "/Users/dev/work/nova-postcard-frontend/app/models";
    const FRONTEND_FEATURE_X: &str = "/Users/dev/work/nova-postcard-frontend-feature-x/app";

    #[test]
    fn parses_every_combination() {
        assert_eq!(parse(""), Address::default());
        assert_eq!(parse("postcard-f"), address(Some("postcard-f"), None, None));
        assert_eq!(parse("@feature-x"), address(None, Some("feature-x"), None));
        assert_eq!(
            parse("@feature-x/worker"),
            address(None, Some("feature-x"), Some("worker"))
        );
        assert_eq!(parse("/web"), address(None, None, Some("web")));
        assert_eq!(
            parse("postcard-f/web"),
            address(Some("postcard-f"), None, Some("web"))
        );
        assert_eq!(
            parse("nova-postcard-frontend@feature-x/worker"),
            address(
                Some("nova-postcard-frontend"),
                Some("feature-x"),
                Some("worker")
            )
        );
        assert_eq!(
            parse("nova-postcard-frontend@claude/fix-thing-123/web"),
            address(
                Some("nova-postcard-frontend"),
                Some("claude/fix-thing-123"),
                Some("web")
            )
        );
    }

    #[test]
    fn rejects_malformed_addresses() {
        for input in ["@", "app@", "@x/", "/", "app/", "a@b@c", "app/web/extra"] {
            assert!(
                matches!(input.parse::<Address>(), Err(AddressError::Invalid { .. })),
                "{input}"
            );
        }
    }

    #[test]
    fn displays_as_typed() {
        for input in ["postcard-f@feature-x/worker", "@feature-x", "/web", "api"] {
            assert_eq!(parse(input).to_string(), input);
        }
    }

    #[test]
    fn matches_apps_exactly_or_by_unambiguous_substring() {
        assert_eq!(
            resolve_str("nova-postcard-frontend", None),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(Stack::Main),
                None
            ))
        );
        assert_eq!(
            resolve_str("postcard-f@feature-x", None),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(branch("feature-x")),
                None
            ))
        );
        assert_eq!(
            resolve_str("bill/web", None),
            Ok(target(
                "nova",
                Some("nova-billing"),
                Some(Stack::Main),
                Some("web")
            ))
        );
    }

    #[test]
    fn exact_matches_win_over_substrings() {
        // "api" is a substring of nova-postcard-api but exactly acme-api's short name.
        assert_eq!(
            resolve_str("api", None),
            Ok(target("acme", Some("acme-api"), Some(Stack::Main), None))
        );
        // "acme" is a substring of acme-api but exactly the collapsed app's name.
        assert_eq!(
            resolve_str("acme", None),
            Ok(target("acme", Some("acme"), Some(Stack::Main), None))
        );
    }

    #[test]
    fn ambiguous_apps_list_candidates() {
        let err = resolve_str("postcard", None).unwrap_err();
        assert_eq!(
            err,
            AddressError::Ambiguous {
                kind: "app",
                input: "postcard".into(),
                candidates: vec!["nova-postcard-frontend".into(), "nova-postcard-api".into()],
            }
        );
        assert_eq!(
            err.to_string(),
            "\"postcard\" matches more than one app: nova-postcard-frontend, nova-postcard-api"
        );
        let AddressError::Ambiguous { candidates, .. } = resolve_str("a", None).unwrap_err() else {
            panic!("expected ambiguity");
        };
        assert!(
            candidates.contains(&"acme-api (acme)".to_string()),
            "{candidates:?}"
        );
    }

    #[test]
    fn unknown_apps_list_candidates() {
        let err = resolve_str("payments", Some(FRONTEND_MAIN)).unwrap_err();
        assert_eq!(
            err,
            AddressError::NoMatch {
                kind: "app",
                input: "payments".into(),
                candidates: vec![
                    "nova-postcard-frontend".into(),
                    "nova-postcard-api".into(),
                    "nova-billing".into()
                ],
            }
        );
        assert!(
            err.to_string()
                .starts_with("no app matches \"payments\"; choose from")
        );
    }

    #[test]
    fn prefers_apps_in_the_current_project() {
        // Both nova-postcard-api and acme-api contain "api"; inside nova only nova's counts.
        assert_eq!(
            resolve_str("-api", Some(FRONTEND_MAIN)),
            Ok(target(
                "nova",
                Some("nova-postcard-api"),
                Some(Stack::Main),
                None
            ))
        );
        assert!(matches!(
            resolve_str("-api", None),
            Err(AddressError::Ambiguous { .. })
        ));
    }

    #[test]
    fn a_project_name_addresses_the_project() {
        assert_eq!(
            resolve_str("nova", None),
            Ok(target("nova", None, None, None))
        );
        assert_eq!(
            resolve_str("nova", Some(FRONTEND_FEATURE_X)),
            Ok(target("nova", None, None, None))
        );
        assert_eq!(
            resolve_str("nova@feature-x", None),
            Ok(target("nova", None, Some(branch("feature-x")), None))
        );
    }

    #[test]
    fn infers_everything_from_the_current_directory() {
        assert_eq!(
            resolve_str("", Some(FRONTEND_MAIN)),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(Stack::Main),
                None
            ))
        );
        assert_eq!(
            resolve_str("", Some(FRONTEND_FEATURE_X)),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(branch("feature-x")),
                None
            ))
        );
        assert_eq!(
            resolve_str("/web", Some(FRONTEND_FEATURE_X)),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(branch("feature-x")),
                Some("web")
            ))
        );
    }

    #[test]
    fn worktrees_inside_the_main_checkout_are_located() {
        let cwd = "/Users/dev/work/nova-postcard-frontend/.claude/worktrees/fix-thing-123/app";
        assert_eq!(
            resolve_str("", Some(cwd)),
            Ok(target(
                "nova",
                Some("nova-postcard-frontend"),
                Some(branch("claude-fix-thing-123")),
                None
            ))
        );
    }

    #[test]
    fn a_branch_without_an_app_addresses_the_stack() {
        assert_eq!(
            resolve_str("@feature-x/worker", Some(FRONTEND_MAIN)),
            Ok(target(
                "nova",
                None,
                Some(branch("feature-x")),
                Some("worker")
            ))
        );
        assert_eq!(
            resolve_str("@main", Some(FRONTEND_FEATURE_X)),
            Ok(target("nova", None, Some(Stack::Main), None))
        );
    }

    #[test]
    fn an_app_takes_the_current_stack() {
        assert_eq!(
            resolve_str("postcard-api", Some(FRONTEND_FEATURE_X)),
            Ok(target(
                "nova",
                Some("nova-postcard-api"),
                Some(branch("feature-x")),
                None
            ))
        );
        assert_eq!(
            resolve_str("postcard-api@main", Some(FRONTEND_FEATURE_X)),
            Ok(target(
                "nova",
                Some("nova-postcard-api"),
                Some(Stack::Main),
                None
            ))
        );
        // Another project's app runs on main: the cwd's stack belongs to nova.
        assert_eq!(
            resolve_str("acme-api", Some(FRONTEND_FEATURE_X)),
            Ok(target("acme", Some("acme-api"), Some(Stack::Main), None))
        );
    }

    #[test]
    fn branches_match_by_name_slug_or_substring() {
        let expected = target(
            "nova",
            Some("nova-postcard-frontend"),
            Some(branch("claude-fix-thing-123")),
            None,
        );
        for input in [
            "postcard-f@claude-fix-thing-123",
            "postcard-f@claude/fix-thing-123",
            "postcard-f@fix-thing",
        ] {
            assert_eq!(resolve_str(input, None), Ok(expected.clone()), "{input}");
        }
        assert_eq!(
            resolve_str("postcard-f@claude/fix-thing-123/web", None),
            Ok(Target {
                process: Some("web".into()),
                ..expected
            })
        );
    }

    #[test]
    fn branch_substrings_can_be_ambiguous() {
        let mut world = world();
        world.projects[0].worktrees.insert(
            "nova-billing".into(),
            vec![worktree(
                "feature-xyz",
                "/Users/dev/work/nova-billing-feature-xyz",
            )],
        );
        assert_eq!(
            resolve(&world, &parse("billing@feature"), None),
            Err(AddressError::Ambiguous {
                kind: "stack",
                input: "feature".into(),
                candidates: vec!["feature-x".into(), "feature-xyz".into()],
            })
        );
        assert_eq!(
            resolve(&world, &parse("billing@feature-x"), None),
            Ok(target(
                "nova",
                Some("nova-billing"),
                Some(branch("feature-x")),
                None
            ))
        );
    }

    #[test]
    fn unknown_branches_list_stacks() {
        assert_eq!(
            resolve_str("billing@feature-y", None),
            Err(AddressError::NoMatch {
                kind: "stack",
                input: "feature-y".into(),
                candidates: vec!["claude-fix-thing-123".into(), "feature-x".into()],
            })
        );
    }

    #[test]
    fn a_branch_alone_searches_every_project() {
        assert_eq!(
            resolve_str("@fix-thing", None),
            Ok(target(
                "nova",
                None,
                Some(branch("claude-fix-thing-123")),
                None
            ))
        );
        assert_eq!(
            resolve_str("@feature-x", None),
            Err(AddressError::Ambiguous {
                kind: "stack",
                input: "feature-x".into(),
                candidates: vec![
                    "feature-x (nova)".into(),
                    "feature-x (eventportal)".into(),
                    "feature-x (acme)".into()
                ],
            })
        );
        assert!(matches!(
            resolve_str("@nope", None),
            Err(AddressError::NoMatch { kind: "stack", .. })
        ));
    }

    #[test]
    fn needs_some_context() {
        assert_eq!(resolve_str("", None), Err(AddressError::NoContext));
        assert_eq!(
            resolve_str("/web", Some("/tmp")),
            Err(AddressError::NoContext)
        );
    }

    #[test]
    fn targets_expose_instance_keys() {
        let t = resolve_str("acme@main", None).unwrap();
        assert_eq!(
            t.instance(),
            Some(InstanceKey {
                project: "acme".into(),
                app: "acme".into(),
                stack: Stack::Main
            })
        );
        assert_eq!(resolve_str("nova", None).unwrap().instance(), None);
    }

    #[test]
    fn locates_directories() {
        let world = world();
        assert_eq!(
            locate(
                &world,
                Path::new("/Users/dev/clients/acme-api-feature-x/lib")
            ),
            Some(Location {
                project: "acme".into(),
                app: "acme-api".into(),
                stack: branch("feature-x"),
            })
        );
        // A sibling directory sharing a name prefix is not inside the checkout.
        assert_eq!(
            locate(&world, Path::new("/Users/dev/clients/acme-apix")),
            None
        );
    }
}
