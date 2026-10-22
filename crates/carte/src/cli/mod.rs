//! The `carte` command line.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::exit::{self, Exit};
use crate::paths::Paths;

pub(crate) mod attach;
mod control;
mod env;
mod exec;
mod import;
mod import_kube;
mod mcp;
mod project;
pub(crate) mod prompt;
mod prune;
mod setup;

#[derive(Parser)]
#[command(
    name = "carte",
    version,
    about = "Runs your apps' processes on demand",
    after_help = "Without a command, carte opens the terminal UI."
)]
pub struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List projects, stacks, apps and their state.
    Ls {
        /// Include stopped instances.
        #[arg(long, short)]
        all: bool,
    },
    /// Show the state of the app here, or of an address: URL, processes, errors.
    Status(Target),
    /// Start an instance, a stack or a process, and wait until it's ready (lazy start
    /// starts apps too).
    Up(StartArgs),
    /// Stop an instance, a stack or a process.
    Down(Target),
    /// Restart an instance, a stack or a process, and wait until it's ready.
    Restart(StartArgs),
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
    /// Show what carte did and why: starts, stops, crashes, reloads, shares.
    Events(EventsArgs),
    /// Open an instance in the browser.
    Open(Target),
    /// Print the rendered env with the source of each value; secrets stay hidden.
    Env(EnvArgs),
    /// Run a command in the foreground with an app's env, like `bin/rails console`.
    Exec(ExecArgs),
    /// Connect your terminal to a running process, to answer a debugger prompt.
    Attach(AttachArgs),
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
    /// Remove state folders, run directories and pidfiles of stacks, apps and
    /// projects that no longer exist.
    Prune(PruneArgs),
    /// Set up this machine: DNS for the TLD, CA trust and the daemon's service.
    Setup(SetupArgs),
    /// Create projects from another tool's setup.
    #[command(subcommand)]
    Import(ImportCommand),
    /// Watch and control everything in a terminal UI.
    Tui,
    /// Run the daemon in the foreground, or manage the one launchd or systemd runs.
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Serve the Model Context Protocol over stdio, for coding agents.
    Mcp {
        /// Also offer start, restart and stop.
        #[arg(long)]
        control: bool,
    },
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
    /// `app@stack/process`; every part is optional and inferred from the current directory.
    pub address: Option<String>,
    /// Resolve the address inside this project; alone, it addresses the whole project
    /// even when an app has the same name.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
}

#[derive(Args, Clone)]
pub struct StartArgs {
    #[command(flatten)]
    pub target: Target,
    /// Return once the start is requested instead of waiting until the app is ready.
    #[arg(long)]
    pub no_wait: bool,
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
    /// Show only this variable, with its value. Alone, `carte env KEY` means the app here.
    pub key: Option<String>,
    /// The process whose env to show; defaults to the one serving HTTP.
    #[arg(long, short)]
    pub process: Option<String>,
    /// Show every value; values from the environment and env files are hidden otherwise.
    #[arg(long)]
    pub values: bool,
}

#[derive(Args, Clone)]
pub struct ExecArgs {
    #[command(flatten)]
    pub target: Target,
    /// Use this process's env, with its `PORT` while it runs; defaults to the env every
    /// process of the app shares, without `PORT`.
    #[arg(long, short)]
    pub process: Option<String>,
    /// Run even when a process that provides env, like a Redis service, isn't running.
    #[arg(long)]
    pub no_check: bool,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args, Clone)]
pub struct AttachArgs {
    #[command(flatten)]
    pub target: Target,
    /// The process; defaults to the one serving HTTP.
    pub process: Option<String>,
    /// The keys that detach, like `ctrl-q d`; overrides `attach.detach_key`.
    #[arg(long, value_name = "KEYS")]
    pub detach_keys: Option<String>,
    /// Without a terminal: how long the output must be quiet after the input ends;
    /// raise it for commands that take a while to answer.
    #[arg(long, value_name = "DURATION", default_value = "500ms", value_parser = parse_duration)]
    pub quiet: std::time::Duration,
    /// Without a terminal: how long to print output after the input ends, at most.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = parse_duration)]
    pub wait: std::time::Duration,
}

#[derive(Args, Clone)]
pub struct ShareArgs {
    /// An app (`wiki`, `api@feature-x`) or a stack (`@feature-x`) together with --apps.
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
    /// Lines from the buffer to show first [default: 100, or all with --since or
    /// --since-start]
    #[arg(short = 'n', long)]
    pub lines: Option<usize>,
    /// Only lines containing this text.
    #[arg(long)]
    pub search: Option<String>,
    /// Only lines from this long ago on, like `5m` or `1h30m`.
    #[arg(long, value_parser = parse_duration)]
    pub since: Option<std::time::Duration>,
    /// Only lines from each process's latest run.
    #[arg(long)]
    pub since_start: bool,
    /// The lines most relevant to these words, best first, with their time.
    #[arg(long, value_name = "WORDS", conflicts_with_all = ["follow", "lines"])]
    pub rank: Option<String>,
    /// How many lines --rank shows.
    #[arg(long, requires = "rank", default_value_t = 20)]
    pub top: usize,
    /// Add a marker with the time to these logs instead of showing them.
    #[arg(
        long,
        conflicts_with_all = ["follow", "lines", "search", "since", "since_start", "rank"]
    )]
    pub mark: bool,
}

fn parse_duration(value: &str) -> Result<std::time::Duration, String> {
    carte_core::duration::parse(value).map_err(|err| err.to_string())
}

#[derive(Args, Clone)]
pub struct EventsArgs {
    /// `app@stack/process` or a project; without one (or --project), every event.
    pub address: Option<String>,
    /// Events about this project, or resolve the address inside it.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// Keep printing new events.
    #[arg(short, long)]
    pub follow: bool,
    /// Events from the buffer to show first.
    #[arg(short = 'n', long, default_value_t = 50)]
    pub lines: usize,
}

#[derive(Args, Clone)]
pub struct PruneArgs {
    /// List what would be removed without removing it.
    #[arg(long)]
    pub dry_run: bool,
    /// Don't ask; remove everything listed.
    #[arg(long, short, conflicts_with = "dry_run")]
    pub yes: bool,
}

#[derive(Args, Clone)]
pub struct SetupArgs {
    /// Show the plan without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Report what is set up, read-only.
    #[arg(long, conflicts_with_all = ["dry_run", "uninstall"])]
    pub check: bool,
    /// Undo setup: the daemon's service, DNS for the TLD and the local CA.
    #[arg(long)]
    pub uninstall: bool,
    /// Set up sharing under this domain: a Cloudflare zone, or your ngrok dev domain
    /// (`<name>.ngrok-free.app`) to share through ngrok.
    #[arg(long, value_name = "DOMAIN", conflicts_with_all = ["check", "uninstall"])]
    pub share: Option<String>,
    /// The Cloudflare tunnel to create or reuse for --share.
    #[arg(long, default_value = "carte", requires = "share")]
    pub tunnel: String,
    /// Leave the skill for coding agents (~/.agents/skills/carte) alone.
    #[arg(long, conflicts_with_all = ["check", "share"])]
    pub no_skill: bool,
    /// Serve apps under this TLD and save it in config.toml: `localhost` needs no
    /// resolver, anything else (like `test`) gets one in /etc/resolver.
    #[arg(long, value_name = "TLD", conflicts_with_all = ["check", "uninstall", "share"])]
    pub tld: Option<String>,
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
    /// Create kube projects from the contexts of your kubeconfig.
    Kube {
        /// Show what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask; add every context whose name is free.
        #[arg(long, short)]
        yes: bool,
        /// Read this kubeconfig, and name it in the projects, instead of
        /// $KUBECONFIG or ~/.kube/config.
        #[arg(long, value_name = "FILE")]
        kubeconfig: Option<PathBuf>,
        /// Write the projects into this folder of projects/, like `clusters`.
        #[arg(long, value_name = "FOLDER")]
        folder: Option<String>,
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

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Restart the daemon launchd or systemd runs, so it picks up a new build or `[share]` changes.
    Restart,
}

pub fn run(cli: Cli) -> ExitCode {
    let paths = Paths::from_env();
    if !matches!(
        cli.command,
        Some(Command::Shim { .. } | Command::Daemon { command: None })
    ) {
        // SAFETY: called before any other thread exists.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        refresh_skill();
    }
    if !matches!(
        cli.command,
        None | Some(
            Command::Shim { .. } | Command::Daemon { .. } | Command::Mcp { .. } | Command::Tui
        )
    ) {
        crate::client::warn_about_version();
    }
    let Some(command) = cli.command else {
        if !std::io::stdout().is_terminal() {
            let _ = <Cli as clap::CommandFactory>::command().print_help();
            return Exit::Usage.into();
        }
        return finish(crate::tui::run(&paths));
    };
    let result = match command {
        Command::Shim { fd, command } => {
            return ExitCode::from(crate::shim::run(fd, &command) as u8);
        }
        Command::Daemon { command: None } => control::daemon(&paths),
        Command::Daemon {
            command: Some(DaemonCommand::Restart),
        } => control::restart_daemon(&paths),
        Command::Mcp { control } => mcp::run(&paths, control),
        Command::Tui => crate::tui::run(&paths),
        Command::Ls { all } => control::ls(&paths, all, cli.json),
        Command::Status(t) => control::status(&paths, t, cli.json),
        Command::Up(s) => control::control(
            &paths,
            s.target,
            carte_api::Action::Up,
            !s.no_wait,
            cli.json,
        ),
        Command::Down(t) => control::control(&paths, t, carte_api::Action::Down, false, cli.json),
        Command::Restart(s) => control::control(
            &paths,
            s.target,
            carte_api::Action::Restart,
            !s.no_wait,
            cli.json,
        ),
        Command::Pin(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Pin { save: p.save },
            false,
            cli.json,
        ),
        Command::Unpin(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Unpin { save: p.save },
            false,
            cli.json,
        ),
        Command::Disable(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Disable { save: p.save },
            false,
            cli.json,
        ),
        Command::Enable(p) => control::control(
            &paths,
            p.target,
            carte_api::Action::Enable { save: p.save },
            false,
            cli.json,
        ),
        Command::Logs(args) => control::logs(&paths, args, cli.json),
        Command::Events(args) => control::events(&paths, args, cli.json),
        Command::Open(t) => control::open(&paths, t),
        Command::StopAll { idle } => control::stop_all(&paths, idle, cli.json),
        Command::Share(args) => control::share(&paths, args, cli.json),
        Command::Unshare(t) => control::unshare(&paths, t, cli.json),
        Command::Env(args) => env::run(&paths, args, cli.json),
        Command::Exec(args) => exec::run(&paths, args),
        Command::Attach(args) => attach::run(&paths, args),
        Command::Project(cmd) => project::run(&paths, cmd, cli.json),
        Command::Prune(args) => prune::run(&paths, args, cli.json),
        Command::Setup(args) => setup::run(&paths, args),
        Command::Import(ImportCommand::PumaDev { dry_run, yes, from }) => {
            import::puma_dev(&paths, from, dry_run, yes)
        }
        Command::Import(ImportCommand::Kube {
            dry_run,
            yes,
            kubeconfig,
            folder,
        }) => import_kube::run(&paths, kubeconfig, folder, dry_run, yes),
    };
    finish(result)
}

/// Brings a skill `carte setup` installed up to this version. `CARTE_SKILL_REFRESH=0`
/// turns it off.
fn refresh_skill() {
    if std::env::var_os("CARTE_SKILL_REFRESH").is_some_and(|v| v == "0") {
        return;
    }
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return;
    };
    let _ = crate::setup::skill::refresh(&crate::setup::skill::dir_in(&PathBuf::from(home)));
}

fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("carte: {err:#}");
            exit::code(&err).into()
        }
    }
}
