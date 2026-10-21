//! The env provider layer: mise, direnv, a login shell, or nothing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use thiserror::Error;

use super::{Env, Source};
use crate::model::EnvProviderKind;

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Replaces the daemon's environment when set.
    pub env: Option<BTreeMap<String, String>>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// Something the command started still had stdout open when it exited.
    pub stdout_held_open: bool,
}

/// Runs the provider commands; tests substitute a fake.
pub trait CommandRunner {
    fn find_program(&self, name: &str) -> Option<PathBuf>;
    /// Returns an `io::ErrorKind::TimedOut` error when `spec.timeout` passes.
    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput>;
}

/// Runs commands with `std::process`, inheriting the daemon's environment
/// unless the spec gives one.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    pub search_path: Vec<PathBuf>,
    /// Runs each command in a process group of its own, which a timeout kills
    /// whole. Off while stdin is a terminal, so a profile can prompt there and
    /// Ctrl-C reaches it.
    pub own_group: bool,
}

impl SystemRunner {
    /// Searches `$PATH`, then the usual install locations, since launchd's
    /// `PATH` doesn't include Homebrew or `~/.local/bin`.
    pub fn from_env() -> Self {
        let mut search_path: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        if let Some(home) = std::env::var_os("HOME") {
            search_path.push(Path::new(&home).join(".local/bin"));
        }
        search_path.push("/opt/homebrew/bin".into());
        search_path.push("/usr/local/bin".into());
        Self {
            search_path,
            own_group: !io::stdin().is_terminal(),
        }
    }
}

impl CommandRunner for SystemRunner {
    fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.search_path
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    }

    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
        let mut command = Command::new(&spec.program);
        if let Some(env) = &spec.env {
            command.env_clear().envs(env);
        }
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.own_group {
            command.process_group(0);
        }
        // SAFETY: the closure runs between fork and exec and makes only
        // async-signal-safe calls.
        unsafe {
            command.pre_exec(|| {
                cloexec_above_stderr();
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut stdout = Pipe::new(child.stdout.take().map(OwnedFd::from))?;
        let mut stderr = Pipe::new(child.stderr.take().map(OwnedFd::from))?;

        // A background process started by a shell profile can keep the pipes
        // open after the shell exits, so don't wait for EOF forever.
        let deadline = Instant::now() + spec.timeout;
        let mut exited = None;
        let mut grace = deadline;
        loop {
            stdout.drain();
            stderr.drain();
            if exited.is_none() {
                exited = child.try_wait()?;
                if exited.is_some() {
                    grace = deadline.min(Instant::now() + Duration::from_secs(1));
                }
            }
            match exited {
                Some(_) if stdout.is_closed() && stderr.is_closed() => break,
                Some(_) if Instant::now() >= grace => break,
                None if Instant::now() >= deadline => {
                    if self.own_group {
                        let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
                    } else {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{} timed out", spec.program.display()),
                    ));
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        Ok(CommandOutput {
            success: exited.is_some_and(|status| status.success()),
            stdout_held_open: !stdout.is_closed(),
            stdout: stdout.buf,
            stderr: String::from_utf8_lossy(&stderr.buf).into_owned(),
        })
    }
}

/// Marks every descriptor above stderr close-on-exec.
/// Runs between fork and exec, so only async-signal-safe calls.
fn cloexec_above_stderr() {
    #[cfg(target_os = "linux")]
    // SAFETY: close_range takes no pointers.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == 0
    {
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` outlives the call.
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.min(65536) as libc::c_int
    } else {
        1024
    };
    for fd in 3..max {
        // SAFETY: F_SETFD takes an int argument.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// A non-blocking read end of a child's pipe, closed at EOF or when dropped.
struct Pipe {
    file: Option<File>,
    buf: Vec<u8>,
}

impl Pipe {
    fn new(fd: Option<OwnedFd>) -> io::Result<Self> {
        if let Some(fd) = &fd {
            fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        }
        Ok(Self {
            file: fd.map(File::from),
            buf: Vec::new(),
        })
    }

    fn is_closed(&self) -> bool {
        self.file.is_none()
    }

    fn drain(&mut self) {
        let Some(file) = &mut self.file else {
            return;
        };
        let mut chunk = [0; 8192];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => break,
            }
        }
        self.file = None;
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("the env provider is set to {program}, but {program} was not found on PATH")]
    NotFound { program: String },
    #[error(
        "mise does not trust the config in {}. Run `mise trust` there, or add your projects directory to mise's trusted_config_paths so new worktrees are trusted automatically",
        checkout.display()
    )]
    MiseUntrusted { checkout: PathBuf },
    #[error("direnv is blocked in {}: run `direnv allow` there", checkout.display())]
    DirenvBlocked { checkout: PathBuf },
    #[error("{provider} failed in {}: {stderr}", checkout.display())]
    Failed {
        provider: String,
        checkout: PathBuf,
        stderr: String,
    },
    #[error("{provider} did not finish within {}s in {}", timeout.as_secs(), checkout.display())]
    TimedOut {
        provider: String,
        checkout: PathBuf,
        timeout: Duration,
    },
    #[error("could not run {provider} in {}: {source}", checkout.display())]
    Io {
        provider: String,
        checkout: PathBuf,
        source: io::Error,
    },
    #[error("could not read the output of {provider} in {}: {detail}", checkout.display())]
    BadOutput {
        provider: String,
        checkout: PathBuf,
        detail: String,
    },
}

/// The configured provider, or mise when it's installed and the checkout
/// has mise config, or the login shell.
pub fn detect_provider(
    configured: Option<EnvProviderKind>,
    checkout: &Path,
    runner: &dyn CommandRunner,
) -> EnvProviderKind {
    if let Some(kind) = configured {
        return kind;
    }
    if has_mise_config(checkout) && runner.find_program("mise").is_some() {
        EnvProviderKind::Mise
    } else {
        EnvProviderKind::Shell
    }
}

/// Whether mise has config for `dir`: in it, or in a directory above it up to its
/// repo's root. Outside a repo only `dir` itself counts, and `$HOME`, which holds
/// mise's global config, counts only as the app's own directory.
fn has_mise_config(dir: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    has_mise_config_below(dir, home.as_deref())
}

fn has_mise_config_below(dir: &Path, home: Option<&Path>) -> bool {
    let below_home: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| *d == dir || home != Some(*d))
        .collect();
    let dirs = match below_home.iter().position(|d| d.join(".git").exists()) {
        Some(repo) => &below_home[..=repo],
        None => &below_home[..1],
    };
    dirs.iter().any(|d| dir_has_mise_config(d))
}

/// Whether one directory has a mise config file of its own.
fn dir_has_mise_config(dir: &Path) -> bool {
    let any_file = |sub: &str, matches: fn(&str) -> bool| {
        std::fs::read_dir(dir.join(sub)).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry.file_name().to_str().is_some_and(matches) && entry.path().is_file()
            })
        })
    };
    let toml = |name: &str| name.ends_with(".toml");
    any_file("", is_mise_config_name)
        || any_file(".config", |name| {
            name.starts_with("mise.") && name.ends_with(".toml")
        })
        || any_file(".config/mise", toml)
        || any_file(".config/mise/conf.d", toml)
        || any_file("mise", toml)
        || any_file(".mise", toml)
        || any_file(".mise/conf.d", toml)
}

/// Whether a file name is one mise reads config from in a project directory itself:
/// `.tool-versions`, `mise.toml` and `.mise.toml`, and their `local` and per-env
/// variants such as `mise.local.toml` or `mise.dev.toml`.
pub fn is_mise_config_name(name: &str) -> bool {
    name == ".tool-versions"
        || ((name.starts_with("mise.") || name.starts_with(".mise.")) && name.ends_with(".toml"))
}

pub fn provider_name(kind: EnvProviderKind) -> &'static str {
    match kind {
        EnvProviderKind::Mise => "mise",
        EnvProviderKind::Direnv => "direnv",
        EnvProviderKind::Shell => "shell",
        EnvProviderKind::None => "none",
    }
}

/// Builds the provider layer for `checkout`. mise and direnv add to the
/// daemon's env, `shell` replaces it, and `none` is the daemon's env alone.
pub fn load_provider(
    kind: EnvProviderKind,
    checkout: &Path,
    daemon_env: &BTreeMap<String, String>,
    runner: &dyn CommandRunner,
) -> Result<Env, ProviderError> {
    let mut env = Env::new();
    if kind != EnvProviderKind::Shell {
        for (key, value) in daemon_env {
            env.set(key, value, Source::Daemon);
        }
    }
    let source = Source::Provider { provider: kind };
    match kind {
        EnvProviderKind::None => {}
        EnvProviderKind::Mise => {
            let output = run(runner, kind, "mise", &["env", "--json"], checkout)?;
            for (key, value) in parse_json(kind, checkout, &output)? {
                if let Some(value) = value {
                    env.set(key, value, source.clone());
                }
            }
        }
        EnvProviderKind::Direnv => {
            let output = run(runner, kind, "direnv", &["export", "json"], checkout)?;
            for (key, value) in parse_json(kind, checkout, &output)? {
                match value {
                    Some(value) => env.set(key, value, source.clone()),
                    None => {
                        env.remove(&key);
                    }
                }
            }
        }
        EnvProviderKind::Shell => {
            let shell = daemon_env
                .get("SHELL")
                .filter(|s| !s.is_empty())
                .map_or("/bin/zsh", String::as_str);
            let marker = "__CARTE_ENV__";
            let spec = CommandSpec {
                program: shell.into(),
                args: vec!["-l".into(), "-c".into(), format!("echo {marker}; env -0")],
                cwd: checkout.to_path_buf(),
                env: None,
                timeout: PROVIDER_TIMEOUT,
            };
            let output = execute(runner, kind, checkout, &spec)?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let Some((_, vars)) = stdout.split_once(&format!("{marker}\n")) else {
                return Err(ProviderError::BadOutput {
                    provider: provider_name(kind).into(),
                    checkout: checkout.to_path_buf(),
                    detail: format!("{shell} did not print the environment"),
                });
            };
            for entry in vars.split('\0') {
                if let Some((key, value)) = entry.split_once('=')
                    && !key.is_empty()
                    && !matches!(key, "_" | "SHLVL" | "PWD" | "OLDPWD")
                {
                    env.set(key, value, source.clone());
                }
            }
            apply_mise_over_shims(&mut env, checkout, runner)?;
        }
    }
    Ok(env)
}

/// mise's shims run `mise exec`, which applies mise's `[env]` over the
/// process's own env, so a shell env with shims on `PATH` gets `mise env`
/// layered on top: its `PATH` reaches the tools without the shims.
fn apply_mise_over_shims(
    env: &mut Env,
    checkout: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), ProviderError> {
    let shell_env = env.to_vars();
    let data_dir_shims = shell_env
        .get("MISE_DATA_DIR")
        .map(|dir| Path::new(dir).join("shims"));
    let has_shims = shell_env.get("PATH").is_some_and(|path| {
        std::env::split_paths(path).any(|dir| {
            dir.ends_with("mise/shims") || data_dir_shims.as_deref() == Some(dir.as_path())
        })
    });
    if !has_shims {
        return Ok(());
    }
    let Some(program) = runner.find_program("mise") else {
        return Ok(());
    };
    let kind = EnvProviderKind::Mise;
    let spec = CommandSpec {
        program,
        args: vec!["env".into(), "--json".into()],
        cwd: checkout.to_path_buf(),
        env: Some(shell_env),
        timeout: PROVIDER_TIMEOUT,
    };
    let output = execute(runner, kind, checkout, &spec)?;
    for (key, value) in parse_json(kind, checkout, &output)? {
        if let Some(value) = value
            && env.value(&key) != Some(value.as_str())
        {
            env.set(key, value, Source::Provider { provider: kind });
        }
    }
    Ok(())
}

fn run(
    runner: &dyn CommandRunner,
    kind: EnvProviderKind,
    program: &str,
    args: &[&str],
    checkout: &Path,
) -> Result<CommandOutput, ProviderError> {
    let path = runner
        .find_program(program)
        .ok_or_else(|| ProviderError::NotFound {
            program: program.into(),
        })?;
    let spec = CommandSpec {
        program: path,
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: checkout.to_path_buf(),
        env: None,
        timeout: PROVIDER_TIMEOUT,
    };
    execute(runner, kind, checkout, &spec)
}

fn execute(
    runner: &dyn CommandRunner,
    kind: EnvProviderKind,
    checkout: &Path,
    spec: &CommandSpec,
) -> Result<CommandOutput, ProviderError> {
    let provider = provider_name(kind).to_string();
    let checkout = checkout.to_path_buf();
    let output = runner.run(spec).map_err(|source| {
        if source.kind() == io::ErrorKind::TimedOut {
            ProviderError::TimedOut {
                provider: provider.clone(),
                checkout: checkout.clone(),
                timeout: spec.timeout,
            }
        } else {
            ProviderError::Io {
                provider: provider.clone(),
                checkout: checkout.clone(),
                source,
            }
        }
    })?;
    if output.success {
        return Ok(output);
    }
    let stderr = output.stderr.trim();
    Err(match kind {
        EnvProviderKind::Mise if stderr.contains("not trusted") => {
            ProviderError::MiseUntrusted { checkout }
        }
        EnvProviderKind::Direnv if stderr.contains("is blocked") => {
            ProviderError::DirenvBlocked { checkout }
        }
        _ => ProviderError::Failed {
            provider,
            checkout,
            stderr: stderr.to_string(),
        },
    })
}

fn parse_json(
    kind: EnvProviderKind,
    checkout: &Path,
    output: &CommandOutput,
) -> Result<BTreeMap<String, Option<String>>, ProviderError> {
    let bad_output = |detail: String| ProviderError::BadOutput {
        provider: provider_name(kind).into(),
        checkout: checkout.to_path_buf(),
        detail,
    };
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        if kind == EnvProviderKind::Mise {
            return Err(bad_output("mise printed nothing".into()));
        }
        if output.stdout_held_open {
            return Err(bad_output(format!(
                "{} printed nothing, and a process it started kept its output open",
                provider_name(kind)
            )));
        }
        return Ok(BTreeMap::new());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| bad_output(e.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    pub(crate) struct FakeRunner {
        pub programs: Vec<&'static str>,
        pub outputs: BTreeMap<String, io::Result<CommandOutput>>,
        pub calls: RefCell<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        pub(crate) fn with(mut self, program: &str, output: io::Result<CommandOutput>) -> Self {
            self.outputs.insert(program.into(), output);
            self
        }
    }

    pub(crate) fn ok(stdout: &str) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            ..Default::default()
        })
    }

    fn failed(stderr: &str) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: false,
            stderr: stderr.into(),
            ..Default::default()
        })
    }

    impl CommandRunner for FakeRunner {
        fn find_program(&self, name: &str) -> Option<PathBuf> {
            self.programs
                .contains(&name)
                .then(|| Path::new("/opt/homebrew/bin").join(name))
        }

        fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
            self.calls.borrow_mut().push(spec.clone());
            let name = spec.program.file_name().unwrap().to_str().unwrap();
            match self.outputs.get(name) {
                Some(Ok(output)) => Ok(output.clone()),
                Some(Err(e)) => Err(io::Error::new(e.kind(), e.to_string())),
                None => panic!("unexpected command {name}"),
            }
        }
    }

    fn daemon_env() -> BTreeMap<String, String> {
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/alex"),
            ("SHELL", "/bin/zsh"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    fn checkout_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            std::fs::write(dir.path().join(file), "").unwrap();
        }
        dir
    }

    #[test]
    fn detects_mise_from_any_config_file_when_installed() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        for file in [
            "mise.toml",
            ".mise.toml",
            "mise.local.toml",
            ".tool-versions",
            ".config/mise.toml",
            ".mise/config.toml",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
            assert_eq!(
                detect_provider(None, dir.path(), &runner),
                EnvProviderKind::Mise,
                "{file}"
            );
        }
    }

    #[test]
    fn detects_more_of_mises_config_names() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        for file in [
            "mise.dev.toml",
            ".mise.local.toml",
            ".config/mise/mise.toml",
            ".config/mise/config.local.toml",
            ".config/mise.local.toml",
            ".config/mise/conf.d/node.toml",
            ".mise/config.local.toml",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
            assert_eq!(
                detect_provider(None, dir.path(), &runner),
                EnvProviderKind::Mise,
                "{file}"
            );
        }
        assert!(!is_mise_config_name("misery.txt"));
        assert!(!is_mise_config_name("mise.json"));
    }

    #[test]
    fn detects_mise_from_the_repo_above_the_app() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mono = dir.path().join("mono");
        let web = mono.join("apps/web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::create_dir_all(mono.join(".git")).unwrap();
        assert_eq!(detect_provider(None, &web, &runner), EnvProviderKind::Shell);

        std::fs::write(dir.path().join("mise.toml"), "").unwrap();
        assert_eq!(
            detect_provider(None, &web, &runner),
            EnvProviderKind::Shell,
            "stops at the repo's root"
        );
        std::fs::write(mono.join("mise.toml"), "").unwrap();
        assert_eq!(detect_provider(None, &web, &runner), EnvProviderKind::Mise);
    }

    #[test]
    fn global_mise_config_doesnt_count_for_apps_outside_a_repo() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let app = home.join("sites/blog");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(home.join(".tool-versions"), "").unwrap();
        std::fs::create_dir_all(home.join(".config/mise")).unwrap();
        std::fs::write(home.join(".config/mise/config.toml"), "").unwrap();
        assert!(!has_mise_config_below(&app, Some(home)));
        assert!(
            !has_mise_config_below(&app, None),
            "no repo: only the app counts"
        );
        assert_eq!(detect_provider(None, &app, &runner), EnvProviderKind::Shell);
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert!(
            !has_mise_config_below(&app, Some(home)),
            "never above or at home"
        );
        assert!(
            has_mise_config_below(home, Some(home)),
            "home as the app itself"
        );
    }

    #[test]
    fn falls_back_to_shell_without_mise_or_its_config() {
        let installed = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        let dir = checkout_with(&["Gemfile"]);
        assert_eq!(
            detect_provider(None, dir.path(), &installed),
            EnvProviderKind::Shell
        );
        let dir = checkout_with(&["mise.toml"]);
        assert_eq!(
            detect_provider(None, dir.path(), &FakeRunner::default()),
            EnvProviderKind::Shell
        );
    }

    #[test]
    fn configured_provider_wins_over_detection() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        };
        let dir = checkout_with(&["mise.toml"]);
        assert_eq!(
            detect_provider(Some(EnvProviderKind::Direnv), dir.path(), &runner),
            EnvProviderKind::Direnv
        );
        assert_eq!(
            detect_provider(Some(EnvProviderKind::None), dir.path(), &runner),
            EnvProviderKind::None
        );
    }

    #[test]
    fn mise_adds_to_the_daemon_env() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "mise",
            ok(r#"{"PATH":"/Users/alex/.local/share/mise/installs/ruby/3.4/bin:/usr/bin:/bin","RUBY_YJIT_ENABLE":"1"}"#),
        );
        let env = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/nova-billing"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("HOME"), Some("/Users/alex"));
        assert_eq!(env.value("RUBY_YJIT_ENABLE"), Some("1"));
        let path = env.get("PATH").unwrap();
        assert!(path.value.starts_with("/Users/alex/.local/share/mise"));
        assert_eq!(
            path.source,
            Source::Provider {
                provider: EnvProviderKind::Mise
            }
        );
        assert_eq!(path.overrode[0].value, "/usr/bin:/bin");
        assert_eq!(path.overrode[0].source, Source::Daemon);

        let call = &runner.calls.borrow()[0];
        assert_eq!(call.program, Path::new("/opt/homebrew/bin/mise"));
        assert_eq!(call.args, ["env", "--json"]);
        assert_eq!(call.cwd, Path::new("/w/nova-billing"));
    }

    #[test]
    fn mise_untrusted_config_is_explained() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "mise",
            failed("mise ERROR Config files in ~/work/nova-billing-feature-x/mise.toml are not trusted.\nTrust them with `mise trust`."),
        );
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/nova-billing-feature-x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MiseUntrusted { .. }));
        let message = err.to_string();
        assert!(message.contains("/w/nova-billing-feature-x"));
        assert!(message.contains("trusted_config_paths"));
    }

    #[test]
    fn explicit_mise_without_mise_installed_is_an_error() {
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/x"),
            &daemon_env(),
            &FakeRunner::default(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the env provider is set to mise, but mise was not found on PATH"
        );
    }

    #[test]
    fn other_failures_carry_stderr() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("mise", failed("mise ERROR failed to parse mise.toml\n"));
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "mise failed in /w/x: mise ERROR failed to parse mise.toml"
        );
    }

    #[test]
    fn timeouts_are_reported() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("mise", Err(io::Error::new(io::ErrorKind::TimedOut, "slow")));
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "mise did not finish within 30s in /w/x");
    }

    #[test]
    fn bad_json_is_reported() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("mise", ok("PATH=/x"));
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::BadOutput { .. }));
    }

    #[test]
    fn direnv_sets_and_unsets() {
        let runner = FakeRunner {
            programs: vec!["direnv"],
            ..Default::default()
        }
        .with(
            "direnv",
            ok(r#"{"DATABASE_URL":"postgres:///billing","HOME":null,"DIRENV_DIR":"-/w/x"}"#),
        );
        let env = load_provider(
            EnvProviderKind::Direnv,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("DATABASE_URL"), Some("postgres:///billing"));
        assert_eq!(env.value("HOME"), None);
        assert_eq!(env.value("PATH"), Some("/usr/bin:/bin"));
        assert_eq!(runner.calls.borrow()[0].args, ["export", "json"]);
    }

    #[test]
    fn direnv_without_envrc_prints_nothing() {
        let runner = FakeRunner {
            programs: vec!["direnv"],
            ..Default::default()
        }
        .with("direnv", ok(""));
        let env = load_provider(
            EnvProviderKind::Direnv,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn direnv_printing_nothing_with_its_output_held_open_is_bad_output() {
        let runner = FakeRunner {
            programs: vec!["direnv"],
            ..Default::default()
        }
        .with(
            "direnv",
            Ok(CommandOutput {
                success: true,
                stdout_held_open: true,
                ..Default::default()
            }),
        );
        let err = load_provider(
            EnvProviderKind::Direnv,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::BadOutput { .. }), "{err}");
    }

    #[test]
    fn mise_printing_nothing_is_bad_output() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("mise", ok(""));
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mise printed nothing"), "{err}");
    }

    #[test]
    fn direnv_blocked_is_explained() {
        let runner = FakeRunner {
            programs: vec!["direnv"],
            ..Default::default()
        }
        .with(
            "direnv",
            failed(
                "direnv: error /w/x/.envrc is blocked. Run `direnv allow` to approve its content",
            ),
        );
        let err = load_provider(
            EnvProviderKind::Direnv,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "direnv is blocked in /w/x: run `direnv allow` there"
        );
    }

    #[test]
    fn shell_replaces_the_daemon_env_and_skips_profile_noise() {
        let runner = FakeRunner::default().with(
            "zsh",
            ok("Welcome back!\n__CARTE_ENV__\nPATH=/Users/alex/.rbenv/shims:/usr/bin\0HOME=/Users/alex\0SHLVL=1\0_=/usr/bin/env\0MULTI=a\nb=c\0"),
        );
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/nova-billing"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("PATH"), Some("/Users/alex/.rbenv/shims:/usr/bin"));
        assert!(env.get("PATH").unwrap().overrode.is_empty());
        assert_eq!(env.value("MULTI"), Some("a\nb=c"));
        assert_eq!(env.value("SHELL"), None);
        assert_eq!(env.value("SHLVL"), None);
        assert_eq!(env.value("_"), None);

        let call = &runner.calls.borrow()[0];
        assert_eq!(call.program, Path::new("/bin/zsh"));
        assert_eq!(call.args[..2], ["-l", "-c"]);
        assert_eq!(call.cwd, Path::new("/w/nova-billing"));
    }

    const SHELL_WITH_SHIMS: &str = "__CARTE_ENV__\nPATH=/Users/alex/.local/share/mise/shims:/usr/bin\0HOME=/Users/alex\0START_URL=http://localhost:3001\0";

    #[test]
    fn shell_with_mise_shims_gets_mise_env_on_top() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("zsh", ok(SHELL_WITH_SHIMS))
        .with(
            "mise",
            ok(r#"{"PATH":"/Users/alex/.local/share/mise/installs/ruby/3.4/bin:/Users/alex/.local/share/mise/shims:/usr/bin","START_URL":"http://localhost:3001","RUBY_YJIT_ENABLE":"1"}"#),
        );
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/make"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        let mise = Source::Provider {
            provider: EnvProviderKind::Mise,
        };
        let shell = Source::Provider {
            provider: EnvProviderKind::Shell,
        };
        let path = env.get("PATH").unwrap();
        assert!(
            path.value
                .starts_with("/Users/alex/.local/share/mise/installs")
        );
        assert_eq!(path.source, mise);
        assert_eq!(
            path.overrode[0].value,
            "/Users/alex/.local/share/mise/shims:/usr/bin"
        );
        assert_eq!(path.overrode[0].source, shell);
        assert_eq!(env.get("RUBY_YJIT_ENABLE").unwrap().source, mise);
        let start_url = env.get("START_URL").unwrap();
        assert_eq!(start_url.source, shell);
        assert!(start_url.overrode.is_empty());

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].program, Path::new("/opt/homebrew/bin/mise"));
        assert_eq!(calls[1].args, ["env", "--json"]);
        assert_eq!(calls[1].cwd, Path::new("/w/make"));
        let base = calls[1].env.as_ref().unwrap();
        assert_eq!(
            base.get("PATH").map(String::as_str),
            Some("/Users/alex/.local/share/mise/shims:/usr/bin")
        );
        assert_eq!(base.get("SHELL"), None);
    }

    #[test]
    fn shell_finds_shims_in_mise_data_dir() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "zsh",
            ok("__CARTE_ENV__\nPATH=/opt/tools/shims:/usr/bin\0MISE_DATA_DIR=/opt/tools\0"),
        )
        .with(
            "mise",
            ok(r#"{"PATH":"/opt/tools/installs/node/22/bin:/usr/bin"}"#),
        );
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(
            env.value("PATH"),
            Some("/opt/tools/installs/node/22/bin:/usr/bin")
        );
    }

    #[test]
    fn shell_without_mise_shims_skips_mise() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "zsh",
            ok("__CARTE_ENV__\nPATH=/Users/alex/.rbenv/shims:/usr/bin\0"),
        );
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("PATH"), Some("/Users/alex/.rbenv/shims:/usr/bin"));
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn shell_with_mise_shims_but_no_mise_keeps_the_shell_env() {
        let runner = FakeRunner::default().with("zsh", ok(SHELL_WITH_SHIMS));
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(
            env.value("PATH"),
            Some("/Users/alex/.local/share/mise/shims:/usr/bin")
        );
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn shell_with_mise_shims_reports_untrusted_mise_config() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with("zsh", ok(SHELL_WITH_SHIMS))
        .with(
            "mise",
            failed("mise ERROR Config files in ~/Dev/make/mise.toml are not trusted."),
        );
        let err = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MiseUntrusted { .. }));
    }

    #[test]
    fn shell_without_marker_is_bad_output() {
        let runner = FakeRunner::default().with("zsh", ok("nothing useful"));
        let err = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("/bin/zsh did not print the environment")
        );
    }

    #[test]
    fn none_is_the_daemon_env() {
        let env = load_provider(
            EnvProviderKind::None,
            Path::new("/w/x"),
            &daemon_env(),
            &FakeRunner::default(),
        )
        .unwrap();
        assert_eq!(env.len(), 3);
        assert_eq!(env.get("HOME").unwrap().source, Source::Daemon);
    }

    #[test]
    fn system_runner_captures_output() {
        let runner = SystemRunner {
            search_path: vec!["/bin".into(), "/usr/bin".into()],
            own_group: true,
        };
        let sh = runner.find_program("sh").unwrap();
        let output = runner
            .run(&CommandSpec {
                program: sh.clone(),
                args: vec!["-c".into(), "pwd; echo oops >&2; exit 3".into()],
                cwd: "/".into(),
                env: None,
                timeout: Duration::from_secs(10),
            })
            .unwrap();
        assert!(!output.success);
        assert_eq!(output.stdout, b"/\n");
        assert_eq!(output.stderr, "oops\n");
        assert!(runner.find_program("definitely-not-a-program").is_none());
    }

    #[test]
    fn system_runner_passes_on_no_descriptors_above_stderr() {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two fds pipe writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let runner = SystemRunner {
            search_path: vec!["/bin".into()],
            own_group: true,
        };
        let output = runner.run(&CommandSpec {
            program: runner.find_program("sh").unwrap(),
            args: vec![
                "-c".into(),
                format!("test -e /dev/fd/{} && echo open || echo closed", fds[1]),
            ],
            cwd: "/".into(),
            env: None,
            timeout: Duration::from_secs(10),
        });
        // SAFETY: both fds are ours and not used again.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert_eq!(output.unwrap().stdout, b"closed\n");
    }

    #[test]
    fn system_runner_replaces_the_env_when_given_one() {
        let runner = SystemRunner {
            search_path: vec!["/usr/bin".into()],
            own_group: true,
        };
        let output = runner
            .run(&CommandSpec {
                program: runner.find_program("env").unwrap(),
                args: Vec::new(),
                cwd: "/".into(),
                env: Some(BTreeMap::from([("ONLY".into(), "this".into())])),
                timeout: Duration::from_secs(10),
            })
            .unwrap();
        assert_eq!(output.stdout, b"ONLY=this\n");
    }

    #[test]
    fn system_runner_times_out() {
        let runner = SystemRunner {
            search_path: vec!["/bin".into()],
            own_group: true,
        };
        let started = Instant::now();
        let err = runner
            .run(&CommandSpec {
                program: "/bin/sleep".into(),
                args: vec!["5".into()],
                cwd: "/".into(),
                env: None,
                timeout: Duration::from_millis(100),
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Kills a process group when dropped, even when an assertion failed.
    /// The process group a test's shell led, killed when dropped unless it was seen
    /// to be gone, after which its id may belong to someone else.
    struct GroupGuard {
        group: Pid,
        alive: bool,
    }

    impl GroupGuard {
        fn new(group: Pid) -> Self {
            assert!(group.as_raw() > 1, "not a group of ours: {group}");
            Self { group, alive: true }
        }

        /// Waits up to five seconds for the group to be gone. A group that can't be
        /// signalled for lack of permission isn't gone.
        fn is_gone(&mut self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match killpg(self.group, None) {
                    Err(nix::errno::Errno::ESRCH) => {
                        self.alive = false;
                        return true;
                    }
                    _ if Instant::now() >= deadline => return false,
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    }

    impl Drop for GroupGuard {
        fn drop(&mut self) {
            if self.alive && killpg(self.group, None).is_ok() {
                let _ = killpg(self.group, Signal::SIGKILL);
            }
        }
    }

    fn read_pid(path: &Path) -> Pid {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return Pid::from_raw(pid);
            }
            assert!(Instant::now() < deadline, "no pid in {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn system_runner_keeps_output_when_a_background_process_holds_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let runner = SystemRunner {
            search_path: vec!["/bin".into()],
            own_group: true,
        };
        let started = Instant::now();
        let output = runner
            .run(&CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "echo $$ > {}; (while kill -0 $$ 2>/dev/null; do sleep 0.01; done; while echo more; do sleep 0.05; done) & echo {{}}",
                        pid_file.display()
                    ),
                ],
                cwd: "/".into(),
                env: None,
                timeout: Duration::from_secs(30),
            })
            .unwrap();
        let mut group = GroupGuard::new(read_pid(&pid_file));
        assert!(output.success);
        assert!(output.stdout.starts_with(b"{}\n"), "{:?}", output.stdout);
        assert!(output.stdout_held_open);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            group.is_gone(),
            "the writer outlived its closed pipe and kept running"
        );
    }

    #[test]
    fn system_runner_kills_the_process_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let runner = SystemRunner {
            search_path: vec!["/bin".into()],
            own_group: true,
        };
        let err = runner
            .run(&CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!("echo $$ > {}; sleep 30 & sleep 30", pid_file.display()),
                ],
                cwd: "/".into(),
                env: None,
                timeout: Duration::from_millis(500),
            })
            .unwrap_err();
        let mut group = GroupGuard::new(read_pid(&pid_file));
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(group.is_gone(), "the provider's group outlived the timeout");
    }
}
