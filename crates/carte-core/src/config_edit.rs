//! Edits to project files that keep their comments and formatting.

use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::model::Policy;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EditError {
    #[error("can't edit the project file: {0}")]
    Parse(String),
    #[error("[{0}] in the project file isn't a table")]
    NotATable(String),
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
    let mut table = doc.as_table_mut();
    for (depth, key) in path.iter().enumerate() {
        if !table.contains_key(key) {
            if policy.is_none() {
                return Ok(doc.to_string());
            }
            let mut new = Table::new();
            new.set_implicit(depth + 1 < path.len());
            table.insert(key, Item::Table(new));
        }
        table = table[key.as_str()]
            .as_table_mut()
            .ok_or_else(|| EditError::NotATable(path[..=depth].join(".")))?;
    }
    match policy {
        Some(policy) => {
            let word = match policy {
                Policy::Pinned => "pinned",
                Policy::Auto => "auto",
                Policy::Disabled => "disabled",
            };
            table["policy"] = value(word);
        }
        None => {
            table.remove("policy");
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# my project\nrepos = \"~/work/make-*\"   # all of them\nstrip_prefix = \"make-\"\n\n[apps.make-clock]\nname = \"clock\" # short\n";

    #[test]
    fn sets_a_process_policy_and_keeps_comments() {
        let out = set_policy(FILE, "make-clock", Some("clock"), Some(Policy::Disabled)).unwrap();
        assert!(
            out.starts_with("# my project\nrepos = \"~/work/make-*\"   # all of them\n"),
            "{out}"
        );
        assert!(out.contains("name = \"clock\" # short"), "{out}");
        assert!(
            out.contains("[apps.make-clock.processes.clock]\npolicy = \"disabled\""),
            "{out}"
        );
    }

    #[test]
    fn sets_an_app_policy_on_a_new_table() {
        let out = set_policy(FILE, "make-billing", None, Some(Policy::Pinned)).unwrap();
        assert!(
            out.contains("[apps.make-billing]\npolicy = \"pinned\""),
            "{out}"
        );
        assert!(!out.contains("[apps]\n"), "{out}");
    }

    #[test]
    fn removing_a_policy_leaves_the_rest() {
        let pinned = set_policy(FILE, "make-clock", None, Some(Policy::Pinned)).unwrap();
        let out = set_policy(&pinned, "make-clock", None, None).unwrap();
        assert!(!out.contains("policy"), "{out}");
        assert!(out.contains("name = \"clock\" # short"), "{out}");
        assert_eq!(
            set_policy(FILE, "make-other", Some("web"), None).unwrap(),
            FILE
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
}
