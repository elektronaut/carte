//! When a stack counts as removed, so that its teardown can run.

use std::collections::BTreeSet;
use std::path::Path;

use crate::dirs::StackRecord;

/// Whether a stack is gone: no worktree is named for it, and every worktree directory
/// its record names is gone. A rebase or a `git switch` inside a worktree keeps the
/// directory, so it never counts, however long it takes.
///
/// `stacks` are the stacks with a worktree as discovery found them; `None` when
/// discovery of the project failed, which never counts. `main` never counts, nor a
/// stack whose record names no worktree.
pub fn is_removed(
    stack: &str,
    stacks: Option<&BTreeSet<String>>,
    record: &StackRecord,
    dir_exists: impl Fn(&Path) -> bool,
) -> bool {
    let Some(stacks) = stacks else {
        return false;
    };
    if stack == "main" || stacks.contains(stack) {
        return false;
    }
    !record.worktrees.is_empty() && record.paths().all(|path| !dir_exists(path))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn record(worktrees: &[(&str, &str)]) -> StackRecord {
        StackRecord {
            branch: Some("feature/x".into()),
            worktrees: worktrees
                .iter()
                .map(|(app, path)| (app.to_string(), PathBuf::from(path)))
                .collect(),
        }
    }

    fn stacks(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn removed(stacks: Option<&BTreeSet<String>>, record: &StackRecord, dirs: &[&str]) -> bool {
        is_removed("feature-x", stacks, record, |path| {
            dirs.iter().any(|d| Path::new(d) == path)
        })
    }

    #[test]
    fn a_worktree_named_for_the_stack_keeps_it() {
        let record = record(&[("web", "/w/web-x")]);
        let found = stacks(&["feature-x"]);
        assert!(!removed(Some(&found), &record, &[]));
    }

    #[test]
    fn an_existing_directory_keeps_the_stack() {
        let record = record(&[("web", "/w/web-x")]);
        let none = stacks(&[]);
        assert!(!removed(Some(&none), &record, &["/w/web-x"]));
    }

    #[test]
    fn a_removed_worktree_removes_the_stack() {
        let record = record(&[("web", "/w/web-x")]);
        let none = stacks(&[]);
        assert!(removed(Some(&none), &record, &[]));
    }

    #[test]
    fn every_recorded_worktree_must_be_gone() {
        let record = record(&[("api", "/w/api-x"), ("web", "/w/web-x")]);
        let none = stacks(&[]);
        assert!(!removed(Some(&none), &record, &["/w/api-x"]));
        assert!(removed(Some(&none), &record, &[]));
    }

    #[test]
    fn a_failed_discovery_never_removes() {
        let record = record(&[("web", "/w/web-x")]);
        assert!(!removed(None, &record, &[]));
    }

    #[test]
    fn main_and_unrecorded_stacks_are_never_removed() {
        let none = stacks(&[]);
        let record = record(&[("web", "/w/web-x")]);
        assert!(!is_removed("main", Some(&none), &record, |_| false));
        assert!(!removed(Some(&none), &StackRecord::default(), &[]));
        let no_branch = StackRecord {
            branch: None,
            ..record
        };
        assert!(removed(Some(&none), &no_branch, &[]));
    }
}
