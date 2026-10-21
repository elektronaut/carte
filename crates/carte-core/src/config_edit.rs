//! Edits to project files that keep their comments and formatting.

use std::path::{Path, PathBuf};

use thiserror::Error;
use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

use crate::model::{Policy, Project};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EditError {
    #[error("can't edit the project file: {0}")]
    Parse(String),
    #[error("[{0}] in the project file isn't a table")]
    NotATable(String),
    #[error(
        "{project} is made by the template {}, and saving there would change every project it makes. Give {project} a project file of its own to save its policies, or run it without --save",
        template.display()
    )]
    FromTemplate { project: String, template: PathBuf },
}

/// The file `--save` writes a project's policies to: its project file, but never
/// the template that made it.
pub fn policy_file(project: &Project) -> Result<&Path, EditError> {
    if project.from_template {
        return Err(EditError::FromTemplate {
            project: project.name.clone(),
            template: project.config_path.clone(),
        });
    }
    Ok(&project.config_path)
}

/// Sets or removes the `policy` of an app (`[apps.<app>]`) or one of its processes
/// (`[apps.<app>.processes.<process>]`). `None` removes the key, so the policy falls
/// back to the app's or the default.
pub fn set_policy(
    text: &str,
    app: &str,
    process: Option<&str>,
    policy: Option<Policy>,
) -> Result<String, EditError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| EditError::Parse(e.to_string()))?;
    let mut path = vec!["apps".to_string(), app.to_string()];
    if let Some(process) = process {
        path.extend(["processes".to_string(), process.to_string()]);
    }
    let mut item = doc.as_item_mut();
    for (depth, key) in path.iter().enumerate() {
        let inline = item.is_inline_table();
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| EditError::NotATable(path[..depth].join(".")))?;
        if !table.contains_key(key) {
            if policy.is_none() {
                return Ok(doc.to_string());
            }
            let new = if inline {
                Item::Value(InlineTable::new().into())
            } else {
                let mut new = Table::new();
                new.set_implicit(depth + 1 < path.len());
                Item::Table(new)
            };
            table.insert(key, new);
        }
        item = table.get_mut(key).expect("just checked or inserted");
    }
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| EditError::NotATable(path.join(".")))?;
    match policy {
        Some(policy) => {
            let word = match policy {
                Policy::Pinned => "pinned",
                Policy::Auto => "auto",
                Policy::Disabled => "disabled",
            };
            match table.get_mut("policy") {
                Some(item) => {
                    let decor = item.as_value().map(|v| v.decor().clone());
                    *item = value(word);
                    if let (Some(decor), Some(new)) = (decor, item.as_value_mut()) {
                        *new.decor_mut() = decor;
                    }
                }
                None => {
                    table.insert("policy", value(word));
                }
            }
        }
        None => {
            table.remove("policy");
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const FILE: &str = "# my project\nrepos = \"~/work/nova-*\"   # all of them\nstrip_prefix = \"nova-\"\n\n[apps.nova-clock]\nname = \"clock\" # short\n";

    #[test]
    fn sets_a_process_policy_and_keeps_comments() {
        let out = set_policy(FILE, "nova-clock", Some("clock"), Some(Policy::Disabled)).unwrap();
        assert!(
            out.starts_with("# my project\nrepos = \"~/work/nova-*\"   # all of them\n"),
            "{out}"
        );
        assert!(out.contains("name = \"clock\" # short"), "{out}");
        assert!(
            out.contains("[apps.nova-clock.processes.clock]\npolicy = \"disabled\""),
            "{out}"
        );

        let file = "[apps.shop]\n# keep me\npolicy = \"auto\" # and me\n";
        let out = set_policy(file, "shop", None, Some(Policy::Pinned)).unwrap();
        assert_eq!(
            out,
            "[apps.shop]\n# keep me\npolicy = \"pinned\" # and me\n"
        );
    }

    #[test]
    fn sets_an_app_policy_on_a_new_table() {
        let out = set_policy(FILE, "nova-billing", None, Some(Policy::Pinned)).unwrap();
        assert!(
            out.contains("[apps.nova-billing]\npolicy = \"pinned\""),
            "{out}"
        );
        assert!(!out.contains("[apps]\n"), "{out}");
    }

    #[test]
    fn removing_a_policy_leaves_the_rest() {
        let pinned = set_policy(FILE, "nova-clock", None, Some(Policy::Pinned)).unwrap();
        let out = set_policy(&pinned, "nova-clock", None, None).unwrap();
        assert!(!out.contains("policy"), "{out}");
        assert!(out.contains("name = \"clock\" # short"), "{out}");
        assert_eq!(
            set_policy(FILE, "nova-other", Some("web"), None).unwrap(),
            FILE
        );
    }

    #[test]
    fn edits_inline_tables() {
        let file = "[apps]\nshop = { policy = \"auto\" } # inline\nwiki = { path = \"wiki\" }\n";
        let out = set_policy(file, "shop", None, Some(Policy::Pinned)).unwrap();
        assert!(
            out.contains("shop = { policy = \"pinned\" } # inline"),
            "{out}"
        );
        let out = set_policy(file, "wiki", Some("web"), Some(Policy::Disabled)).unwrap();
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed["apps"]["wiki"]["processes"]["web"]["policy"].as_str(),
            Some("disabled"),
            "{out}"
        );
        assert_eq!(parsed["apps"]["wiki"]["path"].as_str(), Some("wiki"));
        let out = set_policy(file, "shop", None, None).unwrap();
        assert!(!out.contains("policy"), "{out}");
    }

    #[test]
    fn policies_of_template_projects_are_not_saved() {
        let mut project = crate::test_support::project("acme", None, Vec::new());
        assert_eq!(policy_file(&project), Ok(project.config_path.as_path()));
        project.from_template = true;
        let err = policy_file(&project).unwrap_err();
        assert!(
            err.to_string()
                .contains("Give acme a project file of its own"),
            "{err}"
        );
    }

    #[test]
    fn rejects_broken_files() {
        assert!(matches!(
            set_policy("apps = 3", "x", None, Some(Policy::Pinned)),
            Err(EditError::NotATable(_))
        ));
        assert!(matches!(
            set_policy("[[", "x", None, None),
            Err(EditError::Parse(_))
        ));
    }

    fn policy() -> impl Strategy<Value = Option<Policy>> {
        proptest::option::of(proptest::sample::select(vec![
            Policy::Pinned,
            Policy::Auto,
            Policy::Disabled,
        ]))
    }

    fn policy_line(policy: Option<Policy>) -> String {
        policy.map_or(String::new(), |p| {
            format!("policy = {}\n", toml::Value::try_from(p).unwrap())
        })
    }

    /// A project file with `[apps.<app>]` tables, some with a policy and processes,
    /// some processes written inline.
    fn project_file() -> impl Strategy<Value = String> {
        let process = ("[a-c]", policy(), any::<bool>());
        let app = ("[a-c]", policy(), proptest::collection::vec(process, 0..3));
        proptest::collection::btree_map("[a-c]", app, 0..3).prop_map(|apps| {
            let mut text = String::from("# acme\nstrip_prefix = \"acme-\" # keep\n");
            for (app, (name, policy, processes)) in apps {
                text.push_str(&format!("\n[apps.{app}]\nname = \"{name}\"\n"));
                text.push_str(&policy_line(policy));
                let mut seen = Vec::new();
                for (process, policy, inline) in processes {
                    if seen.contains(&process) {
                        continue;
                    }
                    seen.push(process.clone());
                    match (inline, policy) {
                        (true, Some(p)) => text.push_str(&format!(
                            "processes.{process} = {{ policy = {} }}\n",
                            toml::Value::try_from(p).unwrap()
                        )),
                        _ => text.push_str(&format!(
                            "\n[apps.{app}.processes.{process}]\ncommand = \"run\"\n{}",
                            policy_line(policy)
                        )),
                    }
                }
            }
            text
        })
    }

    /// The table at `path`, if every step is one.
    fn table_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Table> {
        path.iter()
            .try_fold(table, |table, key| table.get(*key)?.as_table())
    }

    /// `table` without the `policy` at `path` and without empty tables.
    fn rest(mut table: toml::Table, path: &[&str]) -> toml::Table {
        fn prune(table: &mut toml::Table) {
            for (_, value) in table.iter_mut() {
                if let toml::Value::Table(inner) = value {
                    prune(inner);
                }
            }
            table.retain(|_, v| v.as_table().is_none_or(|t| !t.is_empty()));
        }
        let mut target = Some(&mut table);
        for key in path {
            target = target.and_then(|t| t.get_mut(*key)?.as_table_mut());
        }
        if let Some(target) = target {
            target.remove("policy");
        }
        prune(&mut table);
        table
    }

    proptest! {
        #[test]
        fn set_policy_changes_only_that_policy(
            text in project_file(),
            app in "[a-d]",
            process in proptest::option::of("[a-d]"),
            policy in policy(),
        ) {
            let before: toml::Table = text.parse().unwrap();
            let Ok(out) = set_policy(&text, &app, process.as_deref(), policy) else {
                return Ok(());
            };
            let after: toml::Table = out.parse().unwrap();
            let mut path = vec!["apps", app.as_str()];
            path.extend(process.as_deref().map(|p| ["processes", p]).into_iter().flatten());
            let read = table_at(&after, &path)
                .and_then(|t| t.get("policy"))
                .map(|v| v.clone().try_into::<Policy>().unwrap());
            prop_assert_eq!(read, policy, "{}", out);
            prop_assert_eq!(rest(after, &path), rest(before, &path), "{}", out);
        }
    }
}
