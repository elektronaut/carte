//! `carte import kube`: a kube project for each kubeconfig context you pick.

use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use carte_core::model::World;
use carte_core::{discovery, kube, slug};
use toml_edit::{DocumentMut, InlineTable, value};

use super::prompt::{self, tilde};
use crate::paths::Paths;

/// A project file to write for a context.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    context: String,
    name: String,
}

/// What to do with each context: a suggested name, or why it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Suggestion {
    Name(String),
    /// Another project or context has the name the context suggests.
    Clash(String),
    /// A kube project already uses the context.
    Imported(String),
}

/// Names a new project can't have: projects, files in `projects/` at any depth, and
/// app aliases.
fn taken_names(paths: &Paths, world: &World) -> BTreeSet<String> {
    world
        .projects
        .iter()
        .flat_map(|state| {
            std::iter::once(state.project.name.clone()).chain(
                state
                    .project
                    .apps
                    .iter()
                    .flat_map(|app| app.settings.aliases.clone()),
            )
        })
        .chain(world.kube_projects.iter().flat_map(|p| {
            std::iter::once(p.name.clone())
                .chain(p.services.values().flat_map(|s| s.aliases.clone()))
        }))
        .chain(
            discovery::project_files(&paths.projects_dir())
                .unwrap_or_default()
                .into_iter()
                .map(|f| f.name),
        )
        .collect()
}

/// The name each context suggests, given the names taken and the kubeconfig read.
fn suggest(
    world: &World,
    taken: &BTreeSet<String>,
    kubeconfig: Option<&Path>,
    contexts: &[String],
) -> Vec<(String, Suggestion)> {
    let labels: Vec<String> = contexts.iter().map(|c| kube::context_label(c)).collect();
    contexts
        .iter()
        .zip(&labels)
        .map(|(context, label)| {
            let suggestion = if let Some(project) = world
                .kube_projects
                .iter()
                .find(|p| p.context == *context && p.kubeconfig.as_deref() == kubeconfig)
            {
                Suggestion::Imported(project.name.clone())
            } else if taken.contains(label)
                || labels.iter().filter(|l| *l == label).count() > 1
                || slug::validate_project_name(label).is_err()
            {
                Suggestion::Clash(label.clone())
            } else {
                Suggestion::Name(label.clone())
            };
            (context.clone(), suggestion)
        })
        .collect()
}

/// A folder of `projects/` that discovery reads: relative, without `..`, and without
/// a part starting with a dot.
fn check_folder(folder: &str) -> anyhow::Result<()> {
    let path = Path::new(folder);
    let fine = !folder.is_empty()
        && path.components().all(|c| match c {
            std::path::Component::Normal(part) => !part.to_string_lossy().starts_with('.'),
            _ => false,
        });
    if !fine {
        anyhow::bail!(
            "--folder {folder:?} must be a folder inside projects/, like `clusters`, not starting with a dot"
        );
    }
    Ok(())
}

/// The one kubeconfig file the daemon should read: `--kubeconfig`, else a single
/// file in `$KUBECONFIG`, since the daemon doesn't see your shell's env.
fn kubeconfig_file(given: Option<PathBuf>) -> anyhow::Result<Option<PathBuf>> {
    let file = match given {
        Some(path) => Some(path),
        None => {
            let env = std::env::var_os("KUBECONFIG").unwrap_or_default();
            let files: Vec<PathBuf> = std::env::split_paths(&env)
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            match files.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                _ => anyhow::bail!(
                    "$KUBECONFIG names several files; pass the one to use with --kubeconfig"
                ),
            }
        }
    };
    file.map(|path| std::path::absolute(&path).context("resolving --kubeconfig"))
        .transpose()
}

fn project_text(context: &str, kubeconfig: Option<&Path>) -> String {
    let mut table = InlineTable::new();
    table.insert("context", context.into());
    if let Some(path) = kubeconfig {
        table.insert("kubeconfig", tilde(path).into());
    }
    let mut doc = DocumentMut::new();
    doc["kube"] = value(table);
    doc.to_string()
}

fn read_contexts(kubeconfig: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let config = match kubeconfig {
        Some(path) => ::kube::config::Kubeconfig::read_from(path),
        None => ::kube::config::Kubeconfig::read(),
    }
    .context("reading the kubeconfig")?;
    Ok(config.contexts.into_iter().map(|c| c.name).collect())
}

pub fn run(
    paths: &Paths,
    kubeconfig: Option<PathBuf>,
    folder: Option<String>,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<ExitCode> {
    if let Some(folder) = &folder {
        check_folder(folder)?;
    }
    let kubeconfig = kubeconfig_file(kubeconfig)?;
    let contexts = read_contexts(kubeconfig.as_deref())?;
    if contexts.is_empty() {
        println!("The kubeconfig has no contexts.");
        return Ok(ExitCode::SUCCESS);
    }
    let world = discovery::load_world(&paths.config_dir);
    let interactive = !dry_run && prompt::interactive(yes);
    let mut taken = taken_names(paths, &world);
    let mut drafts = Vec::new();
    for (context, suggestion) in suggest(&world, &taken, kubeconfig.as_deref(), &contexts) {
        let name = match suggestion {
            Suggestion::Imported(project) => {
                println!("{context}: already in project {project}");
                continue;
            }
            Suggestion::Name(name) if !taken.contains(&name) => {
                if interactive && !prompt::confirm(&format!("Add {context} as {name}?"), true)? {
                    continue;
                }
                name
            }
            Suggestion::Name(name) | Suggestion::Clash(name) => {
                if !interactive {
                    println!("{context}: skipped, {name:?} is taken or not a valid name");
                    continue;
                }
                match ask_name(&context, &name, &taken)? {
                    Some(name) => name,
                    None => continue,
                }
            }
        };
        taken.insert(name.clone());
        drafts.push(Draft { context, name });
    }
    if drafts.is_empty() {
        println!("Nothing to import.");
        return Ok(ExitCode::SUCCESS);
    }
    let dir = match &folder {
        Some(folder) => paths.projects_dir().join(folder),
        None => paths.projects_dir(),
    };
    for draft in &drafts {
        let file = dir.join(format!("{}.toml", draft.name));
        let text = project_text(&draft.context, kubeconfig.as_deref());
        if dry_run {
            println!("{}:\n{text}", tilde(&file));
            continue;
        }
        fs::create_dir_all(&dir)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(mut out) => {
                out.write_all(text.as_bytes())?;
                println!("Wrote {}", tilde(&file));
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                println!("Skipped {}: it exists", tilde(&file));
            }
            Err(err) => return Err(anyhow::Error::from(err).context(file.display().to_string())),
        }
    }
    if dry_run {
        println!("Dry run: nothing was written.");
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks for a name until it's valid and free; `None` when the answer is empty.
fn ask_name(
    context: &str,
    suggested: &str,
    taken: &BTreeSet<String>,
) -> anyhow::Result<Option<String>> {
    let mut question = format!(
        "{context} would be {suggested:?}, which is taken or invalid; name it (empty skips)"
    );
    loop {
        let answer = prompt::ask(&question, None)?;
        if answer.is_empty() {
            return Ok(None);
        }
        match slug::validate_project_name(&answer) {
            Err(err) => question = format!("{err}; name it (empty skips)"),
            Ok(()) if taken.contains(&answer) => {
                question = format!("{answer:?} is taken too; name it (empty skips)")
            }
            Ok(()) => return Ok(Some(answer)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: c
  cluster:
    server: https://127.0.0.1:6443
users:
- name: u
  user:
    token: secret
contexts:
- name: arn:aws:eks:eu-west-1:123456:cluster/production
  context: {cluster: c, user: u}
- name: gke_acme-infra_europe-north1_staging
  context: {cluster: c, user: u}
- name: shop
  context: {cluster: c, user: u}
- name: kind-nova
  context: {cluster: c, user: u}
- name: x
  context: {cluster: c, user: u}
"#;

    #[test]
    fn writes_a_project_for_each_context_with_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.projects_dir()).unwrap();
        fs::write(
            paths.projects_dir().join("shop.toml"),
            "root = \"/nowhere\"\n",
        )
        .unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        fs::write(&kubeconfig, KUBECONFIG).unwrap();
        fs::write(
            paths.projects_dir().join("nova.toml"),
            format!(
                "kube = {{ context = \"kind-nova\", kubeconfig = \"{}\" }}\n",
                kubeconfig.display()
            ),
        )
        .unwrap();

        run(
            &paths,
            Some(kubeconfig.clone()),
            Some("clusters".into()),
            true,
            true,
        )
        .unwrap();
        assert!(!paths.projects_dir().join("clusters").exists());

        run(
            &paths,
            Some(kubeconfig.clone()),
            Some("clusters".into()),
            false,
            true,
        )
        .unwrap();
        let world = discovery::load_world(&paths.config_dir);
        let mut imported: Vec<(String, String, Option<String>)> = world
            .kube_projects
            .iter()
            .map(|p| (p.name.clone(), p.context.clone(), p.folder.clone()))
            .collect();
        imported.sort();
        assert_eq!(
            imported,
            vec![
                ("nova".into(), "kind-nova".into(), None),
                (
                    "production".into(),
                    "arn:aws:eks:eu-west-1:123456:cluster/production".into(),
                    Some("clusters".into())
                ),
                (
                    "staging".into(),
                    "gke_acme-infra_europe-north1_staging".into(),
                    Some("clusters".into())
                ),
            ],
            "shop clashes with a project, x is too short, kind-nova is imported"
        );
        assert_eq!(
            world.kube_project("staging").unwrap().kubeconfig.as_deref(),
            Some(kubeconfig.as_path())
        );
    }

    #[test]
    fn checks_folders() {
        assert!(check_folder("clusters").is_ok());
        assert!(check_folder("work/clusters").is_ok());
        for bad in ["", "../x", "/abs", ".clusters", "a/.b"] {
            assert!(check_folder(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn suggests_names_and_spots_clashes() {
        let world = World::default();
        let contexts = vec![
            "arn:aws:eks:eu-west-1:1:cluster/prod".to_string(),
            "arn:aws:eks:us-east-1:2:cluster/prod".to_string(),
            "kind-nova".to_string(),
        ];
        let suggestions: Vec<Suggestion> = suggest(&world, &BTreeSet::new(), None, &contexts)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            suggestions,
            vec![
                Suggestion::Clash("prod".into()),
                Suggestion::Clash("prod".into()),
                Suggestion::Name("kind-nova".into()),
            ]
        );
    }
}
