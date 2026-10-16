use std::process::ExitCode;

use carte_api::Action;

use super::{LogsArgs, Target};
use crate::paths::Paths;

pub fn daemon(_paths: &Paths) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte daemon is not implemented yet")
}

pub fn ls(_paths: &Paths, _json: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte ls is not implemented yet")
}

pub fn control(
    _paths: &Paths,
    _target: Target,
    _action: Action,
    _json: bool,
) -> anyhow::Result<ExitCode> {
    anyhow::bail!("not implemented yet")
}

pub fn logs(_paths: &Paths, _args: LogsArgs, _json: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte logs is not implemented yet")
}

pub fn open(_paths: &Paths, _target: Target) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte open is not implemented yet")
}

pub fn stop_all(_paths: &Paths, _idle: bool, _json: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte stop-all is not implemented yet")
}
