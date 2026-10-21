//! `carte prune`: removes state folders, run directories and pidfiles that nothing
//! that exists owns any more. The daemon decides and deletes.

use std::fmt::Write as _;
use std::process::ExitCode;

use carte_api::{
    PruneCandidate, PruneKind, PruneList, PruneOutput, PruneRequest, PruneResult, paths,
};

use super::PruneArgs;
use super::prompt::{self, tilde};
use crate::client::Client;
use crate::exit::{Exit, Failure};
use crate::paths::Paths;
use crate::tui::format_bytes;

pub fn run(paths: &Paths, args: PruneArgs, json: bool) -> anyhow::Result<ExitCode> {
    if json && !args.yes && !args.dry_run {
        return Err(Failure::new(
            Exit::Usage,
            "prune --json removes nothing without --yes; pass --dry-run to only list",
        )
        .into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let client = Client::new(&paths.socket());
    let list: PruneList = runtime.block_on(client.get(paths::PRUNE))?;
    if !json {
        print!("{}", render_list(&list));
    }
    if list.candidates.is_empty() || args.dry_run {
        if json {
            let output = PruneOutput { list, result: None };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !args.yes {
        if !prompt::interactive(false) {
            return Err(Failure::new(
                Exit::Usage,
                "not removing anything without a terminal to confirm on; pass --yes to remove, or --dry-run to only list",
            )
            .into());
        }
        if !prompt::confirm(&format!("Remove {}?", list.candidates.len()), false)? {
            return Ok(ExitCode::SUCCESS);
        }
    }
    let request = PruneRequest {
        paths: list.candidates.iter().map(|c| c.path.clone()).collect(),
    };
    let result: PruneResult = runtime.block_on(client.post(paths::PRUNE, &request))?;
    let failed = !result.errors.is_empty();
    if json {
        let output = PruneOutput {
            list,
            result: Some(result),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print!("{}", render_result(&result));
    }
    Ok(if failed {
        Exit::Error.into()
    } else {
        ExitCode::SUCCESS
    })
}

fn kind_label(kind: PruneKind) -> &'static str {
    match kind {
        PruneKind::Project => "project folder",
        PruneKind::Stack => "stack folder",
        PruneKind::App => "app folder",
        PruneKind::RunDir => "run dir",
        PruneKind::Pidfile => "pidfile",
        PruneKind::Trash => "unfinished prune",
    }
}

fn group_label(candidate: &PruneCandidate) -> String {
    match (&candidate.project, &candidate.stack) {
        _ if candidate.kind == PruneKind::Trash => "left by earlier prunes".into(),
        (None, _) => "run dirs of unknown instances".into(),
        (Some(project), None) => format!("{project} (project file gone)"),
        (Some(project), Some(stack)) => format!("{project} @{stack}"),
    }
}

fn render_list(list: &PruneList) -> String {
    let mut out = String::new();
    if list.candidates.is_empty() {
        out.push_str("Nothing to prune.\n");
    }
    let mut group = None;
    for candidate in &list.candidates {
        let label = group_label(candidate);
        if group.as_ref() != Some(&label) {
            let _ = writeln!(out, "{label}");
            group = Some(label);
        }
        let _ = writeln!(
            out,
            "  {:<14} {:>6}  {}",
            kind_label(candidate.kind),
            format_bytes(candidate.bytes),
            tilde(&candidate.path)
        );
    }
    if !list.candidates.is_empty() {
        let _ = writeln!(
            out,
            "Total: {} in {}",
            format_bytes(list.total_bytes),
            list.candidates.len()
        );
    }
    if !list.stacks.is_empty() {
        let stacks: Vec<String> = list
            .stacks
            .iter()
            .map(|s| format!("{}@{}", s.project, s.stack))
            .collect();
        let _ = writeln!(
            out,
            "Teardown never ran or failed for these stacks, so external resources (databases etc.) may remain: {}",
            stacks.join(", ")
        );
    }
    for skip in &list.skipped {
        let _ = match (&skip.project, &skip.stack) {
            (Some(project), Some(stack)) => {
                writeln!(out, "Skipped {project}@{stack}: {}", skip.reason)
            }
            (Some(project), None) => writeln!(out, "Skipped {project}: {}", skip.reason),
            (None, _) => writeln!(out, "Skipped: {}", skip.reason),
        };
    }
    out
}

fn render_result(result: &PruneResult) -> String {
    let mut out = format!(
        "Removed {} ({}).\n",
        result.removed.len(),
        format_bytes(result.removed_bytes)
    );
    for kept in &result.kept {
        let _ = writeln!(out, "Kept {}: {}", tilde(&kept.path), kept.reason);
    }
    for error in &result.errors {
        let _ = writeln!(
            out,
            "Couldn't remove {}: {}",
            tilde(&error.path),
            error.reason
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use carte_api::{PruneSkip, PruneStack};

    use super::*;

    fn candidate(
        kind: PruneKind,
        project: Option<&str>,
        stack: Option<&str>,
        path: &str,
        bytes: u64,
    ) -> PruneCandidate {
        PruneCandidate {
            kind,
            project: project.map(Into::into),
            stack: stack.map(Into::into),
            app: None,
            path: path.into(),
            bytes,
        }
    }

    #[test]
    fn lists_by_project_and_stack_with_sizes() {
        let list = PruneList {
            candidates: vec![
                candidate(PruneKind::Project, Some("gone"), None, "/s/data/gone", 2048),
                candidate(
                    PruneKind::Stack,
                    Some("nova"),
                    Some("x"),
                    "/s/data/nova/x",
                    10,
                ),
                candidate(
                    PruneKind::Pidfile,
                    Some("nova"),
                    Some("x"),
                    "/s/run/nova/web@x.pid",
                    5,
                ),
                candidate(PruneKind::RunDir, None, None, "/r/abcdefgh", 0),
            ],
            total_bytes: 2063,
            stacks: vec![PruneStack {
                project: "nova".into(),
                stack: "x".into(),
            }],
            skipped: vec![PruneSkip {
                project: Some("acme".into()),
                stack: None,
                reason: "git worktree list failed".into(),
            }],
        };
        assert_eq!(
            render_list(&list),
            "gone (project file gone)
  project folder     2K  /s/data/gone
nova @x
  stack folder      10B  /s/data/nova/x
  pidfile            5B  /s/run/nova/web@x.pid
run dirs of unknown instances
  run dir            0B  /r/abcdefgh
Total: 2K in 4
Teardown never ran or failed for these stacks, so external resources (databases etc.) may remain: nova@x
Skipped acme: git worktree list failed
"
        );
    }

    #[test]
    fn says_when_there_is_nothing() {
        let list = PruneList {
            candidates: vec![],
            total_bytes: 0,
            stacks: vec![],
            skipped: vec![],
        };
        assert_eq!(render_list(&list), "Nothing to prune.\n");
    }
}
