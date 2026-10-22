//! `carte setup`: the one-time machine changes, planned as data and shown before they run.

pub mod check;
pub mod linux;
pub mod macos;
pub mod plist;
pub mod share;
pub mod skill;
pub mod trust_stores;
pub mod units;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use carte_core::config;
use carte_core::model::MachineConfig;

use crate::cli::prompt::tilde;
use crate::dns;
use crate::paths::Paths;
use crate::tls::trust::{self, CommandLine};
use crate::tls::{CertificateAuthority, CreateReason};
use linux::LinuxFacts;
use macos::MacFacts;
use skill::{SkillDir, SkillFacts, SkillLink};

/// The operating systems setup knows how to set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::MacOs
        }
    }
}

/// Where setup looks and writes; tests point every path at a temp dir.
#[derive(Debug, Clone)]
pub struct SetupEnv {
    pub platform: Platform,
    pub paths: Paths,
    pub machine: MachineConfig,
    pub home: PathBuf,
    pub uid: u32,
    pub exe: PathBuf,
    pub resolver_dir: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub puma_dev_dir: PathBuf,
    /// Where systemd looks for the user's units.
    pub systemd_user_dir: PathBuf,
    pub sysctl_dir: PathBuf,
    /// Whether `config.toml` sets `tld`, rather than leaving it to the default.
    pub tld_configured: bool,
}

impl SetupEnv {
    pub fn from_system(paths: &Paths) -> anyhow::Result<Self> {
        let machine = config::load_machine_config(&paths.machine_config())?;
        let tld_configured = fs::read_to_string(paths.machine_config())
            .ok()
            .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
            .is_some_and(|doc| doc.contains_key("tld"));
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let resolver_dir = dns::resolver_file_path("localhost")
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/etc/resolver"));
        let systemd_user_dir = linux::systemd_user_dir(
            &home,
            linux::manager_environment().as_deref(),
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        );
        Ok(Self {
            platform: Platform::current(),
            paths: paths.clone(),
            machine,
            // SAFETY: getuid has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            exe: std::env::current_exe().context("finding the carte executable")?,
            resolver_dir,
            launch_agents_dir: home.join("Library/LaunchAgents"),
            puma_dev_dir: home.join(".puma-dev"),
            systemd_user_dir,
            sysctl_dir: PathBuf::from("/etc/sysctl.d"),
            home,
            tld_configured,
        })
    }

    /// Refuses a TLD that isn't one before it names a resolver file. Setup checks the
    /// TLD it ends up with, so `--tld` can replace a bad one from `config.toml`.
    pub fn check_tld(&self) -> anyhow::Result<()> {
        if !is_tld(&self.machine.tld) {
            bail!(
                "{} sets tld = {:?}, which isn't a TLD; fix it there, or choose one with `carte setup --tld test`",
                tilde(&self.paths.machine_config()),
                self.machine.tld
            );
        }
        Ok(())
    }

    /// Whether the TLD needs a resolver file: `.localhost` resolves to loopback without one.
    pub fn needs_resolver(&self) -> bool {
        self.machine.tld != "localhost"
    }

    /// Whether a resolver file sends lookups to carte's DNS server.
    pub fn is_carte_resolver(&self, text: &str) -> bool {
        matches!(check::parse_resolver(text), (Some("127.0.0.1"), Some(port)) if port == self.machine.dns_port)
    }

    pub fn resolver_file(&self) -> PathBuf {
        self.resolver_dir.join(&self.machine.tld)
    }

    pub fn resolver_contents(&self) -> String {
        dns::resolver_file_contents(self.machine.dns_port)
    }

    /// The TLDs the CA can sign names under, as in ".localhost and .test".
    fn ca_names(&self) -> String {
        self.machine
            .served_tlds()
            .iter()
            .map(|tld| format!(".{tld}"))
            .collect::<Vec<_>>()
            .join(" and ")
    }

    pub fn ca_cert(&self) -> PathBuf {
        CertificateAuthority::cert_path_in(&self.paths.state_dir)
    }

    /// The CA's certificate, key, the trust bundle made from it and the record of the CAs
    /// created here.
    pub fn ca_files(&self) -> [PathBuf; 4] {
        let state = &self.paths.state_dir;
        [
            self.ca_cert(),
            state.join("ca-key.pem"),
            state.join("ca-bundle.pem"),
            CertificateAuthority::created_path_in(state),
        ]
    }

    pub fn skill_dir(&self) -> PathBuf {
        skill::dir_in(&self.home)
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }

    /// Claude Code's link to the skill.
    pub fn skill_link(&self) -> PathBuf {
        self.claude_dir().join("skills/carte")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaState {
    Missing,
    /// It exists but setup replaces it.
    Replace(String),
    /// `ca.pem` doesn't hold the certificate paired with the key, after a crash.
    Restore,
    Untrusted,
    Trusted,
    /// The trust check itself failed.
    Unknown(String),
}

/// What setup found on the machine, read without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The files in the resolver directory, by TLD.
    pub resolvers: BTreeMap<String, String>,
    pub ca: CaState,
    /// The CA's files that exist.
    pub ca_files: Vec<PathBuf>,
    /// The SHA-256 hash of the CA in the state dir, when it can be read.
    pub ca_hash: Option<String>,
    pub projects: usize,
    pub daemon_running: bool,
    /// The agent skill, unless setup leaves it alone.
    pub skill: Option<SkillFacts>,
    pub os: OsFacts,
}

/// What setup found that only one operating system has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsFacts {
    MacOs(MacFacts),
    Linux(LinuxFacts),
}

impl Facts {
    pub fn gather(env: &SetupEnv) -> Self {
        let ca = match CertificateAuthority::load_existing(
            &env.paths.state_dir,
            &env.machine.served_tlds(),
        ) {
            Err(CreateReason::Missing) => CaState::Missing,
            Err(CreateReason::CertFileStale) => CaState::Restore,
            Err(CreateReason::Invalid(why)) => CaState::Replace(why),
            Err(CreateReason::Expiring) => CaState::Replace("it is about to expire".into()),
            Err(CreateReason::NotTlsOnly) => {
                CaState::Replace("it can sign more than HTTPS certificates".into())
            }
            Err(CreateReason::TldsChanged { previous }) => {
                CaState::Replace(format!("it was made for .{}", previous.join(", .")))
            }
            Ok(ca) => match trust::is_trusted(&ca) {
                Ok(true) => CaState::Trusted,
                Ok(false) => CaState::Untrusted,
                Err(err) => CaState::Unknown(err.to_string()),
            },
        };
        let projects = carte_core::discovery::project_files(&env.paths.projects_dir())
            .map_or(0, |files| files.len());
        let resolvers = fs::read_dir(&env.resolver_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let text = fs::read_to_string(entry.path()).ok()?;
                Some((entry.file_name().to_string_lossy().into_owned(), text))
            })
            .collect();
        Facts {
            resolvers,
            ca,
            ca_files: env.ca_files().into_iter().filter(|f| f.exists()).collect(),
            ca_hash: crate::tls::ca::current_hash(&env.paths.state_dir),
            projects,
            daemon_running: UnixStream::connect(env.paths.socket()).is_ok(),
            skill: Some(SkillFacts::gather(
                &env.skill_dir(),
                &env.skill_link(),
                &env.claude_dir(),
            )),
            os: match env.platform {
                Platform::MacOs => OsFacts::MacOs(MacFacts::gather(env)),
                Platform::Linux => OsFacts::Linux(LinuxFacts::gather(env)),
            },
        }
    }

    pub fn resolver(&self, tld: &str) -> Option<&str> {
        self.resolvers.get(tld).map(String::as_str)
    }

    /// What setup found that only macOS has, on macOS.
    pub fn mac(&self) -> Option<&MacFacts> {
        match &self.os {
            OsFacts::MacOs(mac) => Some(mac),
            OsFacts::Linux(_) => None,
        }
    }
}

/// Whether `tld` is dot-separated labels of at most 63 letters, digits and inner
/// hyphens, safe to name a resolver file after.
pub fn is_tld(tld: &str) -> bool {
    !tld.is_empty()
        && tld.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Resolver files carte doesn't need: ones pointing at its DNS server, or written by
/// puma-dev, for any TLD but the configured one when that needs a resolver.
pub fn leftover_resolvers<'a>(env: &SetupEnv, facts: &'a Facts) -> Vec<&'a str> {
    facts
        .resolvers
        .iter()
        .filter(|(tld, text)| {
            (env.is_carte_resolver(text) || text.contains("puma-dev"))
                && !(env.needs_resolver() && **tld == env.machine.tld)
        })
        .map(|(tld, _)| tld.as_str())
        .collect()
}

/// TLDs other than `localhost` that puma-dev served or that resolver files send to
/// carte or puma-dev: the hostnames a machine without a configured `tld` used before.
pub fn previous_tlds(env: &SetupEnv, facts: &Facts) -> Vec<String> {
    let from_resolvers = facts
        .resolvers
        .iter()
        .filter(|(_, text)| env.is_carte_resolver(text) || text.contains("puma-dev"))
        .map(|(tld, _)| tld.clone());
    facts
        .mac()
        .into_iter()
        .flat_map(|mac| mac.puma_dev.domains.iter().cloned())
        .chain(from_resolvers)
        .filter(|tld| tld != "localhost" && is_tld(tld))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `config.toml` with `tld` set, everything else kept as written.
pub fn with_tld(text: &str, tld: &str) -> anyhow::Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("parsing config.toml")?;
    let had_values = doc.iter().any(|(_, item)| item.is_value());
    doc["tld"] = toml_edit::value(tld);
    if !had_values
        && let Some(table) = doc
            .as_table_mut()
            .iter_mut()
            .filter_map(|(_, item)| item.as_table_mut())
            .min_by_key(|table| table.position())
    {
        table.decor_mut().set_prefix("\n");
    }
    Ok(doc.to_string())
}

/// Writes `tld` to `config.toml` at the start of the plan when it differs, and restarts
/// a running daemon whose service the plan leaves alone, so it serves the new TLD.
pub fn set_tld(plan: &mut Plan, env: &SetupEnv, facts: &Facts) -> anyhow::Result<()> {
    let path = env.paths.machine_config();
    let current = fs::read_to_string(&path).unwrap_or_default();
    let contents = with_tld(&current, &env.machine.tld)?;
    if contents == current {
        return Ok(());
    }
    plan.steps.insert(
        0,
        Step {
            description: format!("Serve apps under .{} (config.toml)", env.machine.tld),
            action: Action::WriteFile { path, contents },
        },
    );
    restart_daemon(
        plan,
        env,
        facts,
        "Restart the daemon so it serves the new TLD",
    );
    Ok(())
}

/// Restarts a running daemon before the plan verifies, unless the plan already starts
/// or restarts it.
fn restart_daemon(plan: &mut Plan, env: &SetupEnv, facts: &Facts, description: &str) {
    let loaded = match &facts.os {
        OsFacts::MacOs(mac) => mac.agent_loaded,
        OsFacts::Linux(linux) => linux.unit(units::SERVICE).active(),
    };
    let starts = match &facts.os {
        OsFacts::MacOs(_) => plan.steps.iter().any(macos::starts_daemon),
        OsFacts::Linux(linux) => linux::starts_daemon(&plan.steps, linux),
    };
    if !loaded || starts {
        return;
    }
    let verify = plan
        .steps
        .iter()
        .position(|s| s.action == Action::Verify)
        .unwrap_or(plan.steps.len());
    let restart = match &facts.os {
        OsFacts::MacOs(_) => macos::restart_step(env, description),
        OsFacts::Linux(_) => linux::restart_step(description),
    };
    plan.steps.insert(verify, restart);
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
    /// Runs the command until it fails, or fails itself after `timeout`.
    WaitUntilFails {
        command: CommandLine,
        timeout: Duration,
    },
    /// Writes carte's skill and its marker into the directory.
    InstallSkill {
        dir: PathBuf,
    },
    Symlink {
        link: PathBuf,
        target: PathBuf,
    },
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
            Action::WaitUntilFails { command, timeout } => {
                let _ = writeln!(
                    out,
                    "   wait until `{command}` fails, up to {}s",
                    timeout.as_secs()
                );
            }
            Action::InstallSkill { dir } => {
                let _ = writeln!(out, "   write {}", tilde(&dir.join("SKILL.md")));
            }
            Action::Symlink { link, target } => {
                let _ = writeln!(out, "   $ ln -s {} {}", tilde(target), tilde(link));
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
    match &facts.os {
        OsFacts::MacOs(mac) => macos::plan_install(env, facts, mac),
        OsFacts::Linux(linux) => linux::plan_install(env, facts, linux),
    }
}

pub fn plan_uninstall(env: &SetupEnv, facts: &Facts) -> Plan {
    match &facts.os {
        OsFacts::MacOs(mac) => macos::plan_uninstall(env, facts, mac),
        OsFacts::Linux(linux) => linux::plan_uninstall(env, facts, linux),
    }
}

/// Creates the CA, or restores `ca.pem`, when the state dir has no usable one.
fn create_ca_step(facts: &Facts) -> Option<Step> {
    let description = match &facts.ca {
        CaState::Missing => "Create carte's local CA".to_string(),
        CaState::Replace(why) => format!("Create carte's local CA, replacing the old one: {why}"),
        CaState::Restore => "Restore ca.pem from the certificate kept with its key".to_string(),
        CaState::Untrusted | CaState::Trusted | CaState::Unknown(_) => return None,
    };
    Some(Step {
        description,
        action: Action::CreateCa,
    })
}

/// The end of every install plan: the skill, the check that carte answers, and a restart
/// of a running daemon that a new CA would otherwise pass by.
fn finish_install(plan: &mut Plan, env: &SetupEnv, facts: &Facts) {
    if let Some(skill) = &facts.skill {
        skill_steps(env, skill, plan);
    }
    plan.steps.push(Step {
        description: "Check that carte answers".into(),
        action: Action::Verify,
    });
    if matches!(facts.ca, CaState::Missing | CaState::Replace(_)) {
        restart_daemon(plan, env, facts, "Restart the daemon so it uses the new CA");
    }
}

/// Installs or refreshes the agent skill and links it for Claude Code.
fn skill_steps(env: &SetupEnv, skill: &SkillFacts, plan: &mut Plan) {
    let dir = env.skill_dir();
    match skill.dir {
        SkillDir::Unmanaged => {
            plan.notes.push(format!(
                "{} wasn't written by carte, so setup leaves it alone; remove it, or add a {} file to it, to let carte manage it",
                tilde(&dir),
                skill::MARKER
            ));
            return;
        }
        SkillDir::Managed { current: true } => {}
        SkillDir::Missing | SkillDir::Managed { current: false } => plan.steps.push(Step {
            description: "Install carte's skill for coding agents, so they use carte instead of starting servers".into(),
            action: Action::InstallSkill { dir: dir.clone() },
        }),
    }
    match skill.link {
        SkillLink::Missing => plan.steps.push(Step {
            description: "Let Claude Code find the skill".into(),
            action: Action::Symlink {
                link: env.skill_link(),
                target: dir,
            },
        }),
        SkillLink::Other => plan.notes.push(format!(
            "{} isn't carte's, so setup leaves it alone; Claude Code won't see carte's skill until it's gone",
            tilde(&env.skill_link())
        )),
        SkillLink::NoClaude | SkillLink::Ours => {}
    }
}

/// Removes the agent skill and Claude Code's link to it, when they're carte's.
fn skill_uninstall_steps(env: &SetupEnv, facts: &Facts, plan: &mut Plan) {
    let Some(skill) = &facts.skill else {
        return;
    };
    if skill.link == SkillLink::Ours {
        plan.steps.push(Step {
            description: "Remove Claude Code's link to carte's skill".into(),
            action: Action::RemoveFile {
                path: env.skill_link(),
            },
        });
    }
    if matches!(skill.dir, SkillDir::Managed { .. }) {
        plan.steps.push(Step::run(
            "Remove carte's skill for coding agents",
            "rm",
            &["-rf", &env.skill_dir().to_string_lossy()],
        ));
    }
}

/// Deletes the CA's files that exist.
fn ca_files_uninstall_step(facts: &Facts, plan: &mut Plan) {
    if facts.ca_files.is_empty() {
        return;
    }
    let mut args = vec!["-f".to_string()];
    args.extend(
        facts
            .ca_files
            .iter()
            .map(|f| f.to_string_lossy().into_owned()),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    plan.steps.push(Step::run(
        "Delete carte's local CA and its key",
        "rm",
        &args,
    ));
}

/// The command with the hash of the CA in the state dir in place of
/// [`trust_stores::NEW_CA_HASH`], for a step planned before the CA existed.
fn with_new_ca_hash(command: &CommandLine, env: &SetupEnv) -> anyhow::Result<CommandLine> {
    if !command
        .args
        .iter()
        .any(|a| a.contains(trust_stores::NEW_CA_HASH))
    {
        return Ok(command.clone());
    }
    let hash = crate::tls::ca::current_hash(&env.paths.state_dir)
        .context("reading the hash of carte's new CA")?;
    let short = trust_stores::entry_name(&hash);
    let short = short.trim_start_matches("carte-");
    Ok(CommandLine {
        program: command.program.clone(),
        args: command
            .args
            .iter()
            .map(|a| a.replace(trust_stores::NEW_CA_HASH, short))
            .collect(),
    })
}

/// The program sudo runs: on Linux the system's, by absolute path, so it doesn't
/// depend on sudo's `secure_path`.
fn sudo_program(program: &str, env: &SetupEnv) -> PathBuf {
    match env.platform {
        Platform::Linux if !program.contains('/') => {
            trust_stores::find_program(program).unwrap_or_else(|| PathBuf::from(program))
        }
        _ => PathBuf::from(program),
    }
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
            let command = &with_new_ca_hash(command, env)?;
            let mut process = if *sudo {
                let mut process = std::process::Command::new("sudo");
                process
                    .arg(sudo_program(&command.program, env))
                    .args(&command.args);
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
            let ca = CertificateAuthority::load_exact_or_create(
                &env.paths.state_dir,
                &crate::tls::ca::default_common_name(),
                &env.machine.served_tlds(),
            )?;
            crate::tls::bundle::rebuild(&env.paths.state_dir, &ca)?;
        }
        Action::WaitUntilFails { command, timeout } => {
            let start = Instant::now();
            while command
                .command()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| command.to_string())?
                .success()
            {
                if start.elapsed() >= *timeout {
                    bail!("`{command}` still succeeds after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        Action::InstallSkill { dir } => {
            skill::install(dir).with_context(|| dir.display().to_string())?;
        }
        Action::Symlink { link, target } => {
            if let Some(dir) = link.parent() {
                fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
            }
            std::os::unix::fs::symlink(target, link).with_context(|| link.display().to_string())?;
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
pub(super) mod tests {
    use super::*;

    pub(in crate::setup) fn env(root: &Path) -> SetupEnv {
        SetupEnv {
            platform: Platform::MacOs,
            paths: Paths::under(root),
            machine: MachineConfig::default(),
            home: root.join("home"),
            uid: 501,
            exe: "/opt/homebrew/bin/carte".into(),
            resolver_dir: root.join("resolver"),
            launch_agents_dir: root.join("home/Library/LaunchAgents"),
            puma_dev_dir: root.join("home/.puma-dev"),
            systemd_user_dir: root.join("home/.config/systemd/user"),
            sysctl_dir: root.join("sysctl.d"),
            tld_configured: false,
        }
    }

    pub(in crate::setup) fn with_test_tld(mut env: SetupEnv) -> SetupEnv {
        env.machine.tld = "test".into();
        env
    }

    pub(in crate::setup) fn resolvers(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|(tld, text)| (tld.to_string(), text.to_string()))
            .collect()
    }

    pub(in crate::setup) fn fresh() -> Facts {
        Facts {
            resolvers: BTreeMap::new(),
            ca: CaState::Missing,
            ca_files: Vec::new(),
            ca_hash: None,
            projects: 0,
            daemon_running: false,
            skill: None,
            os: OsFacts::MacOs(MacFacts::default()),
        }
    }

    pub(in crate::setup) fn commands(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| match &s.action {
                Action::Run { command, sudo, .. } => {
                    format!("{}{command}", if *sudo { "sudo " } else { "" })
                }
                Action::WriteFile { path, .. } => format!("write {}", path.display()),
                Action::RemoveFile { path } => format!("rm {}", path.display()),
                Action::CreateCa => "create ca".into(),
                Action::WaitUntilFails { command, .. } => format!("wait until {command} fails"),
                Action::InstallSkill { dir } => format!("install skill {}", dir.display()),
                Action::Symlink { link, target } => {
                    format!("ln -s {} {}", target.display(), link.display())
                }
                Action::Verify => "verify".into(),
            })
            .collect()
    }

    #[test]
    fn tells_tlds_from_paths() {
        for tld in ["test", "localhost", "nova.test", "x-1"] {
            assert!(is_tld(tld), "{tld}");
        }
        for tld in [
            "", "../x", "/etc/x", ".test", "test.", "a/b", "-x", "x-", "a..b", "a b",
        ] {
            assert!(!is_tld(tld), "{tld}");
        }
        assert!(is_tld(&format!("{}.test", "t".repeat(63))));
        assert!(!is_tld(&format!("{}.test", "t".repeat(64))));
    }

    #[test]
    fn refuses_a_configured_tld_that_isnt_one() {
        for tld in ["../x", "/etc/x", ".test"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            fs::create_dir_all(&paths.config_dir).unwrap();
            fs::write(paths.machine_config(), format!("tld = {tld:?}\n")).unwrap();
            let mut env = SetupEnv::from_system(&paths).unwrap();
            assert_eq!(env.resolver_dir, Path::new("/etc/resolver"));
            let err = env.check_tld().unwrap_err().to_string();
            assert!(err.contains("isn't a TLD"), "{tld}: {err}");
            env.machine.tld = "test".into();
            env.check_tld().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(paths.machine_config(), "tld = \"test\"\n").unwrap();
        let env = SetupEnv::from_system(&paths).unwrap();
        env.check_tld().unwrap();
        assert_eq!(env.resolver_file(), Path::new("/etc/resolver/test"));
    }

    #[test]
    fn waits_until_a_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let flag = dir.path().join("loaded");
        fs::write(&flag, "").unwrap();
        let wait = |timeout| Step {
            description: "wait".into(),
            action: Action::WaitUntilFails {
                command: CommandLine::new("test", &["-e", &flag.to_string_lossy()]),
                timeout,
            },
        };
        let err = execute(&[wait(Duration::ZERO)], &env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("still succeeds after 0s"), "{err}");

        let remover = std::thread::spawn({
            let flag = flag.clone();
            move || {
                std::thread::sleep(Duration::from_millis(300));
                fs::remove_file(flag).unwrap();
            }
        });
        let start = Instant::now();
        execute(&[wait(Duration::from_secs(30))], &env).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(250));
        remover.join().unwrap();
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
    fn writes_the_skill_and_its_link() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        fs::create_dir_all(env.claude_dir()).unwrap();
        let facts = SkillFacts::gather(&env.skill_dir(), &env.skill_link(), &env.claude_dir());
        let mut plan = Plan::default();
        skill_steps(&env, &facts, &mut plan);
        execute(&plan.steps, &env).unwrap();
        assert_eq!(
            fs::read_to_string(env.skill_link().join("SKILL.md")).unwrap(),
            skill::SKILL
        );
        assert_eq!(
            SkillFacts::gather(&env.skill_dir(), &env.skill_link(), &env.claude_dir()),
            SkillFacts {
                dir: SkillDir::Managed { current: true },
                link: SkillLink::Ours,
            }
        );
    }

    #[test]
    fn runs_sudo_programs_by_absolute_path_on_linux() {
        let mac = env(Path::new("/r"));
        assert_eq!(sudo_program("rm", &mac), Path::new("rm"));
        let linux = SetupEnv {
            platform: Platform::Linux,
            ..mac
        };
        let rm = sudo_program("rm", &linux);
        assert!(rm.is_absolute() && rm.ends_with("rm"), "{}", rm.display());
        assert_eq!(
            sudo_program("/usr/sbin/update-ca-certificates", &linux),
            Path::new("/usr/sbin/update-ca-certificates")
        );
        assert_eq!(
            sudo_program("no-such-carte-program", &linux),
            Path::new("no-such-carte-program")
        );
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
        assert!(
            CertificateAuthority::load_existing(&env.paths.state_dir, &env.machine.served_tlds())
                .is_ok()
        );
    }
}
