//! `carte setup`: the one-time machine changes, planned as data and shown before they run.

pub mod check;
pub mod plist;

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use carte_core::config;
use carte_core::model::MachineConfig;

use crate::cli::prompt::tilde;
use crate::dns;
use crate::paths::Paths;
use crate::tls::trust::{self, CommandLine};
use crate::tls::{CertificateAuthority, CreateReason};
use plist::{Agent, LABEL};

pub const PUMA_DEV_LABEL: &str = "io.puma.dev";

/// Where setup looks and writes; tests point every path at a temp dir.
#[derive(Debug, Clone)]
pub struct SetupEnv {
    pub paths: Paths,
    pub machine: MachineConfig,
    pub home: PathBuf,
    pub uid: u32,
    pub exe: PathBuf,
    pub resolver_dir: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub puma_dev_dir: PathBuf,
}

impl SetupEnv {
    pub fn from_system(paths: &Paths) -> anyhow::Result<Self> {
        let machine = config::load_machine_config(&paths.machine_config())?;
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let resolver_dir = dns::resolver_file_path(&machine.tld)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/etc/resolver"));
        Ok(Self {
            paths: paths.clone(),
            machine,
            // SAFETY: getuid has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            exe: std::env::current_exe().context("finding the carte executable")?,
            resolver_dir,
            launch_agents_dir: home.join("Library/LaunchAgents"),
            puma_dev_dir: home.join(".puma-dev"),
            home,
        })
    }

    pub fn resolver_file(&self) -> PathBuf {
        self.resolver_dir.join(&self.machine.tld)
    }

    pub fn resolver_contents(&self) -> String {
        dns::resolver_file_contents(self.machine.dns_port)
    }

    pub fn agent_plist(&self) -> PathBuf {
        self.launch_agents_dir.join(format!("{LABEL}.plist"))
    }

    pub fn puma_dev_plist(&self) -> PathBuf {
        self.launch_agents_dir
            .join(format!("{PUMA_DEV_LABEL}.plist"))
    }

    pub fn agent(&self) -> Agent {
        Agent {
            exe: self.exe.clone(),
            home: self.home.clone(),
            config_dir: self.paths.config_dir.clone(),
            state_dir: self.paths.state_dir.clone(),
            http_port: self.machine.http_port,
            https_port: self.machine.https_port,
        }
    }

    pub fn ca_cert(&self) -> PathBuf {
        CertificateAuthority::cert_path_in(&self.paths.state_dir)
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service(&self, label: &str) -> String {
        format!("gui/{}/{label}", self.uid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaState {
    Missing,
    /// It exists but `load_or_create` would replace it.
    Replace(String),
    Untrusted,
    Trusted,
    /// The trust check itself failed.
    Unknown(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PumaDev {
    pub plist: bool,
    pub loaded: bool,
    /// Entries in `~/.puma-dev`.
    pub links: usize,
}

impl PumaDev {
    pub fn installed(&self) -> bool {
        self.plist || self.loaded
    }
}

/// What setup found on the machine, read without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub resolver: Option<String>,
    pub ca: CaState,
    pub agent_plist: Option<String>,
    pub agent_loaded: bool,
    pub puma_dev: PumaDev,
    pub projects: usize,
    pub daemon_running: bool,
}

impl Facts {
    pub fn gather(env: &SetupEnv) -> Self {
        let ca =
            match CertificateAuthority::load_existing(&env.paths.state_dir, &[&env.machine.tld]) {
                Err(CreateReason::Missing) => CaState::Missing,
                Err(CreateReason::Invalid(why)) => CaState::Replace(why),
                Err(CreateReason::Expiring) => CaState::Replace("it is about to expire".into()),
                Err(CreateReason::TldsChanged { previous }) => {
                    CaState::Replace(format!("it was made for .{}", previous.join(", .")))
                }
                Ok(ca) => match trust::is_trusted(&ca) {
                    Ok(true) => CaState::Trusted,
                    Ok(false) => CaState::Untrusted,
                    Err(err) => CaState::Unknown(err.to_string()),
                },
            };
        let projects = fs::read_dir(env.paths.projects_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
                    .count()
            })
            .unwrap_or(0);
        Facts {
            resolver: fs::read_to_string(env.resolver_file()).ok(),
            ca,
            agent_plist: fs::read_to_string(env.agent_plist()).ok(),
            agent_loaded: check::launchd_loaded(&env.service(LABEL)),
            puma_dev: PumaDev {
                plist: env.puma_dev_plist().exists(),
                loaded: check::launchd_loaded(&env.service(PUMA_DEV_LABEL)),
                links: fs::read_dir(&env.puma_dev_dir)
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or(0),
            },
            projects,
            daemon_running: UnixStream::connect(env.paths.socket()).is_ok(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run {
        command: CommandLine,
        sudo: bool,
        stdin: Option<String>,
    },
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    RemoveFile {
        path: PathBuf,
    },
    CreateCa,
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub action: Action,
}

impl Step {
    fn run(description: impl Into<String>, program: &str, args: &[&str]) -> Self {
        Step {
            description: description.into(),
            action: Action::Run {
                command: CommandLine::new(program, args),
                sudo: false,
                stdin: None,
            },
        }
    }

    fn sudo(description: impl Into<String>, program: &str, args: &[&str]) -> Self {
        let mut step = Self::run(description, program, args);
        if let Action::Run { sudo, .. } = &mut step.action {
            *sudo = true;
        }
        step
    }

    fn with_stdin(mut self, contents: String) -> Self {
        if let Action::Run { stdin, .. } = &mut self.action {
            *stdin = Some(contents);
        }
        self
    }

    pub fn needs_sudo(&self) -> bool {
        matches!(self.action, Action::Run { sudo: true, .. })
    }

    /// The step as shown to the user: its description, then the command or file it touches.
    pub fn render(&self, number: usize, env: &SetupEnv, show_files: bool) -> String {
        let mut out = format!("{number}. {}\n", self.description);
        let indented = |out: &mut String, text: &str| {
            for line in text.lines() {
                let _ = writeln!(out, "     | {line}");
            }
        };
        match &self.action {
            Action::Run {
                command,
                sudo,
                stdin,
            } => {
                let sudo = if *sudo { "sudo " } else { "" };
                let _ = writeln!(out, "   $ {sudo}{command}");
                if let Some(stdin) = stdin {
                    indented(&mut out, stdin);
                }
            }
            Action::WriteFile { path, contents } => {
                let _ = writeln!(out, "   write {}", tilde(path));
                if show_files {
                    indented(&mut out, contents);
                }
            }
            Action::RemoveFile { path } => {
                let _ = writeln!(out, "   $ rm {}", tilde(path));
            }
            Action::CreateCa => {
                let _ = writeln!(out, "   write {}", tilde(&env.ca_cert()));
            }
            Action::Verify => {
                let _ = writeln!(
                    out,
                    "   check DNS, ports {} and {}, and the daemon's socket",
                    env.machine.http_port, env.machine.https_port
                );
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn render(&self, env: &SetupEnv, show_files: bool) -> String {
        let mut out = String::new();
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&step.render(i + 1, env, show_files));
        }
        for note in &self.notes {
            let _ = writeln!(out, "\nnote: {note}");
        }
        out
    }

    pub fn changes_anything(&self) -> bool {
        self.steps.iter().any(|s| s.action != Action::Verify)
    }
}

pub fn plan_install(env: &SetupEnv, facts: &Facts) -> Plan {
    let mut plan = Plan::default();
    let steps = &mut plan.steps;
    let puma_dev = env.service(PUMA_DEV_LABEL);
    if facts.puma_dev.plist {
        steps.push(Step::run(
            "Keep puma-dev's LaunchAgent from starting at login (its plist and ~/.puma-dev stay)",
            "launchctl",
            &["disable", &puma_dev],
        ));
    }
    if facts.puma_dev.loaded {
        steps.push(Step::run(
            "Stop puma-dev so carte can take over its ports",
            "launchctl",
            &["bootout", &puma_dev],
        ));
    }

    let resolver = env.resolver_contents();
    if facts.resolver.as_deref() != Some(resolver.as_str()) {
        let file = env.resolver_file();
        if facts.resolver.is_none() {
            let dir = env.resolver_dir.to_string_lossy();
            steps.push(Step::sudo(
                format!("Create {}", env.resolver_dir.display()),
                "mkdir",
                &["-p", &dir],
            ));
        }
        steps.push(
            Step::sudo(
                format!(
                    "Send .{} lookups to carte's DNS server on port {}",
                    env.machine.tld, env.machine.dns_port
                ),
                "tee",
                &[&file.to_string_lossy()],
            )
            .with_stdin(resolver),
        );
    }

    let ca_cert = env.ca_cert();
    let trust_step = || {
        let command = trust::trust_command(&ca_cert);
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        Step::sudo(
            "Trust carte's local CA in the system keychain (it can only sign .test names)",
            &command.program,
            &args,
        )
    };
    match &facts.ca {
        CaState::Missing | CaState::Replace(_) => {
            let why = match &facts.ca {
                CaState::Replace(why) => format!(", replacing the old one: {why}"),
                _ => String::new(),
            };
            steps.push(Step {
                description: format!("Create carte's local CA{why}"),
                action: Action::CreateCa,
            });
            steps.push(trust_step());
        }
        CaState::Untrusted | CaState::Unknown(_) => steps.push(trust_step()),
        CaState::Trusted => {}
    }

    let plist = env.agent().to_plist();
    let plist_path = env.agent_plist();
    let plist_changed = facts.agent_plist.as_deref() != Some(plist.as_str());
    if plist_changed {
        if facts.agent_loaded {
            steps.push(Step::run(
                "Stop the running carte LaunchAgent to replace it",
                "launchctl",
                &["bootout", &env.service(LABEL)],
            ));
        }
        steps.push(Step {
            description: format!(
                "Install the LaunchAgent that runs `carte daemon` with ports {} and {} from launchd",
                env.machine.http_port, env.machine.https_port
            ),
            action: Action::WriteFile {
                path: plist_path.clone(),
                contents: plist,
            },
        });
    }
    if plist_changed || !facts.agent_loaded {
        steps.push(Step::run(
            "Start the daemon now and at every login",
            "launchctl",
            &["bootstrap", &env.domain(), &plist_path.to_string_lossy()],
        ));
    }
    steps.push(Step {
        description: "Check that carte answers".into(),
        action: Action::Verify,
    });

    if facts.daemon_running && !facts.agent_loaded {
        plan.notes.push(format!(
            "a carte daemon is already running on {}; stop it first, or the LaunchAgent's daemon will refuse to start",
            tilde(&env.paths.socket())
        ));
    }
    if env.exe.components().any(|c| c.as_os_str() == "target") {
        plan.notes.push(format!(
            "the LaunchAgent will run {}, a build directory; install carte (`cargo install --path crates/carte`) and run setup from there to keep it working after a `cargo clean`",
            tilde(&env.exe)
        ));
    }
    plan
}

pub fn plan_uninstall(env: &SetupEnv, facts: &Facts) -> Plan {
    let mut plan = Plan::default();
    if facts.agent_loaded {
        plan.steps.push(Step::run(
            "Stop the carte daemon and its apps",
            "launchctl",
            &["bootout", &env.service(LABEL)],
        ));
    }
    if facts.agent_plist.is_some() {
        plan.steps.push(Step {
            description: "Remove carte's LaunchAgent".into(),
            action: Action::RemoveFile {
                path: env.agent_plist(),
            },
        });
    }
    if facts.resolver.as_deref() == Some(env.resolver_contents().as_str()) {
        plan.steps.push(Step::sudo(
            format!("Stop sending .{} lookups to carte", env.machine.tld),
            "rm",
            &["-f", &env.resolver_file().to_string_lossy()],
        ));
    }
    if matches!(facts.ca, CaState::Trusted | CaState::Unknown(_)) {
        plan.steps.push(Step::sudo(
            "Stop trusting carte's local CA",
            "security",
            &[
                "remove-trusted-cert",
                "-d",
                &env.ca_cert().to_string_lossy(),
            ],
        ));
    }

    let puma_dev = if env.puma_dev_plist().exists() {
        format!(
            "launchctl enable {service} && launchctl bootstrap {domain} {plist}",
            service = env.service(PUMA_DEV_LABEL),
            domain = env.domain(),
            plist = tilde(&env.puma_dev_plist()),
        )
    } else {
        "puma-dev -install".to_string()
    };
    plan.notes.push(format!(
        "to bring puma-dev back: `{puma_dev}`, and `sudo puma-dev -setup` for /etc/resolver/{tld}. \
         Your projects in {config}, carte's state in {state} and ~/.puma-dev are left alone.",
        tld = env.machine.tld,
        config = tilde(&env.paths.config_dir),
        state = tilde(&env.paths.state_dir),
    ));
    plan
}

/// Runs the steps in order and stops at the first that fails.
pub fn execute(steps: &[Step], env: &SetupEnv) -> anyhow::Result<()> {
    for (i, step) in steps.iter().enumerate() {
        println!("→ {}", step.description);
        let result = run_step(step, env);
        if let Err(err) = result {
            let left = steps.len() - i - 1;
            if left > 0 {
                bail!(
                    "step {} failed: {err:#}; the {left} after it didn't run",
                    i + 1
                );
            }
            bail!("step {} failed: {err:#}", i + 1);
        }
    }
    Ok(())
}

fn run_step(step: &Step, env: &SetupEnv) -> anyhow::Result<()> {
    match &step.action {
        Action::Run {
            command,
            sudo,
            stdin,
        } => {
            let mut process = if *sudo {
                let mut process = std::process::Command::new("sudo");
                process.arg(&command.program).args(&command.args);
                process
            } else {
                command.command()
            };
            if stdin.is_some() {
                process.stdin(Stdio::piped()).stdout(Stdio::null());
            }
            let mut child = process.spawn().with_context(|| command.to_string())?;
            if let (Some(contents), Some(mut pipe)) = (stdin, child.stdin.take()) {
                pipe.write_all(contents.as_bytes())?;
            }
            let status = child.wait()?;
            if !status.success() {
                bail!("`{command}` exited with {status}");
            }
        }
        Action::WriteFile { path, contents } => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
            }
            fs::write(path, contents).with_context(|| path.display().to_string())?;
        }
        Action::RemoveFile { path } => match fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(anyhow::Error::from(err).context(path.display().to_string()));
            }
            _ => {}
        },
        Action::CreateCa => {
            let ca = CertificateAuthority::load_or_create(
                &env.paths.state_dir,
                &crate::tls::ca::default_common_name(),
                &[&env.machine.tld],
            )?;
            crate::tls::bundle::rebuild(&env.paths.state_dir, &ca)?;
        }
        Action::Verify => {
            let report = check::verify(env);
            print!("{report}");
            if !report.ok() {
                bail!("carte isn't answering as it should");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &Path) -> SetupEnv {
        SetupEnv {
            paths: Paths::under(root),
            machine: MachineConfig::default(),
            home: root.join("home"),
            uid: 501,
            exe: "/opt/homebrew/bin/carte".into(),
            resolver_dir: root.join("resolver"),
            launch_agents_dir: root.join("home/Library/LaunchAgents"),
            puma_dev_dir: root.join("home/.puma-dev"),
        }
    }

    fn fresh() -> Facts {
        Facts {
            resolver: None,
            ca: CaState::Missing,
            agent_plist: None,
            agent_loaded: false,
            puma_dev: PumaDev::default(),
            projects: 0,
            daemon_running: false,
        }
    }

    fn commands(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| match &s.action {
                Action::Run { command, sudo, .. } => {
                    format!("{}{command}", if *sudo { "sudo " } else { "" })
                }
                Action::WriteFile { path, .. } => format!("write {}", path.display()),
                Action::RemoveFile { path } => format!("rm {}", path.display()),
                Action::CreateCa => "create ca".into(),
                Action::Verify => "verify".into(),
            })
            .collect()
    }

    #[test]
    fn plans_a_switch_from_puma_dev() {
        let root = Path::new("/r");
        let env = env(root);
        let facts = Facts {
            resolver: Some("# Generated by puma-dev\nnameserver 127.0.0.1\nport 9253\n".into()),
            puma_dev: PumaDev {
                plist: true,
                loaded: true,
                links: 3,
            },
            ..fresh()
        };
        let plan = plan_install(&env, &facts);
        assert_eq!(
            commands(&plan),
            [
                "launchctl disable gui/501/io.puma.dev",
                "launchctl bootout gui/501/io.puma.dev",
                "sudo tee /r/resolver/test",
                "create ca",
                "sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /r/state/ca.pem",
                "write /r/home/Library/LaunchAgents/carte.daemon.plist",
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/carte.daemon.plist",
                "verify",
            ]
        );
        assert!(plan.steps[2].needs_sudo());
        assert!(!plan.steps[5].needs_sudo());
        let Action::Run { stdin, .. } = &plan.steps[2].action else {
            panic!()
        };
        assert_eq!(stdin.as_deref(), Some("nameserver 127.0.0.1\nport 9253\n"));
        let Action::WriteFile { contents, .. } = &plan.steps[5].action else {
            panic!()
        };
        assert_eq!(*contents, env.agent().to_plist());
        assert!(contents.contains("<string>/r/state/carte.log</string>"));
        assert!(plan.notes.is_empty());

        let shown = plan.render(&env, false);
        assert!(shown.contains("3. Send .test lookups to carte's DNS server on port 9253\n   $ sudo tee /r/resolver/test\n     | nameserver 127.0.0.1\n     | port 9253\n"), "{shown}");
    }

    #[test]
    fn plans_only_what_is_missing() {
        let root = Path::new("/r");
        let env = env(root);
        let done = Facts {
            resolver: Some(env.resolver_contents()),
            ca: CaState::Trusted,
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..fresh()
        };
        let plan = plan_install(&env, &done);
        assert_eq!(commands(&plan), ["verify"]);
        assert!(!plan.changes_anything());

        let changed = Facts {
            agent_plist: Some("old".into()),
            ca: CaState::Untrusted,
            ..done.clone()
        };
        assert_eq!(
            commands(&plan_install(&env, &changed)),
            [
                "sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /r/state/ca.pem",
                "launchctl bootout gui/501/carte.daemon",
                "write /r/home/Library/LaunchAgents/carte.daemon.plist",
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/carte.daemon.plist",
                "verify",
            ]
        );

        let unloaded = Facts {
            agent_loaded: false,
            daemon_running: true,
            ..done
        };
        let plan = plan_install(&env, &unloaded);
        assert_eq!(
            commands(&plan),
            [
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/carte.daemon.plist",
                "verify",
            ]
        );
        assert!(
            plan.notes[0].contains("already running"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn warns_about_a_binary_in_a_build_directory() {
        let mut env = env(Path::new("/r"));
        env.exe = "/src/carte/target/debug/carte".into();
        let plan = plan_install(&env, &fresh());
        assert!(plan.notes.iter().any(|n| n.contains("build directory")));
        assert_eq!(
            commands(&plan)[..2],
            ["sudo mkdir -p /r/resolver", "sudo tee /r/resolver/test"]
        );
    }

    #[test]
    fn plans_the_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let facts = Facts {
            resolver: Some(env.resolver_contents()),
            ca: CaState::Trusted,
            agent_plist: Some("x".into()),
            agent_loaded: true,
            ..fresh()
        };
        let plan = plan_uninstall(&env, &facts);
        let root = dir.path().display();
        assert_eq!(
            commands(&plan),
            [
                "launchctl bootout gui/501/carte.daemon".to_string(),
                format!("rm {root}/home/Library/LaunchAgents/carte.daemon.plist"),
                format!("sudo rm -f {root}/resolver/test"),
                format!("sudo security remove-trusted-cert -d {root}/state/ca.pem"),
            ]
        );
        assert!(plan.notes[0].contains("puma-dev -install"));

        fs::create_dir_all(&env.launch_agents_dir).unwrap();
        fs::write(env.puma_dev_plist(), "").unwrap();
        let puma_dev_resolver = Facts {
            resolver: Some("# Generated by puma-dev\nnameserver 127.0.0.1\nport 9253\n".into()),
            ca: CaState::Untrusted,
            ..fresh()
        };
        let plan = plan_uninstall(&env, &puma_dev_resolver);
        assert!(plan.steps.is_empty());
        assert!(
            plan.notes[0]
                .contains("launchctl enable gui/501/io.puma.dev && launchctl bootstrap gui/501 "),
            "{}",
            plan.notes[0]
        );
    }

    #[test]
    fn executes_file_and_command_steps() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let written = dir.path().join("a/b/file");
        let piped = dir.path().join("piped");
        let gone = dir.path().join("gone");
        fs::write(&gone, "x").unwrap();
        let steps = vec![
            Step {
                description: "write".into(),
                action: Action::WriteFile {
                    path: written.clone(),
                    contents: "hello\n".into(),
                },
            },
            Step::run(
                "pipe",
                "sh",
                &["-c", &format!("cat > '{}'", piped.display())],
            )
            .with_stdin("from stdin\n".into()),
            Step {
                description: "remove".into(),
                action: Action::RemoveFile { path: gone.clone() },
            },
            Step {
                description: "remove again".into(),
                action: Action::RemoveFile { path: gone.clone() },
            },
        ];
        execute(&steps, &env).unwrap();
        assert_eq!(fs::read_to_string(&written).unwrap(), "hello\n");
        assert_eq!(fs::read_to_string(&piped).unwrap(), "from stdin\n");
        assert!(!gone.exists());

        let failing = vec![
            Step::run("fail", "false", &[]),
            Step {
                description: "never".into(),
                action: Action::WriteFile {
                    path: dir.path().join("never"),
                    contents: String::new(),
                },
            },
        ];
        let err = execute(&failing, &env).unwrap_err().to_string();
        assert!(err.contains("step 1 failed"), "{err}");
        assert!(err.contains("the 1 after it didn't run"), "{err}");
        assert!(!dir.path().join("never").exists());
    }

    #[test]
    fn creates_the_ca_in_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let steps = [Step {
            description: "ca".into(),
            action: Action::CreateCa,
        }];
        execute(&steps, &env).unwrap();
        assert!(env.ca_cert().exists());
        assert!(CertificateAuthority::load_existing(&env.paths.state_dir, &["test"]).is_ok());
    }
}
