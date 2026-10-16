use std::process::ExitCode;

use super::ProjectCommand;
use crate::paths::Paths;

pub fn run(_paths: &Paths, _command: ProjectCommand, _json: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte project is not implemented yet")
}
