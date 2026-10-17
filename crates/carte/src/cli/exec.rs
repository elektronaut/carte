//! `carte exec`: a command in the foreground with an instance's env.

use std::io::ErrorKind;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use super::ExecArgs;
use crate::exit::{Exit, Failure};
use crate::paths::Paths;
use crate::process_env::Rendered;

pub fn run(paths: &Paths, args: ExecArgs) -> anyhow::Result<ExitCode> {
    let (rendered, daemon) = super::env::render_for(paths, &args.target, args.process, true)?;
    if !args.no_check && !rendered.stopped_providers.is_empty() {
        return Err(refusal(&rendered, daemon, args.target.project.as_deref()).into());
    }

    let checkout = rendered
        .checkout
        .canonicalize()
        .unwrap_or_else(|_| rendered.checkout.clone());
    let cwd = std::env::current_dir()?.canonicalize()?;
    let dir = if cwd.starts_with(&checkout) {
        cwd
    } else {
        checkout
    };
    let (program, rest) = args.command.split_first().expect("clap requires a command");
    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(dir)
        .envs(rendered.env.to_vars());
    if rendered.env.get("PORT").is_none() {
        command.env_remove("PORT");
    }
    let err = command.exec();
    eprintln!("carte: {program}: {err}");
    Ok(ExitCode::from(if err.kind() == ErrorKind::NotFound {
        127
    } else {
        126
    }))
}

/// Why a command won't run while processes that provide its env are down.
fn refusal(rendered: &Rendered, daemon: bool, project: Option<&str>) -> Failure {
    let processes = rendered
        .stopped_providers
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let vars = rendered
        .stopped_providers
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let project = project
        .map(|p| format!(" --project {p}"))
        .unwrap_or_default();
    let address = &rendered.address;
    let run_anyway = "or pass --no-check to run the command anyway";
    if !daemon {
        return Failure::new(
            Exit::DaemonDown,
            format!(
                "the daemon isn't running, so nothing serves {vars} (provided by {processes} of {address}); \
                 start it and run `carte up {address}{project}`, {run_anyway}"
            ),
        );
    }
    if !rendered.active {
        return Failure::new(
            Exit::NotReady,
            format!(
                "{address} isn't running, so nothing serves {vars} (provided by {processes}); \
                 start it with `carte up {address}{project}`, {run_anyway}"
            ),
        );
    }
    let up = rendered
        .stopped_providers
        .keys()
        .map(|process| format!("`carte up {address}/{process}{project}`"))
        .collect::<Vec<_>>()
        .join(" and ");
    Failure::new(
        Exit::NotReady,
        format!(
            "{processes} of {address} isn't running, so nothing serves {vars}; \
             start it with {up}, {run_anyway}"
        ),
    )
}
