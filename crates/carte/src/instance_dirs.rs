//! Creates an instance's state and run directories, and keeps its stack's record.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use carte_core::dirs::{DirRoots, InstanceDirs, STACK_RECORD, StackRecord};
use carte_core::model::{InstanceKey, ProjectState};

/// Creates `key`'s directories and records the stack's current worktrees in
/// its folder. Creates nothing inside the instance's directories.
pub fn prepare(
    roots: &DirRoots,
    key: &InstanceKey,
    state: &ProjectState,
) -> io::Result<InstanceDirs> {
    let dirs = roots.instance(key);
    create_run_root(&roots.run)?;
    fs::create_dir_all(&dirs.run)?;
    fs::create_dir_all(&dirs.state)?;

    let stack = key.stack.slug();
    let stack_dir = roots.stack_dir(&key.project, stack);
    let existing = read_stack_record(&stack_dir).ok().flatten();
    let mut record = existing.clone().unwrap_or_default();
    record.update(state.worktrees.iter().flat_map(|(app, worktrees)| {
        worktrees
            .iter()
            .filter(|wt| wt.slug == stack)
            .map(move |wt| (app.as_str(), wt))
    }));
    if existing.as_ref() != Some(&record) {
        write_stack_record(&stack_dir, &record)?;
    }
    Ok(dirs)
}

/// Creates the run root, owned by this user and closed to everyone else.
fn create_run_root(root: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    let meta = fs::metadata(root)?;
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", root.display()),
        ));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The record in a stack's folder; `None` when there is none.
pub fn read_stack_record(stack_dir: &Path) -> io::Result<Option<StackRecord>> {
    match fs::read_to_string(stack_dir.join(STACK_RECORD)) {
        Ok(text) => StackRecord::parse(&text)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replaces the record in a stack's folder, creating the folder.
pub fn write_stack_record(stack_dir: &Path, record: &StackRecord) -> io::Result<()> {
    fs::create_dir_all(stack_dir)?;
    let mut file = tempfile::NamedTempFile::new_in(stack_dir)?;
    io::Write::write_all(&mut file, record.to_toml().as_bytes())?;
    file.persist(stack_dir.join(STACK_RECORD))
        .map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use carte_core::model::{Project, Scheme, Stack, Worktree};

    use super::*;

    fn worktree(app: &str, slug: &str) -> Worktree {
        Worktree {
            branch: format!("feature/{slug}"),
            slug: slug.into(),
            path: PathBuf::from("/w").join(format!("{app}-{slug}")),
        }
    }

    fn project_state(worktrees: &[(&str, &str)]) -> ProjectState {
        let mut map: BTreeMap<String, Vec<Worktree>> = BTreeMap::new();
        for (app, slug) in worktrees {
            map.entry(app.to_string())
                .or_default()
                .push(worktree(app, slug));
        }
        ProjectState {
            project: Project {
                folder: None,
                name: "nova".into(),
                config_path: "/c/nova.toml".into(),
                strip_prefix: None,
                env_provider: None,
                env: Vec::new(),
                requires: Vec::new(),
                scheme: Scheme::Https,
                idle_timeout: None,
                teardown_delay: None,
                apps: Vec::new(),
            },
            worktrees: map,
            processes: BTreeMap::new(),
        }
    }

    fn key(app: &str, stack: Stack) -> InstanceKey {
        InstanceKey {
            project: "nova".into(),
            app: app.into(),
            stack,
        }
    }

    #[test]
    fn creates_the_directories_and_nothing_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("state/data"),
            run: tmp.path().join("tmp"),
        };
        let key = key("web", Stack::Branch("x".into()));
        let dirs = prepare(&roots, &key, &project_state(&[("web", "x")])).unwrap();
        assert_eq!(dirs, roots.instance(&key));
        assert!(dirs.state.is_dir());
        assert!(dirs.run.is_dir());
        assert_eq!(fs::read_dir(&dirs.state).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&dirs.run).unwrap().count(), 0);
        let mode = fs::metadata(&roots.run).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        fs::set_permissions(&roots.run, fs::Permissions::from_mode(0o755)).unwrap();
        prepare(&roots, &key, &project_state(&[("web", "x")])).unwrap();
        let mode = fs::metadata(&roots.run).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn records_the_stacks_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("state/data"),
            run: tmp.path().join("tmp"),
        };
        let stack_dir = roots.stack_dir("nova", "x");
        assert_eq!(read_stack_record(&stack_dir).unwrap(), None);

        let state = project_state(&[("web", "x"), ("api", "x"), ("api", "y")]);
        prepare(&roots, &key("billing", Stack::Branch("x".into())), &state).unwrap();
        let record = read_stack_record(&stack_dir).unwrap().unwrap();
        assert_eq!(record.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            record.paths().collect::<Vec<_>>(),
            [Path::new("/w/api-x"), Path::new("/w/web-x")]
        );

        let state = project_state(&[("web", "x")]);
        prepare(&roots, &key("web", Stack::Branch("x".into())), &state).unwrap();
        let kept = read_stack_record(&stack_dir).unwrap().unwrap();
        assert_eq!(kept, record);

        prepare(&roots, &key("web", Stack::Main), &state).unwrap();
        let main = read_stack_record(&roots.stack_dir("nova", "main")).unwrap();
        assert_eq!(main, Some(StackRecord::default()));
    }

    #[test]
    fn an_unreadable_record_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("state/data"),
            run: tmp.path().join("tmp"),
        };
        let stack_dir = roots.stack_dir("nova", "x");
        fs::create_dir_all(&stack_dir).unwrap();
        fs::write(stack_dir.join(STACK_RECORD), "worktrees = 1").unwrap();
        assert!(read_stack_record(&stack_dir).is_err());
        let state = project_state(&[("web", "x")]);
        prepare(&roots, &key("web", Stack::Branch("x".into())), &state).unwrap();
        let record = read_stack_record(&stack_dir).unwrap().unwrap();
        assert_eq!(record.paths().collect::<Vec<_>>(), [Path::new("/w/web-x")]);
    }
}
