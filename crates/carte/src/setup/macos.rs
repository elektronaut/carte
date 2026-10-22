//! Setup on macOS: a LaunchAgent holds the ports, `/etc/resolver` sends lookups to carte
//! and the system keychain trusts its CA.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::cli::prompt::tilde;
use crate::tls::CertificateAuthority;
use crate::tls::trust::{self, CommandLine, KeychainCa};

use super::check::{Report, Status};
use super::plist::{Agent, LABEL, OLD_LABEL};
use super::{Action, CaState, Facts, Plan, SetupEnv, Step, is_tld};

pub const PUMA_DEV_LABEL: &str = "io.puma.dev";

impl SetupEnv {
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

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service(&self, label: &str) -> String {
        format!("gui/{}/{label}", self.uid)
    }
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

/// What setup found that only macOS has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacFacts {
    /// The CAs in the system keychain that this install created.
    pub keychain_cas: Vec<KeychainCa>,
    /// Other carte CAs in the system keychain under this install's name, which may be
    /// another install's.
    pub unknown_keychain_cas: Vec<KeychainCa>,
    pub agent_plist: Option<String>,
    pub agent_loaded: bool,
    /// The agent under its old label.
    pub old_agent: OldAgent,
    pub puma_dev: PumaDev,
}

impl MacFacts {
    pub fn gather(env: &SetupEnv) -> Self {
        let (keychain_cas, unknown_keychain_cas) = trust::own_cas(
            trust::keychain_cas(),
            &CertificateAuthority::created_hashes(&env.paths.state_dir),
            &crate::tls::ca::default_common_name(),
        );
        MacFacts {
            keychain_cas,
            unknown_keychain_cas,
            agent_plist: fs::read_to_string(env.agent_plist()).ok(),
            agent_loaded: launchd_loaded(&env.service(LABEL)),
            old_agent: OldAgent {
                plist: env.old_agent_plist().exists(),
                loaded: launchd_loaded(&env.service(OLD_LABEL)),
            },
            puma_dev: PumaDev {
                plist: env.puma_dev_plist().exists(),
                loaded: launchd_loaded(&env.service(PUMA_DEV_LABEL)),
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
        }
    }
}

/// Whether launchd has `service` (`gui/<uid>/<label>`) loaded, asked read-only.
pub fn launchd_loaded(service: &str) -> bool {
    cfg!(target_os = "macos")
        && Command::new("launchctl")
            .args(["print", service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// The pid of the process launchd runs for `service`, asked read-only.
pub fn launchd_pid(service: &str) -> Option<u32> {
    let output = Command::new("launchctl")
        .args(["print", service])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_launchd_pid(&String::from_utf8_lossy(&output.stdout))
}

fn parse_launchd_pid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
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

/// The program a LaunchAgent runs: the first of its `ProgramArguments`.
pub fn launch_agent_program(plist: &str) -> Option<PathBuf> {
    let (_, rest) = plist.split_once("<key>ProgramArguments</key>")?;
    let (_, rest) = rest.split_once("<string>")?;
    let (program, _) = rest.split_once("</string>")?;
    Some(PathBuf::from(program.trim()))
}

/// Whether the step starts or restarts carte's LaunchAgent.
pub(super) fn starts_daemon(step: &Step) -> bool {
    matches!(&step.action, Action::Run { command, .. }
        if command.program == "launchctl"
            && command.args.first().is_some_and(|a| a == "bootstrap" || a == "kickstart"))
}

pub(super) fn restart_step(env: &SetupEnv, description: &str) -> Step {
    Step::run(
        description,
        "launchctl",
        &["kickstart", "-k", &env.service(LABEL)],
    )
}

/// Stops a launchd service and waits until launchd no longer has it loaded.
fn bootout(description: impl Into<String>, env: &SetupEnv, label: &str) -> [Step; 2] {
    let service = env.service(label);
    [
        Step::run(description, "launchctl", &["bootout", &service]),
        Step {
            description: format!("Wait for launchd to finish stopping {label}"),
            action: Action::WaitUntilFails {
                command: CommandLine::new("launchctl", &["print", &service]),
                timeout: Duration::from_secs(60),
            },
        },
    ]
}

pub(super) fn plan_install(env: &SetupEnv, facts: &Facts, mac: &MacFacts) -> Plan {
    let mut plan = Plan::default();
    let steps = &mut plan.steps;
    let puma_dev = env.service(PUMA_DEV_LABEL);
    if mac.puma_dev.plist && mac.puma_dev.installed() {
        steps.push(Step::run(
            "Keep puma-dev's LaunchAgent from starting at login (its plist and ~/.puma-dev stay)",
            "launchctl",
            &["disable", &puma_dev],
        ));
    }
    if mac.puma_dev.loaded {
        steps.extend(bootout(
            "Stop puma-dev so carte can take over its ports",
            env,
            PUMA_DEV_LABEL,
        ));
    }

    old_agent_steps(env, mac, steps);

    for tld in super::leftover_resolvers(env, facts) {
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

    steps.extend(super::create_ca_step(facts));
    if facts.ca != CaState::Trusted {
        let command = trust::trust_command(&env.ca_cert());
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        steps.push(Step::sudo(
            format!(
                "Trust carte's local CA in the system keychain (it can only sign {} names)",
                env.ca_names()
            ),
            &command.program,
            &args,
        ));
        let kept = match facts.ca {
            CaState::Restore | CaState::Untrusted | CaState::Unknown(_) => facts.ca_hash.as_deref(),
            _ => None,
        };
        for ca in mac
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
    let plist_changed = mac.agent_plist.as_deref() != Some(plist.as_str());
    if plist_changed {
        if mac.agent_loaded {
            steps.extend(bootout(
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
    if plist_changed || !mac.agent_loaded {
        steps.push(Step::run(
            "Start the daemon now and at every login",
            "launchctl",
            &["bootstrap", &env.domain(), &plist_path.to_string_lossy()],
        ));
    }
    super::finish_install(&mut plan, env, facts);

    if facts.daemon_running && !mac.agent_loaded && !mac.old_agent.loaded {
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

/// Takes down an agent installed under the old label.
fn old_agent_steps(env: &SetupEnv, mac: &MacFacts, steps: &mut Vec<Step>) {
    if mac.old_agent.loaded {
        steps.extend(bootout(
            format!("Stop carte's LaunchAgent under its old label, {OLD_LABEL}"),
            env,
            OLD_LABEL,
        ));
    }
    if mac.old_agent.plist {
        steps.push(Step {
            description: format!("Remove carte's LaunchAgent under its old label, {OLD_LABEL}"),
            action: Action::RemoveFile {
                path: env.old_agent_plist(),
            },
        });
    }
}

pub(super) fn plan_uninstall(env: &SetupEnv, facts: &Facts, mac: &MacFacts) -> Plan {
    let mut plan = Plan::default();
    old_agent_steps(env, mac, &mut plan.steps);
    if mac.agent_loaded {
        plan.steps
            .extend(bootout("Stop the carte daemon and its apps", env, LABEL));
    }
    if mac.agent_plist.is_some() {
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
    super::skill_uninstall_steps(env, facts, &mut plan);
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

    for ca in &mac.keychain_cas {
        plan.steps.push(Step::sudo(
            format!("Remove {} from the system keychain", ca.name()),
            "security",
            &["delete-certificate", "-Z", &ca.hash, trust::SYSTEM_KEYCHAIN],
        ));
    }
    super::ca_files_uninstall_step(facts, &mut plan);

    let puma_dev = if mac.puma_dev.plist && mac.puma_dev.binary {
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
    for ca in &mac.unknown_keychain_cas {
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

/// The LaunchAgent's lines in `carte setup --check`.
pub(super) fn check_agent(report: &mut Report, env: &SetupEnv, mac: &MacFacts) {
    let plist = tilde(&env.agent_plist());
    let loaded = if mac.agent_loaded {
        "loaded"
    } else {
        "not loaded"
    };
    match &mac.agent_plist {
        None if mac.agent_loaded => report.push(
            Status::Warn,
            "LaunchAgent",
            format!("{LABEL} is loaded but {plist} is missing"),
        ),
        None => report.push(
            Status::Missing,
            "LaunchAgent",
            format!("{plist} is missing"),
        ),
        Some(text) if *text != env.agent().to_plist() => report.push(
            Status::Warn,
            "LaunchAgent",
            format!("{plist} differs from what setup writes now; {loaded}"),
        ),
        Some(_) => report.push(
            if mac.agent_loaded {
                Status::Ok
            } else {
                Status::Missing
            },
            "LaunchAgent",
            format!("{plist}, {loaded}"),
        ),
    }
    if mac.old_agent.installed() {
        report.push(
            Status::Warn,
            "LaunchAgent",
            format!(
                "{} is installed under its old label; `carte setup` replaces it",
                if mac.old_agent.plist {
                    tilde(&env.old_agent_plist())
                } else {
                    OLD_LABEL.to_string()
                }
            ),
        );
    }
}

/// puma-dev's line in `carte setup --check`, when there is anything of it.
pub(super) fn check_puma_dev(report: &mut Report, env: &SetupEnv, mac: &MacFacts) {
    let puma_dev = &mac.puma_dev;
    if !puma_dev.installed() && (puma_dev.plist || puma_dev.links > 0) {
        let mut parts = vec!["not installed".to_string()];
        if puma_dev.plist {
            parts.push(format!("{} left behind", tilde(&env.puma_dev_plist())));
        }
        if puma_dev.links > 0 {
            parts.push(format!(
                "{} entries in {}",
                puma_dev.links,
                tilde(&env.puma_dev_dir)
            ));
        }
        report.push(Status::Info, "puma-dev", parts.join(", "));
    } else if puma_dev.installed() {
        let mut parts = Vec::new();
        if puma_dev.plist {
            parts.push(format!("{} present", tilde(&env.puma_dev_plist())));
        }
        parts.push(if puma_dev.loaded {
            format!("{PUMA_DEV_LABEL} loaded")
        } else {
            format!("{PUMA_DEV_LABEL} not loaded")
        });
        parts.push(format!(
            "{} entries in {}",
            puma_dev.links,
            tilde(&env.puma_dev_dir)
        ));
        report.push(
            if puma_dev.loaded {
                Status::Warn
            } else {
                Status::Info
            },
            "puma-dev",
            parts.join(", "),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::setup::skill::{SkillDir, SkillFacts, SkillLink};
    use crate::setup::tests::{commands, env, fresh, resolvers, with_test_tld};
    use crate::setup::{
        OsFacts, check, leftover_resolvers, plan_install, plan_uninstall, previous_tlds, set_tld,
        with_tld,
    };

    const PUMA_DEV_RESOLVER: &str = "# Generated by puma-dev\nnameserver 127.0.0.1\nport 9253\n";

    /// A LaunchAgent that is installed as setup writes it, and loaded.
    fn loaded(env: &SetupEnv) -> OsFacts {
        OsFacts::MacOs(MacFacts {
            agent_plist: Some(env.agent().to_plist()),
            agent_loaded: true,
            ..MacFacts::default()
        })
    }

    fn with_mac(facts: &Facts, change: impl FnOnce(&mut MacFacts)) -> Facts {
        let mut mac = facts.mac().unwrap().clone();
        change(&mut mac);
        Facts {
            os: OsFacts::MacOs(mac),
            ..facts.clone()
        }
    }

    #[test]
    fn reads_the_pid_from_launchctl_print() {
        let text = "gui/501/no.elektronaut.carte = {\n\tactive count = 2\n\tstate = running\n\tpid = 32003\n\tprogram = /x/carte\n}\n";
        assert_eq!(parse_launchd_pid(text), Some(32003));
        assert_eq!(parse_launchd_pid("state = not running\n"), None);
    }

    #[test]
    fn plans_a_switch_from_puma_dev() {
        let root = Path::new("/r");
        let env = with_test_tld(env(root));
        let facts = Facts {
            resolvers: resolvers(&[("test", PUMA_DEV_RESOLVER)]),
            os: OsFacts::MacOs(MacFacts {
                puma_dev: PumaDev {
                    plist: true,
                    loaded: true,
                    binary: true,
                    links: 3,
                    domains: vec!["test".into()],
                },
                ..MacFacts::default()
            }),
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
            os: OsFacts::MacOs(MacFacts {
                puma_dev: PumaDev {
                    plist: true,
                    loaded: true,
                    binary: true,
                    links: 3,
                    domains: vec!["test".into(), "localhost".into()],
                },
                ..MacFacts::default()
            }),
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
            os: loaded(&env),
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
            os: OsFacts::MacOs(MacFacts {
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
                ..MacFacts::default()
            }),
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
            os: OsFacts::MacOs(MacFacts {
                keychain_cas: vec![old.clone()],
                ..MacFacts::default()
            }),
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
            os: OsFacts::MacOs(MacFacts {
                keychain_cas: vec![
                    old,
                    KeychainCa {
                        hash: "C544".into(),
                        label: "carte local CA (alice@mac)".into(),
                    },
                ],
                ..MacFacts::default()
            }),
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
            os: loaded(&env),
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

        let stopped = with_mac(&facts, |mac| mac.agent_loaded = false);
        assert!(
            !commands(&plan_install(&env, &stopped))
                .iter()
                .any(|c| c.contains("kickstart"))
        );
        let replaced_plist = with_mac(&facts, |mac| mac.agent_plist = Some("old".into()));
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
            os: loaded(&env),
            ..fresh()
        };
        let plan = plan_install(&env, &done);
        assert_eq!(commands(&plan), ["verify"]);
        assert!(!plan.changes_anything());

        let changed = Facts {
            ca: CaState::Untrusted,
            ..with_mac(&done, |mac| mac.agent_plist = Some("old".into()))
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
            daemon_running: true,
            ..with_mac(&done, |mac| mac.agent_loaded = false)
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
            os: OsFacts::MacOs(MacFacts {
                old_agent: OldAgent {
                    plist: true,
                    loaded: true,
                },
                ..MacFacts::default()
            }),
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
            os: OsFacts::MacOs(MacFacts {
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
                ..MacFacts::default()
            }),
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
            os: OsFacts::MacOs(MacFacts {
                puma_dev: PumaDev {
                    plist: true,
                    binary: true,
                    ..PumaDev::default()
                },
                ..MacFacts::default()
            }),
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

        let leftover_plist = with_mac(&puma_dev_resolver, |mac| {
            mac.puma_dev = PumaDev {
                plist: true,
                ..PumaDev::default()
            }
        });
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
            os: loaded(&env),
            ..fresh()
        };
        let leftover = with_mac(&done, |mac| {
            mac.puma_dev = PumaDev {
                plist: true,
                ..PumaDev::default()
            }
        });
        assert!(!leftover.mac().unwrap().puma_dev.installed());
        assert_eq!(commands(&plan_install(&env, &leftover)), ["verify"]);
        let report = check::check(&env, &leftover).to_string();
        assert!(
            report.contains("  puma-dev        not installed, /r/home/Library/LaunchAgents/io.puma.dev.plist left behind\n"),
            "{report}"
        );

        let not_running = with_mac(&done, |mac| {
            mac.puma_dev = PumaDev {
                plist: true,
                binary: true,
                ..PumaDev::default()
            }
        });
        assert!(not_running.mac().unwrap().puma_dev.installed());
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
    fn a_plist_with_the_old_state_dir_key_is_outdated() {
        let root = Path::new("/r");
        let env = env(root);
        let old = env
            .agent()
            .to_plist()
            .replace("CARTE_STATE_HOME", "CARTE_STATE_DIR");
        let facts = Facts {
            ca: CaState::Trusted,
            os: OsFacts::MacOs(MacFacts {
                agent_plist: Some(old),
                agent_loaded: true,
                ..MacFacts::default()
            }),
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
            os: loaded(&env),
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
}
