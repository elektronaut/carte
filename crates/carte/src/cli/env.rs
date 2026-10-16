use std::process::ExitCode;

use super::Target;
use crate::paths::Paths;

pub fn run(_paths: &Paths, _target: Target, _json: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("carte env is not implemented yet")
}
