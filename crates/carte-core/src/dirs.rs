//! Per-instance directories, the stack id, and the record kept in each stack's
//! state folder.
//!
//! An instance's state directory is `<data>/<project>/<stack>/<app>`, with
//! readable names. Its run directory is `<run>/<hash>`, short enough for Unix
//! socket paths. The stack's folder, `<data>/<project>/<stack>`, holds
//! `stack.toml`: the stack's branch and the worktree each app last had.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{InstanceKey, Worktree};

/// The file in a stack's folder that records its branch and worktrees.
pub const STACK_RECORD: &str = "stack.toml";

/// The first 5 bytes of the SHA-256 of `parts`, each followed by a NUL byte,
/// as 8 characters of lowercase RFC 4648 base32.
fn short_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let bits = digest[..5]
        .iter()
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    (0..8)
        .rev()
        .map(|i| alphabet[((bits >> (i * 5)) & 31) as usize] as char)
        .collect()
}

/// `{{stack.id}}`: the same for a project and stack name on every machine and
/// every run.
pub fn stack_id(project: &str, stack: &str) -> String {
    short_hash(&[project, stack])
}

/// Where instance directories go: `data` for state, `run` for run directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRoots {
    pub data: PathBuf,
    pub run: PathBuf,
}

impl DirRoots {
    /// The stack's state folder, which holds its record and the apps' state directories.
    pub fn stack_dir(&self, project: &str, stack: &str) -> PathBuf {
        self.data.join(project).join(stack)
    }

    pub fn instance(&self, key: &InstanceKey) -> InstanceDirs {
        let stack = key.stack.slug();
        InstanceDirs {
            state: self.stack_dir(&key.project, stack).join(&key.app),
            run: self.run.join(short_hash(&[
                &self.data.to_string_lossy(),
                &key.project,
                stack,
                &key.app,
            ])),
        }
    }
}

/// The file beside a run directory that names the data root of the carte that made
/// it, since several cartes can share a run root.
pub fn owner_marker(run_dir: &Path) -> PathBuf {
    run_dir.with_extension("owner")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDirs {
    /// `{{state_dir}}`, `CARTE_STATE_DIR`.
    pub state: PathBuf,
    /// `{{run_dir}}`, `CARTE_RUN_DIR`.
    pub run: PathBuf,
}

/// What a stack's `stack.toml` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRecord {
    /// The last branch seen checked out in the stack's worktrees, for `CARTE_BRANCH`
    /// in teardowns. Informational only: stacks are named after worktree folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The worktree each app last had for the stack, by app name.
    #[serde(default)]
    pub worktrees: BTreeMap<String, PathBuf>,
}

impl StackRecord {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a stack record serializes")
    }

    /// Records the stack's current worktrees, keeping apps that no longer have one.
    pub fn update<'a>(&mut self, worktrees: impl IntoIterator<Item = (&'a str, &'a Worktree)>) {
        for (app, wt) in worktrees {
            if let Some(branch) = &wt.branch {
                self.branch = Some(branch.clone());
            }
            self.worktrees.insert(app.to_string(), wt.path.clone());
        }
    }

    /// The recorded worktree directories.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.worktrees.values().map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stack;

    fn roots() -> DirRoots {
        DirRoots {
            data: "/state/data".into(),
            run: "/run/carte".into(),
        }
    }

    fn key(stack: Stack) -> InstanceKey {
        InstanceKey {
            project: "nova".into(),
            app: "nova-billing".into(),
            stack,
        }
    }

    #[test]
    fn stack_id_is_stable() {
        assert_eq!(stack_id("nova", "feature-x"), "yo5bblna");
        assert_eq!(stack_id("nova", "main"), "zdguiwfa");
        assert_ne!(stack_id("nov", "afeature-x"), stack_id("nova", "feature-x"));
    }

    #[test]
    fn short_hash_is_lowercase_base32() {
        for parts in [&["a"][..], &["nova", "main", "nova-billing"], &[""]] {
            let hash = short_hash(parts);
            assert_eq!(hash.len(), 8);
            assert!(
                hash.chars()
                    .all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)),
                "{hash}"
            );
        }
    }

    #[test]
    fn instance_dirs() {
        let dirs = roots().instance(&key(Stack::Branch("feature-x".into())));
        assert_eq!(
            dirs.state,
            PathBuf::from("/state/data/nova/feature-x/nova-billing")
        );
        assert_eq!(dirs.run, PathBuf::from("/run/carte/72bkht4w"));
        let other = DirRoots {
            data: "/other/data".into(),
            ..roots()
        };
        assert_ne!(
            other.instance(&key(Stack::Branch("feature-x".into()))).run,
            dirs.run,
            "cartes sharing a run root get their own run directories"
        );
        let main = roots().instance(&key(Stack::Main));
        assert_eq!(
            main.state,
            PathBuf::from("/state/data/nova/main/nova-billing")
        );
        assert_ne!(main.run, dirs.run);
        assert_eq!(
            roots().stack_dir("nova", "feature-x"),
            PathBuf::from("/state/data/nova/feature-x")
        );
    }

    #[test]
    fn stack_record_round_trips() {
        let mut record = StackRecord::default();
        assert_eq!(record.to_toml(), "[worktrees]\n");
        let wt = |path: &str| Worktree {
            branch: Some("feature/X".into()),
            slug: "feature-x".into(),
            path: path.into(),
        };
        let (web, api) = (wt("/w/web-x"), wt("/w/api-x"));
        record.update([("web", &web), ("api", &api)]);
        let text = record.to_toml();
        assert_eq!(
            text,
            "branch = \"feature/X\"\n\n[worktrees]\napi = \"/w/api-x\"\nweb = \"/w/web-x\"\n"
        );
        assert_eq!(StackRecord::parse(&text).unwrap(), record);

        let moved = wt("/w/web-moved");
        record.update([("web", &moved)]);
        assert_eq!(
            record.paths().collect::<Vec<_>>(),
            [Path::new("/w/api-x"), Path::new("/w/web-moved")]
        );
        assert_eq!(StackRecord::parse("").unwrap(), StackRecord::default());
        assert!(StackRecord::parse("worktrees = 1").is_err());
    }
}
