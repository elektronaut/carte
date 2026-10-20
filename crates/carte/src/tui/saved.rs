//! What the TUI remembers between sessions: folds and which instances it shows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tree::{Fold, Node, NodeId, Show, node, path_to};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub show: Show,
    /// Explicit fold choices.
    #[serde(default)]
    pub expanded: Vec<(NodeId, bool)>,
}

impl Saved {
    /// The fold choices of nodes that still exist.
    pub fn from_fold(fold: &Fold, roots: &[Node]) -> Self {
        let mut expanded: Vec<(NodeId, bool)> = fold
            .expanded
            .iter()
            .filter(|(id, _)| roots.is_empty() || path_to(roots, id).is_some())
            .filter(|(id, _)| {
                let kube_project = matches!(id, NodeId::Project { .. })
                    && path_to(roots, id).is_some_and(|path| node(roots, &path).status.kube);
                !kube_project
            })
            .map(|(id, expanded)| (id.clone(), *expanded))
            .collect();
        expanded.sort();
        Saved {
            show: fold.show,
            expanded,
        }
    }

    pub fn apply(self, fold: &mut Fold) {
        fold.show = self.show;
        fold.expanded = self.expanded.into_iter().collect();
    }
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("tui.json")
}

/// The saved state, or the defaults when there is none or it can't be read.
pub fn load(path: &Path) -> Saved {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes the state through a temporary file.
pub fn store(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures::sample;
    use crate::tui::tree;

    #[test]
    fn kube_projects_open_afresh_each_session() {
        let roots = tree::build(&crate::tui::fixtures::kube_sample());
        let mut fold = Fold::default();
        let project = NodeId::Project {
            project: "prod".into(),
        };
        let namespace = NodeId::Stack {
            project: "prod".into(),
            stack: "logging".into(),
        };
        fold.expanded.insert(project, true);
        fold.expanded.insert(namespace.clone(), false);
        let saved = Saved::from_fold(&fold, &roots);
        assert_eq!(saved.expanded, vec![(namespace, false)]);
    }

    #[test]
    fn round_trips_and_forgets_nodes_that_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        assert_eq!(load(&file), Saved::default(), "nothing saved yet");

        let roots = tree::build(&sample());
        let mut fold = Fold {
            show: Show::Running,
            ..Fold::default()
        };
        let stack = NodeId::Stack {
            project: "nova".into(),
            stack: "main".into(),
        };
        let gone = NodeId::Project {
            project: "gone".into(),
        };
        fold.expanded.insert(stack.clone(), false);
        fold.expanded.insert(gone, true);
        store(&file, &Saved::from_fold(&fold, &roots)).unwrap();

        let mut restored = Fold::default();
        load(&file).apply(&mut restored);
        assert_eq!(restored.show, Show::Running);
        assert_eq!(
            restored.expanded.into_iter().collect::<Vec<_>>(),
            [(stack, false)]
        );
    }

    #[test]
    fn a_broken_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load(&file), Saved::default());
    }
}
