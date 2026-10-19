//! `carte exec`: a command in the foreground with an instance's env.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

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
    let _hold = (daemon && rendered.active)
        .then(|| {
            let target = carte_api::Address {
                address: rendered.address.clone(),
                cwd: Some(cwd.clone()),
                project: args.target.project.clone(),
            };
            hold(paths.socket(), &target)
        })
        .flatten();
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

/// A connection to the daemon that keeps the instance from idling out while it is
/// open. It survives the exec, so the hold lasts until the command and anything that
/// inherited it exit. `None` when the daemon doesn't take the hold.
fn hold(socket: PathBuf, target: &carte_api::Address) -> Option<UnixStream> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let body = serde_json::to_vec(target).ok()?;
    let head = format!(
        "POST {} HTTP/1.1\r\nhost: carte\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        carte_api::paths::HOLD,
        body.len()
    );
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    let mut status = [0; 12];
    stream.read_exact(&mut status).ok()?;
    if &status[9..] != b"200" {
        return None;
    }
    stream.set_read_timeout(None).ok()?;
    stream.set_write_timeout(None).ok()?;
    crate::process::set_cloexec(stream.as_raw_fd(), false).ok()?;
    Some(stream)
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
