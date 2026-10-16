//! The env provider layer: mise, direnv, a login shell, or nothing.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::{Env, Source};
use crate::model::EnvProviderKind;

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs the provider commands; tests substitute a fake.
pub trait CommandRunner {
    fn find_program(&self, name: &str) -> Option<PathBuf>;
    /// Returns an `io::ErrorKind::TimedOut` error when `spec.timeout` passes.
    fn run(&self, spec: &CommandSpec) -> io::Result<CommandOutput>;
}

/// Runs commands with `std::process`, inheriting the daemon's environment.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    pub search_path: Vec<PathBuf>,
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
        Self { search_path }
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
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (tx, rx) = mpsc::channel();
        for (is_stdout, mut pipe) in [
            (
                true,
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            ),
            (
                false,
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
            ),
        ]
        .into_iter()
        .filter_map(|(o, p)| p.map(|p| (o, p)))
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((is_stdout, buf));
            });
        }
        drop(tx);

        let deadline = Instant::now() + spec.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} timed out", spec.program.display()),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        // A background process started by a shell profile can keep the pipes
        // open after the shell exits, so don't wait for EOF forever.
        let mut output = CommandOutput {
            success: status.success(),
            ..Default::default()
        };
        let grace = Instant::now() + Duration::from_secs(1);
        for _ in 0..2 {
            let wait = deadline
                .max(grace)
                .saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok((true, buf)) => output.stdout = buf,
                Ok((false, buf)) => output.stderr = String::from_utf8_lossy(&buf).into_owned(),
                Err(_) => break,
            }
        }
        Ok(output)
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
    let has_mise_config = ["mise.toml", ".mise.toml", ".tool-versions"]
        .iter()
        .any(|name| checkout.join(name).is_file());
    if has_mise_config && runner.find_program("mise").is_some() {
        EnvProviderKind::Mise
    } else {
        EnvProviderKind::Shell
    }
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
            for (key, value) in parse_json(kind, checkout, &output.stdout)? {
                if let Some(value) = value {
                    env.set(key, value, source.clone());
                }
            }
        }
        EnvProviderKind::Direnv => {
            let output = run(runner, kind, "direnv", &["export", "json"], checkout)?;
            for (key, value) in parse_json(kind, checkout, &output.stdout)? {
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
        }
    }
    Ok(env)
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
    stdout: &[u8],
) -> Result<BTreeMap<String, Option<String>>, ProviderError> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(BTreeMap::new());
    }
    serde_json::from_slice(stdout).map_err(|e| ProviderError::BadOutput {
        provider: provider_name(kind).into(),
        checkout: checkout.to_path_buf(),
        detail: e.to_string(),
    })
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
            stderr: String::new(),
        })
    }

    fn failed(stderr: &str) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.into(),
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
            ("HOME", "/Users/inge"),
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
        for file in ["mise.toml", ".mise.toml", ".tool-versions"] {
            let dir = checkout_with(&[file]);
            assert_eq!(
                detect_provider(None, dir.path(), &runner),
                EnvProviderKind::Mise
            );
        }
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
            ok(r#"{"PATH":"/Users/inge/.local/share/mise/installs/ruby/3.4/bin:/usr/bin:/bin","RUBY_YJIT_ENABLE":"1"}"#),
        );
        let env = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/make-billing"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("HOME"), Some("/Users/inge"));
        assert_eq!(env.value("RUBY_YJIT_ENABLE"), Some("1"));
        let path = env.get("PATH").unwrap();
        assert!(path.value.starts_with("/Users/inge/.local/share/mise"));
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
        assert_eq!(call.cwd, Path::new("/w/make-billing"));
    }

    #[test]
    fn mise_untrusted_config_is_explained() {
        let runner = FakeRunner {
            programs: vec!["mise"],
            ..Default::default()
        }
        .with(
            "mise",
            failed("mise ERROR Config files in ~/work/make-billing-feature-x/mise.toml are not trusted.\nTrust them with `mise trust`."),
        );
        let err = load_provider(
            EnvProviderKind::Mise,
            Path::new("/w/make-billing-feature-x"),
            &daemon_env(),
            &runner,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MiseUntrusted { .. }));
        let message = err.to_string();
        assert!(message.contains("/w/make-billing-feature-x"));
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
            ok("Welcome back!\n__CARTE_ENV__\nPATH=/Users/inge/.rbenv/shims:/usr/bin\0HOME=/Users/inge\0SHLVL=1\0_=/usr/bin/env\0MULTI=a\nb=c\0"),
        );
        let env = load_provider(
            EnvProviderKind::Shell,
            Path::new("/w/make-billing"),
            &daemon_env(),
            &runner,
        )
        .unwrap();
        assert_eq!(env.value("PATH"), Some("/Users/inge/.rbenv/shims:/usr/bin"));
        assert!(env.get("PATH").unwrap().overrode.is_empty());
        assert_eq!(env.value("MULTI"), Some("a\nb=c"));
        assert_eq!(env.value("SHELL"), None);
        assert_eq!(env.value("SHLVL"), None);
        assert_eq!(env.value("_"), None);

        let call = &runner.calls.borrow()[0];
        assert_eq!(call.program, Path::new("/bin/zsh"));
        assert_eq!(call.args[..2], ["-l", "-c"]);
        assert_eq!(call.cwd, Path::new("/w/make-billing"));
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
        };
        let sh = runner.find_program("sh").unwrap();
        let output = runner
            .run(&CommandSpec {
                program: sh.clone(),
                args: vec!["-c".into(), "pwd; echo oops >&2; exit 3".into()],
                cwd: "/".into(),
                timeout: Duration::from_secs(10),
            })
            .unwrap();
        assert!(!output.success);
        assert_eq!(output.stdout, b"/\n");
        assert_eq!(output.stderr, "oops\n");
        assert!(runner.find_program("definitely-not-a-program").is_none());
    }

    #[test]
    fn system_runner_times_out() {
        let runner = SystemRunner {
            search_path: vec!["/bin".into()],
        };
        let started = Instant::now();
        let err = runner
            .run(&CommandSpec {
                program: "/bin/sleep".into(),
                args: vec!["5".into()],
                cwd: "/".into(),
                timeout: Duration::from_millis(100),
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
