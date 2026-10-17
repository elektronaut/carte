//! The `carte` command line.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::paths::Paths;

mod control;
mod env;
mod import;
mod project;
pub(crate) mod prompt;
mod setup;

#[derive(Parser)]
#[command(name = "carte", version, about = "Runs your apps' processes on demand")]
pub struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List projects, stacks, apps and their state.
    Ls {
        /// Include stopped instances.
        #[arg(long, short)]
        all: bool,
    },
    /// Start an instance, a stack or a process (lazy start does this too).
    Up(Target),
    /// Stop an instance, a stack or a process.
    Down(Target),
    /// Restart an instance, a stack or a process.
    Restart(Target),
    /// Keep a process running through idle periods.
    Pin(PolicyArgs),
    /// Let a pinned process idle out again.
    Unpin(PolicyArgs),
    /// Never start a process.
    Disable(PolicyArgs),
    /// Allow a disabled process to start again.
    Enable(PolicyArgs),
    /// Show logs.
    Logs(LogsArgs),
    /// Open an instance in the browser.
    Open(Target),
    /// Print the rendered env with the source of each value.
    Env(EnvArgs),
    /// Manage projects.
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Expose apps on the public internet through the share tunnel.
    Share(ShareArgs),
    /// End the share of a stack.
    Unshare(Target),
    /// Stop every instance, or only idle ones.
    StopAll {
        #[arg(long)]
        idle: bool,
    },
    /// Set up this Mac: DNS resolver, CA trust and the daemon's LaunchAgent.
    Setup(SetupArgs),
    /// Create projects from another tool's setup.
    #[command(subcommand)]
    Import(ImportCommand),
    /// Watch and control everything in a terminal UI.
    Tui,
    /// Run the daemon in the foreground.
    Daemon,
    /// Runs one process for the daemon; not for direct use.
    #[command(name = "__shim", hide = true)]
    Shim {
        /// The lifeline socket inherited from the daemon.
        #[arg(long)]
        fd: i32,
        command: String,
    },
}

#[derive(Args, Clone, Default)]
pub struct Target {
    /// `app@branch/process`; every part is optional and inferred from the current directory.
    pub address: Option<String>,
}

#[derive(Args, Clone)]
pub struct PolicyArgs {
    #[command(flatten)]
    pub target: Target,
    /// Write the change to the project's config file.
    #[arg(long)]
    pub save: bool,
}

#[derive(Args, Clone)]
pub struct EnvArgs {
    #[command(flatten)]
    pub target: Target,
    /// The process whose env to show; defaults to the one serving HTTP.
    #[arg(long, short)]
    pub process: Option<String>,
}

#[derive(Args, Clone)]
pub struct ShareArgs {
    /// An app (`gwpa`, `api@feature-x`) or a stack (`@feature-x`) together with --apps.
    #[command(flatten)]
    pub target: Target,
    /// More apps of the same stack, comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub apps: Vec<String>,
    /// End the share after this long, like `2h`.
    #[arg(long)]
    pub ttl: Option<String>,
    /// Require a user name and password, which are printed.
    #[arg(long)]
    pub auth: bool,
    /// Keep the apps running while shared, so visitors never wait for a boot.
    #[arg(long)]
    pub pin: bool,
    /// With a provider that carries one app at a time (ngrok), stop sharing the
    /// current app instead of refusing.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Args, Clone)]
pub struct LogsArgs {
    #[command(flatten)]
    pub target: Target,
    /// Keep printing new lines.
    #[arg(short, long)]
    pub follow: bool,
    /// Lines from the buffer to show first.
    #[arg(short = 'n', long, default_value_t = 100)]
    pub lines: usize,
    /// Only lines containing this text.
    #[arg(long)]
    pub search: Option<String>,
}

#[derive(Args, Clone)]
pub struct SetupArgs {
    /// Show the plan without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Report what is set up, read-only.
    #[arg(long, conflicts_with_all = ["dry_run", "uninstall"])]
    pub check: bool,
    /// Undo setup: LaunchAgent, resolver file and the local CA.
    #[arg(long)]
    pub uninstall: bool,
    /// Set up sharing under this domain: a Cloudflare zone, or your ngrok dev domain
    /// (`<name>.ngrok-free.app`) to share through ngrok.
    #[arg(long, value_name = "DOMAIN", conflicts_with_all = ["check", "uninstall"])]
    pub share: Option<String>,
    /// The Cloudflare tunnel to create or reuse for --share.
    #[arg(long, default_value = "carte", requires = "share")]
    pub tunnel: String,
}

#[derive(Subcommand, Clone)]
pub enum ImportCommand {
    /// Create projects from the apps linked in ~/.puma-dev.
    PumaDev {
        /// Show what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask; accept the suggestions.
        #[arg(long, short)]
        yes: bool,
        /// The puma-dev directory.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
    },
}

#[derive(Subcommand, Clone)]
pub enum ProjectCommand {
    /// Create a project from a repo or a directory of repos.
    Add {
        path: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        strip_prefix: Option<String>,
        /// Web command for apps without a Procfile (non-interactive use).
        #[arg(long)]
        command: Option<String>,
        /// Don't ask; accept the suggestions.
        #[arg(long, short)]
        yes: bool,
    },
    Ls,
    /// Open the project's file in $EDITOR and validate it on save.
    Edit {
        name: String,
    },
    /// Stop the project's instances and delete its file.
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

pub fn run(cli: Cli) -> ExitCode {
    let paths = Paths::from_env();
    let result = match cli.command {
        Command::Shim { fd, command } => {
            return ExitCode::from(crate::shim::run(fd, &command) as u8);
        }
        Command::Daemon => control::daemon(&paths),
        Command::Tui => crate::tui::run(&paths),
        Command::Ls { all } => control::ls(&paths, all, cli.json),
        Command::Up(t) => control::control(&paths, t, carte_api::Action::Up, cli.json),
        Command::Down(t) => control::control(&paths, t, carte_api::Action::Down, cli.json),
        Command::Restart(t) => control::control(&paths, t, carte_api::Action::Restart, cli.json),
        Command::Pin(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Pin { save: p.save },
            cli.json,
        ),
        Command::Unpin(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Unpin { save: p.save },
            cli.json,
        ),
        Command::Disable(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Disable { save: p.save },
            cli.json,
        ),
        Command::Enable(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Enable { save: p.save },
            cli.json,
        ),
        Command::Logs(args) => control::logs(&paths, args, cli.json),
        Command::Open(t) => control::open(&paths, t),
        Command::StopAll { idle } => control::stop_all(&paths, idle, cli.json),
        Command::Share(args) => control::share(&paths, args, cli.json),
        Command::Unshare(t) => control::unshare(&paths, t, cli.json),
        Command::Env(args) => env::run(&paths, args, cli.json),
        Command::Project(cmd) => project::run(&paths, cmd, cli.json),
        Command::Setup(args) => setup::run(&paths, args),
        Command::Import(ImportCommand::PumaDev { dry_run, yes, from }) => {
            import::puma_dev(&paths, from, dry_run, yes)
        }
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("carte: {err:#}");
            ExitCode::FAILURE
        }
    }
}
