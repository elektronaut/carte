use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    carte::cli::run(carte::cli::Cli::parse())
}
