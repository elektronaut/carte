//! `carte setup`: the one-time machine changes, planned as data and shown before they run.

pub mod check;
pub mod plist;
pub mod share;
pub mod skill;

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
use crate::tls::trust::{self, CommandLine, KeychainCa};
use crate::tls::{CertificateAuthority, CreateReason};
use plist::{Agent, LABEL, OLD_LABEL};
use skill::{SkillDir, SkillFacts, SkillLink};

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

    pub fn agent_plist(&self) -> PathBuf {
        self.launch_agents_dir.join(format!("{LABEL}.plist"))
    }

    pub fn old_agent_plist(&self) -> PathBuf {
        self.launch_agents_dir.join(format!("{OLD_LABEL}.plist"))
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
    /// It exists but setup replaces it.
    Replace(String),
    /// `ca.pem` doesn't hold the certificate paired with the key, after a crash.
    Restore,
    Untrusted,
    Trusted,
    /// The trust check itself failed.
    Unknown(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PumaDev {
    pub plist: bool,
    pub loaded: bool,
    /// Whether the program its LaunchAgent runs, or `puma-dev` on `PATH`, exists.
    pub binary: bool,
    /// Entries in `~/.puma-dev`.
    pub links: usize,
    /// The domains its LaunchAgent serves, from `-d`.
    pub domains: Vec<String>,
}

impl PumaDev {
    /// A plist left behind after puma-dev itself was removed doesn't count.
    pub fn installed(&self) -> bool {
        self.loaded || self.binary
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OldAgent {
    pub plist: bool,
    pub loaded: bool,
}

impl OldAgent {
    pub fn installed(&self) -> bool {
        self.plist || self.loaded
    }
}

/// What setup found on the machine, read without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The files in the resolver directory, by TLD.
    pub resolvers: BTreeMap<String, String>,
    pub ca: CaState,
    /// The CA's files that exist.
    pub ca_files: Vec<PathBuf>,
    /// The CAs in the system keychain that this install created.
    pub keychain_cas: Vec<KeychainCa>,
    /// Other carte CAs in the system keychain under this install's name, which may be
    /// another install's.
    pub unknown_keychain_cas: Vec<KeychainCa>,
    /// The SHA-256 hash of the CA in the state dir, when it can be read.
    pub ca_hash: Option<String>,
    pub agent_plist: Option<String>,
    pub agent_loaded: bool,
    /// The agent under its old label.
    pub old_agent: OldAgent,
    pub puma_dev: PumaDev,
    pub projects: usize,
    pub daemon_running: bool,
    /// The agent skill, unless setup leaves it alone.
    pub skill: Option<SkillFacts>,
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
        let (keychain_cas, unknown_keychain_cas) = trust::own_cas(
            trust::keychain_cas(),
            &CertificateAuthority::created_hashes(&env.paths.state_dir),
            &crate::tls::ca::default_common_name(),
        );
        Facts {
            resolvers,
            ca,
            ca_files: env.ca_files().into_iter().filter(|f| f.exists()).collect(),
            keychain_cas,
            unknown_keychain_cas,
            ca_hash: crate::tls::ca::current_hash(&env.paths.state_dir),
            agent_plist: fs::read_to_string(env.agent_plist()).ok(),
            agent_loaded: check::launchd_loaded(&env.service(LABEL)),
            old_agent: OldAgent {
                plist: env.old_agent_plist().exists(),
                loaded: check::launchd_loaded(&env.service(OLD_LABEL)),
            },
            puma_dev: PumaDev {
                plist: env.puma_dev_plist().exists(),
                loaded: check::launchd_loaded(&env.service(PUMA_DEV_LABEL)),
                binary: fs::read_to_string(env.puma_dev_plist())
                    .ok()
                    .and_then(|text| launch_agent_program(&text))
                    .is_some_and(|program| program.is_file())
                    || std::env::var_os("PATH").is_some_and(|path| {
                        std::env::split_paths(&path).any(|dir| dir.join("puma-dev").is_file())
                    }),
                links: fs::read_dir(&env.puma_dev_dir)
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or(0),
                domains: fs::read_to_string(env.puma_dev_plist())
                    .map(|text| puma_dev_domains(&text))
                    .unwrap_or_default(),
            },
            projects,
            daemon_running: UnixStream::connect(env.paths.socket()).is_ok(),
            skill: Some(SkillFacts::gather(
                &env.skill_dir(),
                &env.skill_link(),
                &env.claude_dir(),
            )),
        }
    }
}

impl Facts {
    pub fn resolver(&self, tld: &str) -> Option<&str> {
        self.resolvers.get(tld).map(String::as_str)
    }
}

/// The domains in puma-dev's LaunchAgent: its `-d` argument split on `:`, or
/// puma-dev's default, `test`.
pub fn puma_dev_domains(plist: &str) -> Vec<String> {
    let strings: Vec<&str> = plist
        .split("<string>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</string>").map(|(value, _)| value.trim()))
        .collect();
    let value = strings
        .iter()
        .position(|s| *s == "-d")
        .and_then(|i| strings.get(i + 1).copied())
        .or_else(|| strings.iter().find_map(|s| s.strip_prefix("-d=")))
        .unwrap_or("test");
    value
        .split(':')
        .map(str::trim)
        .filter(|d| is_tld(d))
        .map(str::to_string)
        .collect()
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

/// The program a LaunchAgent runs: the first of its `ProgramArguments`.
pub fn launch_agent_program(plist: &str) -> Option<PathBuf> {
    let (_, rest) = plist.split_once("<key>ProgramArguments</key>")?;
    let (_, rest) = rest.split_once("<string>")?;
    let (program, _) = rest.split_once("</string>")?;
    Some(PathBuf::from(program.trim()))
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
        .puma_dev
        .domains
        .iter()
        .cloned()
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
/// a running daemon whose LaunchAgent the plan leaves alone, so it serves the new TLD.
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
    let starts = plan.steps.iter().any(|s| {
        matches!(&s.action, Action::Run { command, .. }
            if command.program == "launchctl"
                && command.args.first().is_some_and(|a| a == "bootstrap" || a == "kickstart"))
    });
    if !facts.agent_loaded || starts {
        return;
    }
    let verify = plan
        .steps
        .iter()
        .position(|s| s.action == Action::Verify)
        .unwrap_or(plan.steps.len());
    plan.steps.insert(
        verify,
        Step::run(
            description,
            "launchctl",
            &["kickstart", "-k", &env.service(LABEL)],
        ),
    );
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

    /// Stops a launchd service and waits until launchd no longer has it loaded.
    fn bootout(description: impl Into<String>, env: &SetupEnv, label: &str) -> [Self; 2] {
        let service = env.service(label);
        [
            Self::run(description, "launchctl", &["bootout", &service]),
            Step {
                description: format!("Wait for launchd to finish stopping {label}"),
                action: Action::WaitUntilFails {
                    command: CommandLine::new("launchctl", &["print", &service]),
                    timeout: Duration::from_secs(60),
                },
            },
        ]
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
    let mut plan = Plan::default();
    let steps = &mut plan.steps;
    let puma_dev = env.service(PUMA_DEV_LABEL);
    if facts.puma_dev.plist && facts.puma_dev.installed() {
        steps.push(Step::run(
            "Keep puma-dev's LaunchAgent from starting at login (its plist and ~/.puma-dev stay)",
            "launchctl",
            &["disable", &puma_dev],
        ));
    }
    if facts.puma_dev.loaded {
        steps.extend(Step::bootout(
            "Stop puma-dev so carte can take over its ports",
            env,
            PUMA_DEV_LABEL,
        ));
    }

    old_agent_steps(env, facts, steps);

    for tld in leftover_resolvers(env, facts) {
        let why = if tld == "localhost" {
            ".localhost resolves without it".to_string()
        } else {
            format!("carte serves .{}, not .{tld}", env.machine.tld)
        };
        steps.push(Step::sudo(
            format!("Remove /etc/resolver/{tld}: {why}"),
            "rm",
            &["-f", &env.resolver_dir.join(tld).to_string_lossy()],
        ));
    }

    let resolver = env.resolver_contents();
    if env.needs_resolver() && facts.resolver(&env.machine.tld) != Some(resolver.as_str()) {
        let file = env.resolver_file();
        if facts.resolvers.is_empty() {
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
    let names = env
        .machine
        .served_tlds()
        .iter()
        .map(|tld| format!(".{tld}"))
        .collect::<Vec<_>>()
        .join(" and ");
    let trust_step = || {
        let command = trust::trust_command(&ca_cert);
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        Step::sudo(
            format!(
                "Trust carte's local CA in the system keychain (it can only sign {names} names)"
            ),
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
        CaState::Restore => {
            steps.push(Step {
                description: "Restore ca.pem from the certificate kept with its key".into(),
                action: Action::CreateCa,
            });
            steps.push(trust_step());
        }
        CaState::Untrusted | CaState::Unknown(_) => steps.push(trust_step()),
        CaState::Trusted => {}
    }
    if facts.ca != CaState::Trusted {
        let kept = match facts.ca {
            CaState::Restore | CaState::Untrusted | CaState::Unknown(_) => facts.ca_hash.as_deref(),
            _ => None,
        };
        for ca in facts
            .keychain_cas
            .iter()
            .filter(|ca| Some(ca.hash.as_str()) != kept)
        {
            steps.push(Step::sudo(
                format!("Remove the old {} from the system keychain", ca.name()),
                "security",
                &["delete-certificate", "-Z", &ca.hash, trust::SYSTEM_KEYCHAIN],
            ));
        }
    }

    let plist = env.agent().to_plist();
    let plist_path = env.agent_plist();
    let plist_changed = facts.agent_plist.as_deref() != Some(plist.as_str());
    if plist_changed {
        if facts.agent_loaded {
            steps.extend(Step::bootout(
                "Stop the running carte LaunchAgent to replace it",
                env,
                LABEL,
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
    if let Some(skill) = &facts.skill {
        skill_steps(env, skill, &mut plan);
    }
    plan.steps.push(Step {
        description: "Check that carte answers".into(),
        action: Action::Verify,
    });
    if matches!(facts.ca, CaState::Missing | CaState::Replace(_)) {
        restart_daemon(
            &mut plan,
            env,
            facts,
            "Restart the daemon so it uses the new CA",
        );
    }

    if facts.daemon_running && !facts.agent_loaded && !facts.old_agent.loaded {
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

/// Takes down an agent installed under the old label.
fn old_agent_steps(env: &SetupEnv, facts: &Facts, steps: &mut Vec<Step>) {
    if facts.old_agent.loaded {
        steps.extend(Step::bootout(
            format!("Stop carte's LaunchAgent under its old label, {OLD_LABEL}"),
            env,
            OLD_LABEL,
        ));
    }
    if facts.old_agent.plist {
        steps.push(Step {
            description: format!("Remove carte's LaunchAgent under its old label, {OLD_LABEL}"),
            action: Action::RemoveFile {
                path: env.old_agent_plist(),
            },
        });
    }
}

pub fn plan_uninstall(env: &SetupEnv, facts: &Facts) -> Plan {
    let mut plan = Plan::default();
    old_agent_steps(env, facts, &mut plan.steps);
    if facts.agent_loaded {
        plan.steps.extend(Step::bootout(
            "Stop the carte daemon and its apps",
            env,
            LABEL,
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
    for (tld, text) in &facts.resolvers {
        if *text == env.resolver_contents() {
            plan.steps.push(Step::sudo(
                format!("Stop sending .{tld} lookups to carte"),
                "rm",
                &["-f", &env.resolver_dir.join(tld).to_string_lossy()],
            ));
        }
    }
    if let Some(skill) = &facts.skill {
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

    for ca in &facts.keychain_cas {
        plan.steps.push(Step::sudo(
            format!("Remove {} from the system keychain", ca.name()),
            "security",
            &["delete-certificate", "-Z", &ca.hash, trust::SYSTEM_KEYCHAIN],
        ));
    }
    if !facts.ca_files.is_empty() {
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

    let puma_dev = if facts.puma_dev.plist && facts.puma_dev.binary {
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
        "to bring puma-dev back: `{puma_dev}`, and `sudo puma-dev -setup` for its resolver files. \
         Your projects in {config}, the rest of carte's state in {state} and ~/.puma-dev are left alone.",
        config = tilde(&env.paths.config_dir),
        state = tilde(&env.paths.state_dir),
    ));
    for ca in &facts.unknown_keychain_cas {
        plan.notes.push(format!(
            "{name} is left in the system keychain; it isn't known to be this install's. \
             If it is, remove it with `sudo security delete-certificate -Z {hash} {keychain}`.",
            name = ca.name(),
            hash = ca.hash,
            keychain = trust::SYSTEM_KEYCHAIN,
        ));
    }
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
            tld_configured: false,
        }
    }

    fn with_test_tld(mut env: SetupEnv) -> SetupEnv {
        env.machine.tld = "test".into();
        env
    }

    const PUMA_DEV_RESOLVER: &str = "# Generated by puma-dev\nnameserver 127.0.0.1\nport 9253\n";

    fn resolvers(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|(tld, text)| (tld.to_string(), text.to_string()))
            .collect()
    }

    fn fresh() -> Facts {
        Facts {
            resolvers: BTreeMap::new(),
            ca: CaState::Missing,
            ca_files: Vec::new(),
            keychain_cas: Vec::new(),
            unknown_keychain_cas: Vec::new(),
            ca_hash: None,
            agent_plist: None,
            agent_loaded: false,
            old_agent: OldAgent::default(),
            puma_dev: PumaDev::default(),
            projects: 0,
            daemon_running: false,
            skill: None,
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
    fn plans_a_switch_from_puma_dev() {
        let root = Path::new("/r");
        let env = with_test_tld(env(root));
        let facts = Facts {
            resolvers: resolvers(&[("test", PUMA_DEV_RESOLVER)]),
            puma_dev: PumaDev {
                plist: true,
                loaded: true,
                binary: true,
                links: 3,
                domains: vec!["test".into()],
            },
            ..fresh()
        };
        let plan = plan_install(&env, &facts);
        assert_eq!(
            commands(&plan),
            [
                "launchctl disable gui/501/io.puma.dev",
                "launchctl bootout gui/501/io.puma.dev",
                "wait until launchctl print gui/501/io.puma.dev fails",
                "sudo tee /r/resolver/test",
                "create ca",
                "sudo security add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain /r/state/ca.pem",
                "write /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
                "verify",
            ]
        );
        assert!(plan.steps[3].needs_sudo());
        assert!(!plan.steps[6].needs_sudo());
        let Action::Run { stdin, .. } = &plan.steps[3].action else {
            panic!()
        };
        assert_eq!(stdin.as_deref(), Some("nameserver 127.0.0.1\nport 9253\n"));
        let Action::WriteFile { contents, .. } = &plan.steps[6].action else {
            panic!()
        };
        assert_eq!(*contents, env.agent().to_plist());
        assert!(contents.contains("<string>/r/state/carte.log</string>"));
        assert!(plan.notes.is_empty());

        let shown = plan.render(&env, false);
        assert!(shown.contains("2. Stop puma-dev so carte can take over its ports\n   $ launchctl bootout gui/501/io.puma.dev\n3. Wait for launchd to finish stopping io.puma.dev\n   wait until `launchctl print gui/501/io.puma.dev` fails, up to 60s\n"), "{shown}");
        assert!(shown.contains("4. Send .test lookups to carte's DNS server on port 9253\n   $ sudo tee /r/resolver/test\n     | nameserver 127.0.0.1\n     | port 9253\n"), "{shown}");
    }

    #[test]
    fn switches_from_puma_dev_to_localhost_without_a_resolver() {
        let root = Path::new("/r");
        let env = env(root);
        let facts = Facts {
            resolvers: resolvers(&[
                ("localhost", PUMA_DEV_RESOLVER),
                ("test", PUMA_DEV_RESOLVER),
                ("vpn", "nameserver 10.0.0.1\n"),
            ]),
            puma_dev: PumaDev {
                plist: true,
                loaded: true,
                binary: true,
                links: 3,
                domains: vec!["test".into(), "localhost".into()],
            },
            ..fresh()
        };
        let plan = plan_install(&env, &facts);
        assert_eq!(
            commands(&plan)[..5],
            [
                "launchctl disable gui/501/io.puma.dev",
                "launchctl bootout gui/501/io.puma.dev",
                "wait until launchctl print gui/501/io.puma.dev fails",
                "sudo rm -f /r/resolver/localhost",
                "sudo rm -f /r/resolver/test",
            ]
        );
        assert_eq!(commands(&plan)[5], "create ca");
        assert_eq!(
            plan.steps[4].description,
            "Remove /etc/resolver/test: carte serves .localhost, not .test"
        );
        assert_eq!(previous_tlds(&env, &facts), ["test"]);
    }

    #[test]
    fn reads_puma_devs_domains() {
        let plist = "<array>\n<string>/opt/homebrew/bin/puma-dev</string>\n<string>-launchd</string>\n\
                     <string>-d</string>\n<string>test:localhost</string>\n</array>";
        assert_eq!(puma_dev_domains(plist), ["test", "localhost"]);
        assert_eq!(puma_dev_domains("<string>-d=dev</string>"), ["dev"]);
        assert_eq!(puma_dev_domains("<string>puma-dev</string>"), ["test"]);
        assert_eq!(
            puma_dev_domains("<string>-d=../hosts:/etc/x:.test:dev</string>"),
            ["dev"]
        );
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
    fn previous_tlds_come_from_carte_and_puma_dev_resolvers() {
        let env = env(Path::new("/r"));
        let facts = Facts {
            resolvers: resolvers(&[
                ("test", "nameserver 127.0.0.1\nport 9253\n"),
                ("dev", "nameserver 127.0.0.1\nport 9999\n"),
                ("localhost", PUMA_DEV_RESOLVER),
                (".test", "nameserver 127.0.0.1\nport 9253\n"),
                ("-x", PUMA_DEV_RESOLVER),
            ]),
            ..fresh()
        };
        assert_eq!(previous_tlds(&env, &facts), ["test"]);
        assert_eq!(
            leftover_resolvers(&env, &facts),
            ["-x", ".test", "localhost", "test"]
        );
        let test = with_test_tld(env);
        assert_eq!(
            leftover_resolvers(&test, &facts),
            ["-x", ".test", "localhost"]
        );
    }

    #[test]
    fn saves_the_tld_and_restarts_a_running_daemon() {
        assert_eq!(
            with_tld(
                "# mine\nidle_timeout = \"10m\"\n\n[share]\nprovider = \"none\"\n",
                "test"
            )
            .unwrap(),
            "# mine\nidle_timeout = \"10m\"\ntld = \"test\"\n\n[share]\nprovider = \"none\"\n"
        );
        assert_eq!(
            with_tld("[share]\nprovider = \"none\"\n", "localhost").unwrap(),
            "tld = \"localhost\"\n\n[share]\nprovider = \"none\"\n"
        );
        let dir = tempfile::tempdir().unwrap();
        let env = with_test_tld(env(dir.path()));
        let facts = Facts {
            resolvers: resolvers(&[("test", "nameserver 127.0.0.1\nport 9253\n")]),
            ca: CaState::Trusted,
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..fresh()
        };
        let mut plan = plan_install(&env, &facts);
        set_tld(&mut plan, &env, &facts).unwrap();
        let root = dir.path().display();
        assert_eq!(
            commands(&plan),
            [
                format!("write {root}/config/config.toml"),
                "launchctl kickstart -k gui/501/no.elektronaut.carte".to_string(),
                "verify".to_string(),
            ]
        );
    }

    #[test]
    fn restores_ca_pem_and_keeps_the_ca_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let facts = Facts {
            ca: CaState::Restore,
            ca_hash: Some("C544".into()),
            keychain_cas: vec![
                KeychainCa {
                    hash: "1D2D".into(),
                    label: "carte local CA (alice@mac)".into(),
                },
                KeychainCa {
                    hash: "C544".into(),
                    label: "carte local CA (alice@mac)".into(),
                },
            ],
            ..fresh()
        };
        let planned = commands(&plan_install(&env, &facts));
        let root = dir.path().display();
        assert_eq!(
            planned[..3],
            [
                "create ca".to_string(),
                format!(
                    "sudo security add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain {root}/state/ca.pem"
                ),
                "sudo security delete-certificate -Z 1D2D /Library/Keychains/System.keychain"
                    .to_string(),
            ]
        );
        assert!(!planned.iter().any(|c| c.contains("-Z C544")));
    }

    #[test]
    fn removes_the_cas_a_new_one_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let old = KeychainCa {
            hash: "1D2D".into(),
            label: "carte local CA (alice@mac)".into(),
        };
        let facts = Facts {
            ca: CaState::Replace("it is about to expire".into()),
            keychain_cas: vec![old],
            ..fresh()
        };
        let plan = plan_install(&env, &facts);
        let planned = commands(&plan);
        let trust = planned
            .iter()
            .position(|c| c.contains("add-trusted-cert"))
            .unwrap();
        assert_eq!(planned[trust - 1], "create ca");
        assert_eq!(
            planned[trust + 1],
            "sudo security delete-certificate -Z 1D2D /Library/Keychains/System.keychain"
        );
        assert_eq!(
            plan.steps[trust + 1].description,
            "Remove the old carte local CA (alice@mac) from the system keychain"
        );

        let deletes = |facts: &Facts| -> Vec<String> {
            commands(&plan_install(&env, facts))
                .into_iter()
                .filter(|c| c.contains("delete-certificate"))
                .collect()
        };
        let replaced_by_the_daemon = Facts {
            ca: CaState::Untrusted,
            ca_hash: Some("C544".into()),
            keychain_cas: vec![
                facts.keychain_cas[0].clone(),
                KeychainCa {
                    hash: "C544".into(),
                    label: "carte local CA (alice@mac)".into(),
                },
            ],
            ..fresh()
        };
        assert_eq!(
            deletes(&replaced_by_the_daemon),
            ["sudo security delete-certificate -Z 1D2D /Library/Keychains/System.keychain"]
        );
        let unknown = Facts {
            ca: CaState::Unknown("security failed".into()),
            ..replaced_by_the_daemon.clone()
        };
        assert_eq!(deletes(&unknown).len(), 1);
        let trusted = Facts {
            ca: CaState::Trusted,
            ..replaced_by_the_daemon
        };
        assert!(deletes(&trusted).is_empty());
    }

    #[test]
    fn restarts_a_running_daemon_after_replacing_its_ca() {
        let dir = tempfile::tempdir().unwrap();
        let env = with_test_tld(env(dir.path()));
        let facts = Facts {
            resolvers: resolvers(&[("test", "nameserver 127.0.0.1\nport 9253\n")]),
            ca: CaState::Replace("it was made for .nova, .localhost".into()),
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..fresh()
        };
        let mut plan = plan_install(&env, &facts);
        set_tld(&mut plan, &env, &facts).unwrap();
        let root = dir.path().display();
        let planned = commands(&plan);
        assert_eq!(
            planned[planned.len() - 2..],
            [
                "launchctl kickstart -k gui/501/no.elektronaut.carte".to_string(),
                "verify".to_string(),
            ]
        );
        assert_eq!(
            planned.iter().filter(|c| c.contains("kickstart")).count(),
            1
        );
        assert_eq!(planned[0], format!("write {root}/config/config.toml"));

        let stopped = Facts {
            agent_loaded: false,
            ..facts.clone()
        };
        assert!(
            !commands(&plan_install(&env, &stopped))
                .iter()
                .any(|c| c.contains("kickstart"))
        );
        let replaced_plist = Facts {
            agent_plist: Some("old".into()),
            ..facts
        };
        assert!(
            !commands(&plan_install(&env, &replaced_plist))
                .iter()
                .any(|c| c.contains("kickstart"))
        );
    }

    #[test]
    fn plans_only_what_is_missing() {
        let root = Path::new("/r");
        let env = env(root);
        let done = Facts {
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
                "sudo security add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain /r/state/ca.pem",
                "launchctl bootout gui/501/no.elektronaut.carte",
                "wait until launchctl print gui/501/no.elektronaut.carte fails",
                "write /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
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
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
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
    fn replaces_an_agent_under_the_old_label() {
        let root = Path::new("/r");
        let env = env(root);
        let facts = Facts {
            ca: CaState::Trusted,
            old_agent: OldAgent {
                plist: true,
                loaded: true,
            },
            daemon_running: true,
            ..fresh()
        };
        let plan = plan_install(&env, &facts);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(
            commands(&plan),
            [
                "launchctl bootout gui/501/carte.daemon",
                "wait until launchctl print gui/501/carte.daemon fails",
                "rm /r/home/Library/LaunchAgents/carte.daemon.plist",
                "write /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
                "launchctl bootstrap gui/501 /r/home/Library/LaunchAgents/no.elektronaut.carte.plist",
                "verify",
            ]
        );
        assert_eq!(
            commands(&plan_uninstall(&env, &facts))[..3],
            [
                "launchctl bootout gui/501/carte.daemon",
                "wait until launchctl print gui/501/carte.daemon fails",
                "rm /r/home/Library/LaunchAgents/carte.daemon.plist",
            ]
        );
    }

    #[test]
    fn warns_about_a_binary_in_a_build_directory() {
        let mut env = with_test_tld(env(Path::new("/r")));
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
            resolvers: resolvers(&[("test", &env.resolver_contents())]),
            ca: CaState::Trusted,
            ca_files: env.ca_files()[..2].to_vec(),
            keychain_cas: vec![
                KeychainCa {
                    hash: "1D2D".into(),
                    label: "carte local CA (alice@mac)".into(),
                },
                KeychainCa {
                    hash: "C544".into(),
                    label: String::new(),
                },
            ],
            unknown_keychain_cas: vec![KeychainCa {
                hash: "7E7E".into(),
                label: "carte local CA (alice@mac)".into(),
            }],
            agent_plist: Some("x".into()),
            agent_loaded: true,
            ..fresh()
        };
        let plan = plan_uninstall(&env, &facts);
        assert_eq!(
            plan.steps[5].description,
            "Remove carte local CA (alice@mac) from the system keychain"
        );
        assert_eq!(
            plan.steps[6].description,
            "Remove the carte CA with SHA-256 C544 from the system keychain"
        );
        assert_eq!(
            plan.notes[1],
            "carte local CA (alice@mac) is left in the system keychain; it isn't known to be \
             this install's. If it is, remove it with \
             `sudo security delete-certificate -Z 7E7E /Library/Keychains/System.keychain`."
        );
        let root = dir.path().display();
        assert_eq!(
            commands(&plan),
            [
                "launchctl bootout gui/501/no.elektronaut.carte".to_string(),
                "wait until launchctl print gui/501/no.elektronaut.carte fails".to_string(),
                format!("rm {root}/home/Library/LaunchAgents/no.elektronaut.carte.plist"),
                format!("sudo rm -f {root}/resolver/test"),
                format!("sudo security remove-trusted-cert -d {root}/state/ca.pem"),
                "sudo security delete-certificate -Z 1D2D /Library/Keychains/System.keychain"
                    .to_string(),
                "sudo security delete-certificate -Z C544 /Library/Keychains/System.keychain"
                    .to_string(),
                format!("rm -f {root}/state/ca.pem {root}/state/ca-key.pem"),
            ]
        );
        assert!(plan.notes[0].contains("puma-dev -install"));

        let puma_dev_resolver = Facts {
            resolvers: resolvers(&[("test", PUMA_DEV_RESOLVER)]),
            ca: CaState::Untrusted,
            puma_dev: PumaDev {
                plist: true,
                binary: true,
                ..PumaDev::default()
            },
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

        let leftover_plist = Facts {
            puma_dev: PumaDev {
                plist: true,
                ..PumaDev::default()
            },
            ..puma_dev_resolver
        };
        let plan = plan_uninstall(&env, &leftover_plist);
        assert!(
            plan.notes[0].contains("`puma-dev -install`"),
            "{}",
            plan.notes[0]
        );
    }

    #[test]
    fn leaves_a_leftover_puma_dev_plist_alone() {
        let env = env(Path::new("/r"));
        let done = Facts {
            ca: CaState::Trusted,
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..fresh()
        };
        let leftover = Facts {
            puma_dev: PumaDev {
                plist: true,
                ..PumaDev::default()
            },
            ..done.clone()
        };
        assert!(!leftover.puma_dev.installed());
        assert_eq!(commands(&plan_install(&env, &leftover)), ["verify"]);
        let report = check::check(&env, &leftover).to_string();
        assert!(
            report.contains("  puma-dev        not installed, /r/home/Library/LaunchAgents/io.puma.dev.plist left behind\n"),
            "{report}"
        );

        let not_running = Facts {
            puma_dev: PumaDev {
                plist: true,
                binary: true,
                ..PumaDev::default()
            },
            ..done
        };
        assert!(not_running.puma_dev.installed());
        assert_eq!(
            commands(&plan_install(&env, &not_running)),
            ["launchctl disable gui/501/io.puma.dev", "verify"]
        );
    }

    #[test]
    fn reads_a_launch_agents_program() {
        let plist = "<dict>\n<key>Label</key>\n<string>io.puma.dev</string>\n\
                     <key>ProgramArguments</key>\n<array>\n<string>/opt/homebrew/bin/puma-dev</string>\n\
                     <string>-launchd</string>\n</array>";
        assert_eq!(
            launch_agent_program(plist),
            Some(PathBuf::from("/opt/homebrew/bin/puma-dev"))
        );
        assert_eq!(launch_agent_program("<string>x</string>"), None);
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
    fn a_plist_with_the_old_state_dir_key_is_outdated() {
        let root = Path::new("/r");
        let env = env(root);
        let old = env
            .agent()
            .to_plist()
            .replace("CARTE_STATE_HOME", "CARTE_STATE_DIR");
        let facts = Facts {
            ca: CaState::Trusted,
            agent_plist: Some(old),
            agent_loaded: true,
            ..fresh()
        };
        let report = check::check(&env, &facts).to_string();
        assert!(
            report.contains("differs from what setup writes now"),
            "{report}"
        );
        assert!(
            commands(&plan_install(&env, &facts))
                .contains(&"write /r/home/Library/LaunchAgents/no.elektronaut.carte.plist".into())
        );
    }

    #[test]
    fn installs_the_skill_and_links_it_for_claude() {
        let root = Path::new("/r");
        let env = env(root);
        let done = Facts {
            ca: CaState::Trusted,
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..fresh()
        };
        let missing = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Missing,
                link: SkillLink::Missing,
            }),
            ..done.clone()
        };
        let plan = plan_install(&env, &missing);
        assert_eq!(
            commands(&plan),
            [
                "install skill /r/home/.agents/skills/carte",
                "ln -s /r/home/.agents/skills/carte /r/home/.claude/skills/carte",
                "verify",
            ]
        );
        assert!(!plan.steps.iter().any(Step::needs_sudo));

        let current = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Managed { current: true },
                link: SkillLink::Ours,
            }),
            ..done.clone()
        };
        assert_eq!(commands(&plan_install(&env, &current)), ["verify"]);

        let outdated_without_claude = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Managed { current: false },
                link: SkillLink::NoClaude,
            }),
            ..done.clone()
        };
        assert_eq!(
            commands(&plan_install(&env, &outdated_without_claude)),
            ["install skill /r/home/.agents/skills/carte", "verify"]
        );

        let theirs = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Unmanaged,
                link: SkillLink::Other,
            }),
            ..done
        };
        let plan = plan_install(&env, &theirs);
        assert_eq!(commands(&plan), ["verify"]);
        assert!(
            plan.notes[0].contains(".managed-by-carte"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn uninstalls_only_carte_s_skill() {
        let root = Path::new("/r");
        let env = env(root);
        let ours = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Managed { current: true },
                link: SkillLink::Ours,
            }),
            ..fresh()
        };
        assert_eq!(
            commands(&plan_uninstall(&env, &ours)),
            [
                "rm /r/home/.claude/skills/carte",
                "rm -rf /r/home/.agents/skills/carte",
            ]
        );
        let theirs = Facts {
            skill: Some(SkillFacts {
                dir: SkillDir::Unmanaged,
                link: SkillLink::Other,
            }),
            ..fresh()
        };
        assert!(plan_uninstall(&env, &theirs).steps.is_empty());
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
