//! Setup on Linux: systemd user units hold the ports and run the daemon, and a sysctl
//! lets them open ports below 1024 without root.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::prompt::tilde;

use super::check::{PortUser, Report, Status};
use super::trust_stores::{self, TrustFacts};
use super::units::{SERVICE, UNITS};
use super::{Action, Facts, Plan, SetupEnv, Step};

pub const PORT_START_KEY: &str = "net.ipv4.ip_unprivileged_port_start";

impl SetupEnv {
    pub fn unit_path(&self, unit: &str) -> PathBuf {
        self.systemd_user_dir.join(unit)
    }

    pub fn sysctl_file(&self) -> PathBuf {
        self.sysctl_dir.join("50-carte.conf")
    }

    /// The lower of carte's ports when it needs root to open, below 1024.
    pub fn privileged_port(&self) -> Option<u16> {
        Some(self.machine.http_port.min(self.machine.https_port)).filter(|&port| port < 1024)
    }

    pub fn sysctl_contents(&self, port: u16) -> String {
        format!(
            "# Written by `carte setup`: lets systemd open carte's ports {} and {} for your user.\n\
             {PORT_START_KEY}={port}\n",
            self.machine.http_port, self.machine.https_port
        )
    }
}

/// A systemd user unit as `systemctl --user show` reports it, and its file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unit {
    pub file: Option<String>,
    pub enabled: bool,
    /// Its `ActiveState`: `active`, `inactive`, `failed`, …
    pub state: String,
}

impl Unit {
    pub fn active(&self) -> bool {
        self.state == "active"
    }
}

/// What setup found that only Linux has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinuxFacts {
    /// Whether `systemctl --user` answers.
    pub systemd: bool,
    /// carte's units, by name.
    pub units: BTreeMap<String, Unit>,
    /// The current `net.ipv4.ip_unprivileged_port_start`.
    pub port_start: Option<u16>,
    /// carte's sysctl drop-in.
    pub sysctl_file: Option<String>,
    /// Whether the sysctl drop-in directory exists; Fedora has none.
    pub sysctl_dir: bool,
    /// The other sysctl files that set the port start, and their values.
    pub port_start_files: Vec<(PathBuf, String)>,
    /// Whether the loopback interface has `::1`.
    pub ipv6: bool,
    pub trust: TrustFacts,
}

impl LinuxFacts {
    pub fn gather(env: &SetupEnv) -> Self {
        let show = cfg!(target_os = "linux")
            .then(|| {
                Command::new("systemctl")
                    .args(["--user", "show", "-p", "Id", "-p", "ActiveState"])
                    .args(["-p", "UnitFileState"])
                    .args(UNITS)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
            })
            .flatten();
        let mut states = show
            .as_ref()
            .map(|output| parse_show(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default();
        let units = UNITS
            .iter()
            .map(|&name| {
                let mut props = states.remove(name).unwrap_or_default();
                let unit = Unit {
                    file: fs::read_to_string(env.unit_path(name)).ok(),
                    enabled: props.get("UnitFileState").is_some_and(|s| s == "enabled"),
                    state: props.remove("ActiveState").unwrap_or_default(),
                };
                (name.to_string(), unit)
            })
            .collect();
        LinuxFacts {
            systemd: show.is_some(),
            units,
            port_start: fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
                .ok()
                .and_then(|text| text.trim().parse().ok()),
            sysctl_file: fs::read_to_string(env.sysctl_file()).ok(),
            sysctl_dir: env.sysctl_dir.is_dir(),
            port_start_files: port_start_files(&env.sysctl_file()),
            ipv6: has_ipv6_loopback(),
            trust: TrustFacts::gather(env),
        }
    }

    pub fn unit(&self, name: &str) -> Unit {
        self.units.get(name).cloned().unwrap_or_default()
    }

    fn service_active(&self) -> bool {
        self.unit(SERVICE).active()
    }
}

/// The sysctl files other than `ours` that set the port start, and their values; a
/// link to a file already listed, like Debian's 99-sysctl.conf, counts once.
fn port_start_files(ours: &Path) -> Vec<(PathBuf, String)> {
    let dirs = [
        "/etc/sysctl.d",
        "/run/sysctl.d",
        "/usr/local/lib/sysctl.d",
        "/usr/lib/sysctl.d",
        "/lib/sysctl.d",
    ];
    let mut seen = std::collections::BTreeSet::new();
    dirs.iter()
        .flat_map(|dir| {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
        })
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "conf"))
        .chain([PathBuf::from("/etc/sysctl.conf")])
        .filter(|path| path != ours)
        .filter(|path| seen.insert(fs::canonicalize(path).unwrap_or_else(|_| path.clone())))
        .filter_map(|path| {
            let value = parse_sysctl(&fs::read_to_string(&path).ok()?, PORT_START_KEY)?;
            Some((path, value))
        })
        .collect()
}

/// The files that `sysctl --system` reads after carte's drop-in and that set the port
/// start above `port`, overriding it: later names, and `/etc/sysctl.conf`, last of all.
pub fn overriding(files: &[(PathBuf, String)], ours: &Path, port: u16) -> Vec<String> {
    let ours = ours.file_name().unwrap_or_default();
    files
        .iter()
        .filter(|(path, _)| {
            path == Path::new("/etc/sysctl.conf") || path.file_name().unwrap_or_default() > ours
        })
        .filter(|(_, value)| value.parse::<u16>().map_or(true, |v| v > port))
        .map(|(path, value)| format!("{} sets {value}", path.display()))
        .collect()
}

/// Where the user's systemd looks for units: under the user manager's
/// `XDG_CONFIG_HOME` when it has an absolute one, or when it doesn't answer, the shell's;
/// otherwise under `~/.config`.
pub fn systemd_user_dir(home: &Path, manager: Option<&str>, shell: Option<&OsStr>) -> PathBuf {
    let config_home = match manager {
        Some(environment) => parse_environment(environment, "XDG_CONFIG_HOME").map(PathBuf::from),
        None => shell.filter(|v| !v.is_empty()).map(PathBuf::from),
    };
    config_home
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
}

/// The user manager's environment, from `systemctl --user show-environment`.
pub fn manager_environment() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A variable's value in `systemctl show-environment` output; one it had to quote
/// counts as unset.
fn parse_environment(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.starts_with("$'"))
}

/// Whether the loopback interface has `::1`, from `/proc/net/if_inet6`.
pub fn has_ipv6_loopback() -> bool {
    fs::read_to_string("/proc/net/if_inet6").is_ok_and(|text| parse_if_inet6(&text))
}

fn parse_if_inet6(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("00000000000000000000000000000001 "))
}

/// The properties of each unit in `systemctl show -p Id …` output, by its `Id`.
pub fn parse_show(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut units = BTreeMap::new();
    for block in text.split("\n\n") {
        let props: BTreeMap<String, String> = block
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        if let Some(id) = props.get("Id").cloned() {
            units.insert(id, props);
        }
    }
    units
}

/// The value a sysctl drop-in sets for `key`.
pub fn parse_sysctl(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, value)| value.trim().to_string())
        .next_back()
}

/// The `MainPID` of a user unit, asked read-only; `None` when it isn't running.
pub fn main_pid(unit: &str) -> Option<u32> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", unit])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_main_pid(&String::from_utf8_lossy(&output.stdout))
}

fn parse_main_pid(text: &str) -> Option<u32> {
    text.trim().parse().ok().filter(|&pid| pid != 0)
}

/// Parses `ss -Hlnp` output. A socket systemd holds for carte lists both; then only
/// carte counts.
pub fn parse_ss(output: &str) -> Vec<PortUser> {
    let mut users = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(address) = fields.get(3) else {
            continue;
        };
        let process = fields
            .get(5..)
            .map(|rest| rest.join(" "))
            .unwrap_or_default();
        let mut here: Vec<PortUser> = process
            .split("(\"")
            .skip(1)
            .filter_map(|entry| {
                let (command, rest) = entry.split_once('"')?;
                let pid = rest
                    .split_once("pid=")?
                    .1
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()?;
                Some(PortUser {
                    command: command.to_string(),
                    pid,
                    address: address.to_string(),
                })
            })
            .collect();
        if here.iter().any(|u| u.command == "carte") {
            here.retain(|u| u.command == "carte");
        }
        if here.is_empty() {
            here.push(PortUser {
                command: "unknown".into(),
                pid: 0,
                address: address.to_string(),
            });
        }
        for user in here {
            if !users.contains(&user) {
                users.push(user);
            }
        }
    }
    users
}

/// Who listens on `port` (`TCP`) or has it bound (`UDP`); empty when ss isn't there.
pub fn port_users(protocol: &str, port: u16) -> Vec<PortUser> {
    let flags = if protocol == "TCP" {
        "-Hlnpt"
    } else {
        "-Hlnpu"
    };
    Command::new("ss")
        .args([flags, "sport", "=", &format!(":{port}")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| parse_ss(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

fn systemctl_args(step: &Step) -> Option<&[String]> {
    match &step.action {
        Action::Run { command, .. } if command.program == "systemctl" => Some(&command.args),
        _ => None,
    }
}

/// Whether the steps start the daemon afresh: they restart it, or `enable --now` it
/// when it isn't running or they stopped it first.
pub(super) fn starts_daemon(steps: &[Step], linux: &LinuxFacts) -> bool {
    let has = |arg: &str| {
        steps
            .iter()
            .filter_map(systemctl_args)
            .any(|args| args.iter().any(|a| a == arg))
    };
    has("restart") || has("--now") && (has("stop") || !linux.service_active())
}

pub(super) fn restart_step(description: &str) -> Step {
    Step::run(description, "systemctl", &["--user", "restart", SERVICE])
}

fn systemctl(description: impl Into<String>, args: &[&str]) -> Step {
    let mut all = vec!["--user"];
    all.extend(args);
    Step::run(description, "systemctl", &all)
}

pub(super) fn plan_install(env: &SetupEnv, facts: &Facts, linux: &LinuxFacts) -> Plan {
    let mut plan = Plan::default();
    let steps = &mut plan.steps;
    match env.privileged_port() {
        Some(port) if linux.port_start.is_none_or(|start| start > port) => {
            let contents = env.sysctl_contents(port);
            let overriding = overriding(&linux.port_start_files, &env.sysctl_file(), port);
            if !overriding.is_empty() {
                plan.notes.push(format!(
                    "{}, after carte's {}, so systemd can't open port {port} for carte; set it \
                     to {port} or lower there (it applies to every program), or use ports from \
                     1024 up",
                    overriding.join(" and "),
                    env.sysctl_file().display()
                ));
            } else {
                if linux.sysctl_file.as_deref() != Some(contents.as_str()) {
                    if !linux.sysctl_dir {
                        steps.push(Step::sudo(
                            format!("Create {}", env.sysctl_dir.display()),
                            "mkdir",
                            &["-p", &env.sysctl_dir.to_string_lossy()],
                        ));
                    }
                    steps.push(
                        Step::sudo(
                            format!(
                                "Let programs open ports from {port} up without root (this is machine-wide)"
                            ),
                            "tee",
                            &[&env.sysctl_file().to_string_lossy()],
                        )
                        .with_stdin(contents),
                    );
                }
                steps.push(Step::sudo(
                    format!("Apply the sysctl drop-ins, so systemd can open port {port} now"),
                    "sysctl",
                    &["--system"],
                ));
            }
        }
        Some(_) => {}
        None => sysctl_removal_steps(
            env,
            linux,
            &format!(
                "Remove carte's sysctl setting for low ports: ports {} and {} don't need it",
                env.machine.http_port, env.machine.https_port
            ),
            steps,
        ),
    }

    steps.extend(super::create_ca_step(facts));
    trust_stores::install_steps(env, facts, &linux.trust, &mut plan);
    let steps = &mut plan.steps;

    if linux.systemd {
        let agent = env.agent();
        let units = agent.units(linux.ipv6);
        let changed: Vec<_> = units
            .iter()
            .filter(|(name, contents)| linux.unit(name).file.as_deref() != Some(contents))
            .collect();
        if !changed.is_empty() {
            let active: Vec<&str> = UNITS
                .into_iter()
                .filter(|name| linux.unit(name).active())
                .collect();
            if !active.is_empty() {
                let mut args = vec!["stop"];
                args.extend(&active);
                steps.push(systemctl("Stop carte's units to replace them", &args));
            }
            for (name, contents) in &changed {
                let description = if *name == SERVICE {
                    format!(
                        "Install the user service that runs `carte daemon` with ports {} and {} from systemd",
                        env.machine.http_port, env.machine.https_port
                    )
                } else {
                    let port = if contents.contains("FileDescriptorName=https") {
                        env.machine.https_port
                    } else {
                        env.machine.http_port
                    };
                    format!("Install the socket unit that opens port {port} for carte")
                };
                steps.push(Step {
                    description,
                    action: Action::WriteFile {
                        path: env.unit_path(name),
                        contents: contents.clone(),
                    },
                });
            }
            steps.push(systemctl(
                "Have systemd read the new units",
                &["daemon-reload"],
            ));
        }
        let ready = UNITS.iter().all(|name| {
            let unit = linux.unit(name);
            unit.enabled && unit.active()
        });
        if !changed.is_empty() || !ready {
            let failed: Vec<&str> = UNITS
                .into_iter()
                .filter(|name| linux.unit(name).state == "failed")
                .collect();
            if !failed.is_empty() {
                let mut args = vec!["reset-failed"];
                args.extend(&failed);
                steps.push(systemctl(
                    "Clear the failure of carte's units, so systemd starts them again",
                    &args,
                ));
            }
            let mut args = vec!["enable", "--now"];
            args.extend(UNITS);
            steps.push(systemctl("Start the daemon now and at every login", &args));
        }
        super::finish_install(&mut plan, env, facts);
    } else {
        if let Some(skill) = &facts.skill {
            super::skill_steps(env, skill, &mut plan);
        }
        plan.notes.push(
            "systemd doesn't answer for your user (`systemctl --user`), so setup can't install \
             carte's service; run `carte daemon` yourself"
                .to_string(),
        );
    }

    if env.needs_resolver() {
        plan.notes.push(format!(
            "setup doesn't send .{tld} lookups to carte on Linux yet; point them at 127.0.0.1 port \
             {port}, such as with dnsmasq's `server=/{tld}/127.0.0.1#{port}`",
            tld = env.machine.tld,
            port = env.machine.dns_port,
        ));
    }
    if facts.daemon_running && !linux.service_active() {
        plan.notes.push(format!(
            "a carte daemon is already running on {}; stop it first, or the service's daemon will refuse to start",
            tilde(&env.paths.socket())
        ));
    }
    if env.exe.components().any(|c| c.as_os_str() == "target") {
        plan.notes.push(format!(
            "the service will run {}, a build directory; install carte (`cargo install --path crates/carte`) and run setup from there to keep it working after a `cargo clean`",
            tilde(&env.exe)
        ));
    }
    plan
}

/// Removes carte's sysctl drop-in. When its value is the one in effect, resets the
/// setting to the kernel's default and lets the remaining files set it again.
fn sysctl_removal_steps(
    env: &SetupEnv,
    linux: &LinuxFacts,
    description: &str,
    steps: &mut Vec<Step>,
) {
    let Some(text) = &linux.sysctl_file else {
        return;
    };
    steps.push(Step::sudo(
        description,
        "rm",
        &["-f", &env.sysctl_file().to_string_lossy()],
    ));
    let ours = parse_sysctl(text, PORT_START_KEY).and_then(|v| v.parse::<u16>().ok());
    if ours.is_some_and(|v| v < 1024) && linux.port_start == ours {
        steps.push(Step::sudo(
            "Let only root open ports below 1024 again",
            "sysctl",
            &["-w", &format!("{PORT_START_KEY}=1024")],
        ));
        steps.push(Step::sudo(
            "Apply the remaining sysctl files, in case one sets it too",
            "sysctl",
            &["--system"],
        ));
    }
}

pub(super) fn plan_uninstall(env: &SetupEnv, facts: &Facts, linux: &LinuxFacts) -> Plan {
    let mut plan = Plan::default();
    let units = |keep: fn(&Unit) -> bool| -> Vec<&str> {
        UNITS
            .into_iter()
            .filter(|name| keep(&linux.unit(name)))
            .collect()
    };
    let active = units(Unit::active);
    if !active.is_empty() {
        let mut args = vec!["stop"];
        args.extend(&active);
        plan.steps
            .push(systemctl("Stop the carte daemon and its apps", &args));
    }
    let enabled = units(|unit| unit.enabled);
    if !enabled.is_empty() {
        let mut args = vec!["disable"];
        args.extend(&enabled);
        plan.steps
            .push(systemctl("Keep carte from starting at login", &args));
    }
    let failed = units(|unit| unit.state == "failed");
    if !failed.is_empty() {
        let mut args = vec!["reset-failed"];
        args.extend(&failed);
        plan.steps
            .push(systemctl("Clear the failure of carte's units", &args));
    }
    let files = units(|unit| unit.file.is_some());
    for name in &files {
        plan.steps.push(Step {
            description: format!("Remove {name}"),
            action: Action::RemoveFile {
                path: env.unit_path(name),
            },
        });
    }
    if !files.is_empty() {
        plan.steps.push(systemctl(
            "Have systemd forget carte's units",
            &["daemon-reload"],
        ));
    }
    sysctl_removal_steps(
        env,
        linux,
        "Remove carte's sysctl setting for low ports",
        &mut plan.steps,
    );
    super::skill_uninstall_steps(env, facts, &mut plan);
    trust_stores::uninstall_steps(&linux.trust, &mut plan);
    super::ca_files_uninstall_step(facts, &mut plan);
    plan.notes.push(format!(
        "your projects in {config} and the rest of carte's state in {state} are left alone.",
        config = tilde(&env.paths.config_dir),
        state = tilde(&env.paths.state_dir),
    ));
    plan
}

/// The units' and the port sysctl's lines in `carte setup --check`.
pub(super) fn check_units(report: &mut Report, env: &SetupEnv, facts: &Facts, linux: &LinuxFacts) {
    trust_stores::check(report, facts, &linux.trust);
    if let Some(port) = env.privileged_port() {
        match linux.port_start {
            Some(start) if start <= port => report.push(
                Status::Ok,
                "low ports",
                format!("{PORT_START_KEY} is {start}"),
            ),
            Some(start) => {
                let overriding = overriding(&linux.port_start_files, &env.sysctl_file(), port);
                let why = if overriding.is_empty() {
                    String::new()
                } else {
                    format!(" ({}, after carte's drop-in)", overriding.join(" and "))
                };
                report.push(
                    Status::Missing,
                    "low ports",
                    format!(
                        "{PORT_START_KEY} is {start}{why}, so systemd can't open port {port} for carte"
                    ),
                );
            }
            None => report.push(
                Status::Warn,
                "low ports",
                format!("couldn't read {PORT_START_KEY}"),
            ),
        }
    }
    if !linux.systemd {
        report.push(
            Status::Missing,
            "systemd",
            "`systemctl --user` doesn't answer, so carte's service can't run",
        );
        return;
    }
    for (name, contents) in env.agent().units(linux.ipv6) {
        let unit = linux.unit(name);
        let path = tilde(&env.unit_path(name));
        let enabled = if unit.enabled {
            "enabled"
        } else {
            "not enabled"
        };
        let state = if unit.state.is_empty() {
            "inactive"
        } else {
            unit.state.as_str()
        };
        match &unit.file {
            None => report.push(Status::Missing, name, format!("{path} is missing")),
            Some(text) if *text != contents => report.push(
                Status::Warn,
                name,
                format!("{path} differs from what setup writes now; {enabled}, {state}"),
            ),
            Some(_) => report.push(
                if unit.enabled && unit.active() {
                    Status::Ok
                } else {
                    Status::Missing
                },
                name,
                format!("{path}, {enabled}, {state}"),
            ),
        }
    }
}

/// The DNS line in `carte setup --check` for a TLD that needs one.
pub(super) fn check_dns(report: &mut Report, env: &SetupEnv) {
    report.push(
        Status::Missing,
        "resolver",
        format!(
            "setup doesn't send .{} lookups to carte on Linux yet; point them at 127.0.0.1 port {}",
            env.machine.tld, env.machine.dns_port
        ),
    );
}

/// What to do when the daemon didn't come up behind systemd's sockets.
pub fn fallback(env: &SetupEnv) -> String {
    format!(
        "- `systemctl --user status carte.service carte-http.socket carte-https.socket` shows \
         whether systemd started carte and why it stopped; its log is {log}.\n\
         - If a socket unit failed with `Permission denied`, {PORT_START_KEY} is above {low}; \
         `sysctl {PORT_START_KEY}` shows it.\n\
         - If systemd didn't open ports {http} and {https} on 127.0.0.1, carte can still run on \
         high ports: set http_port = 8080 and https_port = 8443 in {config} and run `carte setup` \
         again; URLs then carry the port.\n\
         - `carte setup --uninstall` undoes these changes.",
        log = tilde(&env.agent().log_path()),
        http = env.machine.http_port,
        https = env.machine.https_port,
        low = env.machine.http_port.min(env.machine.https_port),
        config = tilde(&env.paths.machine_config()),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use std::path::Path;

    use super::*;
    use crate::setup::tests::{commands, env, fresh, with_test_tld};
    use crate::setup::trust_stores::SystemStore;
    use crate::setup::units::HTTP_SOCKET;
    use crate::setup::{CaState, OsFacts, Platform, plan_install, plan_uninstall, set_tld};

    fn linux_env(root: &Path) -> SetupEnv {
        SetupEnv {
            platform: Platform::Linux,
            ..env(root)
        }
    }

    pub(in crate::setup) const HASH: &str = "C544AB0012FF34EE9900";

    pub(in crate::setup) fn on_linux(linux: LinuxFacts) -> Facts {
        Facts {
            ca_hash: Some(HASH.into()),
            os: OsFacts::Linux(linux),
            ..fresh()
        }
    }

    pub(in crate::setup) fn debian(own: &[&str]) -> SystemStore {
        SystemStore {
            dir: "/usr/local/share/ca-certificates".into(),
            update: "/usr/sbin/update-ca-certificates".into(),
            extension: "crt".into(),
            own: own.iter().map(|name| name.to_string()).collect(),
            others: Vec::new(),
        }
    }

    pub(in crate::setup) fn bare() -> LinuxFacts {
        LinuxFacts {
            systemd: true,
            sysctl_dir: true,
            port_start: Some(1024),
            ipv6: true,
            trust: TrustFacts {
                system: Some(debian(&[])),
                certutil: Some("/usr/bin/certutil".into()),
                ..TrustFacts::default()
            },
            ..LinuxFacts::default()
        }
    }

    /// Units installed as setup writes them, enabled and running, low ports open, and
    /// the CA in the system's store.
    pub(in crate::setup) fn ready(env: &SetupEnv) -> LinuxFacts {
        let units = env
            .agent()
            .units(true)
            .map(|(name, contents)| {
                let unit = Unit {
                    file: Some(contents),
                    enabled: true,
                    state: "active".into(),
                };
                (name.to_string(), unit)
            })
            .into_iter()
            .collect();
        LinuxFacts {
            units,
            port_start: Some(80),
            trust: TrustFacts {
                system: Some(debian(&["carte-C544AB0012FF34EE"])),
                certutil: Some("/usr/bin/certutil".into()),
                ..TrustFacts::default()
            },
            ..bare()
        }
    }

    #[test]
    fn plans_a_fresh_install() {
        let env = linux_env(Path::new("/r"));
        let plan = plan_install(&env, &on_linux(bare()));
        assert_eq!(
            commands(&plan),
            [
                "sudo tee /r/sysctl.d/50-carte.conf",
                "sudo sysctl --system",
                "create ca",
                "sudo install -m 644 /r/state/ca.pem '/usr/local/share/ca-certificates/carte-{new-ca-hash}.crt'",
                "sudo /usr/sbin/update-ca-certificates",
                "write /r/home/.config/systemd/user/carte-http.socket",
                "write /r/home/.config/systemd/user/carte-https.socket",
                "write /r/home/.config/systemd/user/carte.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now carte-http.socket carte-https.socket carte.service",
                "verify",
            ]
        );
        let Action::Run { stdin, .. } = &plan.steps[0].action else {
            panic!()
        };
        assert_eq!(
            stdin.as_deref(),
            Some(
                "# Written by `carte setup`: lets systemd open carte's ports 80 and 443 for your user.\n\
                 net.ipv4.ip_unprivileged_port_start=80\n"
            )
        );
        assert_eq!(
            plan.steps[5].description,
            "Install the socket unit that opens port 80 for carte"
        );
        assert_eq!(
            plan.steps[6].description,
            "Install the socket unit that opens port 443 for carte"
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let shown = plan.render(&env, false);
        assert!(
            shown.contains(
                "2. Apply the sysctl drop-ins, so systemd can open port 80 now\n   \
                 $ sudo sysctl --system\n"
            ),
            "{shown}"
        );
    }

    #[test]
    fn plans_only_what_is_missing() {
        let env = linux_env(Path::new("/r"));
        let done = Facts {
            ca: CaState::Trusted,
            ..on_linux(ready(&env))
        };
        let plan = plan_install(&env, &done);
        assert_eq!(commands(&plan), ["verify"]);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let mut stopped = ready(&env);
        stopped.units.get_mut(SERVICE).unwrap().state = "failed".into();
        let stopped = Facts {
            ca: CaState::Trusted,
            ..on_linux(stopped)
        };
        assert_eq!(
            commands(&plan_install(&env, &stopped)),
            [
                "systemctl --user reset-failed carte.service",
                "systemctl --user enable --now carte-http.socket carte-https.socket carte.service",
                "verify",
            ]
        );
    }

    #[test]
    fn replaces_a_changed_unit_after_stopping_carte() {
        let env = linux_env(Path::new("/r"));
        let mut changed = ready(&env);
        changed.units.get_mut(SERVICE).unwrap().file = Some("old".into());
        let facts = Facts {
            ca: CaState::Replace("it is about to expire".into()),
            ..on_linux(changed)
        };
        assert_eq!(
            commands(&plan_install(&env, &facts)),
            [
                "create ca",
                "sudo rm -f /usr/local/share/ca-certificates/carte-C544AB0012FF34EE.crt",
                "sudo install -m 644 /r/state/ca.pem '/usr/local/share/ca-certificates/carte-{new-ca-hash}.crt'",
                "sudo /usr/sbin/update-ca-certificates",
                "systemctl --user stop carte-http.socket carte-https.socket carte.service",
                "write /r/home/.config/systemd/user/carte.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now carte-http.socket carte-https.socket carte.service",
                "verify",
            ]
        );
    }

    #[test]
    fn restarts_a_running_daemon_for_a_new_ca_or_tld() {
        let dir = tempfile::tempdir().unwrap();
        let env = linux_env(dir.path());
        let facts = Facts {
            ca: CaState::Replace("it was made for .nova, .localhost".into()),
            ..on_linux(ready(&env))
        };
        let planned = commands(&plan_install(&env, &facts));
        assert_eq!(
            planned[planned.len() - 3..],
            [
                "sudo /usr/sbin/update-ca-certificates",
                "systemctl --user restart carte.service",
                "verify"
            ]
        );

        let test = with_test_tld(env);
        let trusted = Facts {
            ca: CaState::Trusted,
            ..on_linux(ready(&test))
        };
        let mut plan = plan_install(&test, &trusted);
        set_tld(&mut plan, &test, &trusted).unwrap();
        let root = dir.path().display();
        assert_eq!(
            commands(&plan),
            [
                format!("write {root}/config/config.toml"),
                "systemctl --user restart carte.service".to_string(),
                "verify".to_string(),
            ]
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("server=/test/127.0.0.1#9253")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn leaves_the_port_sysctl_alone_when_it_isnt_needed() {
        let env = linux_env(Path::new("/r"));
        let sysctl = |env: &SetupEnv, facts: LinuxFacts| -> Vec<String> {
            commands(&plan_install(env, &on_linux(facts)))
                .into_iter()
                .filter(|c| c.contains("sysctl"))
                .collect()
        };
        let open = LinuxFacts {
            port_start: Some(0),
            ..bare()
        };
        assert!(sysctl(&env, open).is_empty());

        let mut high = linux_env(Path::new("/r"));
        high.machine.http_port = 8080;
        high.machine.https_port = 8443;
        assert_eq!(high.privileged_port(), None);
        assert!(sysctl(&high, bare()).is_empty());

        let written = LinuxFacts {
            sysctl_file: Some(env.sysctl_contents(80)),
            ..bare()
        };
        assert_eq!(sysctl(&env, written), ["sudo sysctl --system"]);
        let fedora = LinuxFacts {
            sysctl_dir: false,
            ..bare()
        };
        assert_eq!(
            commands(&plan_install(&env, &on_linux(fedora)))[..3],
            [
                "sudo mkdir -p /r/sysctl.d",
                "sudo tee /r/sysctl.d/50-carte.conf",
                "sudo sysctl --system",
            ]
        );
    }

    #[test]
    fn without_systemd_only_notes_how_to_run_the_daemon() {
        let env = linux_env(Path::new("/r"));
        let facts = LinuxFacts {
            systemd: false,
            port_start: Some(80),
            ..bare()
        };
        let plan = plan_install(&env, &on_linux(facts));
        assert_eq!(
            commands(&plan),
            [
                "create ca",
                "sudo install -m 644 /r/state/ca.pem '/usr/local/share/ca-certificates/carte-{new-ca-hash}.crt'",
                "sudo /usr/sbin/update-ca-certificates",
            ]
        );
        assert!(
            plan.notes[0].contains("run `carte daemon` yourself"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn plans_the_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let env = linux_env(dir.path());
        let facts = Facts {
            ca: CaState::Trusted,
            ca_files: env.ca_files()[..2].to_vec(),
            ..on_linux(LinuxFacts {
                sysctl_file: Some(env.sysctl_contents(80)),
                ..ready(&env)
            })
        };
        let root = dir.path().display();
        let plan = plan_uninstall(&env, &facts);
        assert_eq!(
            commands(&plan),
            [
                "systemctl --user stop carte-http.socket carte-https.socket carte.service"
                    .to_string(),
                "systemctl --user disable carte-http.socket carte-https.socket carte.service"
                    .to_string(),
                format!("rm {root}/home/.config/systemd/user/carte-http.socket"),
                format!("rm {root}/home/.config/systemd/user/carte-https.socket"),
                format!("rm {root}/home/.config/systemd/user/carte.service"),
                "systemctl --user daemon-reload".to_string(),
                format!("sudo rm -f {root}/sysctl.d/50-carte.conf"),
                "sudo sysctl -w net.ipv4.ip_unprivileged_port_start=1024".to_string(),
                "sudo sysctl --system".to_string(),
                "sudo rm -f /usr/local/share/ca-certificates/carte-C544AB0012FF34EE.crt"
                    .to_string(),
                "sudo /usr/sbin/update-ca-certificates".to_string(),
                format!("rm -f {root}/state/ca.pem {root}/state/ca-key.pem"),
            ]
        );
        assert!(!plan.notes[0].contains("puma-dev"));

        let set_elsewhere = on_linux(LinuxFacts {
            sysctl_file: Some(env.sysctl_contents(80)),
            port_start: Some(0),
            ..bare()
        });
        assert_eq!(
            commands(&plan_uninstall(&env, &set_elsewhere)),
            [format!("sudo rm -f {root}/sysctl.d/50-carte.conf")]
        );
        assert!(plan_uninstall(&env, &on_linux(bare())).steps.is_empty());

        let mut failed = ready(&env);
        failed.units.get_mut(HTTP_SOCKET).unwrap().state = "failed".into();
        assert_eq!(
            commands(&plan_uninstall(&env, &on_linux(failed)))[..3],
            [
                "systemctl --user stop carte-https.socket carte.service",
                "systemctl --user disable carte-http.socket carte-https.socket carte.service",
                "systemctl --user reset-failed carte-http.socket",
            ]
        );
    }

    #[test]
    fn restarts_for_a_new_ca_when_enabling_leaves_the_daemon_running() {
        let env = linux_env(Path::new("/r"));
        let mut disabled = ready(&env);
        disabled.units.get_mut(HTTP_SOCKET).unwrap().enabled = false;
        let facts = Facts {
            ca: CaState::Replace("it is about to expire".into()),
            ..on_linux(disabled)
        };
        let planned = commands(&plan_install(&env, &facts));
        assert_eq!(
            planned[planned.len() - 3..],
            [
                "systemctl --user enable --now carte-http.socket carte-https.socket carte.service",
                "systemctl --user restart carte.service",
                "verify",
            ]
        );
    }

    #[test]
    fn notes_a_sysctl_file_that_overrides_carte_s() {
        let env = linux_env(Path::new("/r"));
        let overridden = LinuxFacts {
            sysctl_file: Some(env.sysctl_contents(80)),
            port_start_files: vec![
                ("/etc/sysctl.conf".into(), "1024".into()),
                ("/usr/lib/sysctl.d/10-early.conf".into(), "1024".into()),
                ("/etc/sysctl.d/99-rootless.conf".into(), "0".into()),
            ],
            ..bare()
        };
        let plan = plan_install(&env, &on_linux(overridden.clone()));
        assert!(!commands(&plan).iter().any(|c| c.contains("sysctl")));
        assert!(
            plan.notes[0].starts_with(
                "/etc/sysctl.conf sets 1024, after carte's /r/sysctl.d/50-carte.conf, so systemd can't open port 80 for carte"
            ),
            "{:?}",
            plan.notes
        );
        let mut report = Report::default();
        check_units(
            &mut report,
            &env,
            &on_linux(overridden.clone()),
            &overridden,
        );
        assert!(
            report.to_string().contains(
                "net.ipv4.ip_unprivileged_port_start is 1024 (/etc/sysctl.conf sets 1024, after carte's drop-in), so systemd can't open port 80 for carte"
            ),
            "{report}"
        );
    }

    #[test]
    fn removes_the_sysctl_drop_in_when_the_ports_no_longer_need_it() {
        let mut env = linux_env(Path::new("/r"));
        env.machine.http_port = 8080;
        env.machine.https_port = 8443;
        let facts = LinuxFacts {
            sysctl_file: Some("net.ipv4.ip_unprivileged_port_start=80\n".into()),
            port_start: Some(80),
            ..bare()
        };
        assert_eq!(
            commands(&plan_install(&env, &on_linux(facts)))[..3],
            [
                "sudo rm -f /r/sysctl.d/50-carte.conf",
                "sudo sysctl -w net.ipv4.ip_unprivileged_port_start=1024",
                "sudo sysctl --system",
            ]
        );
    }

    #[test]
    fn finds_the_user_managers_unit_directory() {
        let home = Path::new("/home/me");
        let manager = "HOME=/home/me\nXDG_CONFIG_HOME=/data/config\nLANG=C.UTF-8\n";
        assert_eq!(
            systemd_user_dir(home, Some(manager), Some(OsStr::new("/shell/config"))),
            Path::new("/data/config/systemd/user")
        );
        assert_eq!(
            systemd_user_dir(
                home,
                Some("HOME=/home/me\n"),
                Some(OsStr::new("/shell/config"))
            ),
            Path::new("/home/me/.config/systemd/user")
        );
        assert_eq!(
            systemd_user_dir(home, None, Some(OsStr::new("/shell/config"))),
            Path::new("/shell/config/systemd/user")
        );
        assert_eq!(
            systemd_user_dir(home, Some("XDG_CONFIG_HOME=config\n"), None),
            Path::new("/home/me/.config/systemd/user")
        );
        assert_eq!(
            systemd_user_dir(home, None, Some(OsStr::new("relative"))),
            Path::new("/home/me/.config/systemd/user")
        );
        assert_eq!(
            systemd_user_dir(home, Some("XDG_CONFIG_HOME=$'/a b\\n'\n"), None),
            Path::new("/home/me/.config/systemd/user")
        );
    }

    #[test]
    fn reports_units_and_low_ports() {
        let env = linux_env(Path::new("/r"));
        let report = |linux: &LinuxFacts| {
            let mut report = Report::default();
            check_units(&mut report, &env, &on_linux(linux.clone()), linux);
            report.to_string()
        };
        assert_eq!(
            report(&ready(&env)),
            "  ok  system store        /usr/local/share/ca-certificates/carte-C544AB0012FF34EE.crt\n  \
             ok  low ports           net.ipv4.ip_unprivileged_port_start is 80\n  \
             ok  carte-http.socket   /r/home/.config/systemd/user/carte-http.socket, enabled, active\n  \
             ok  carte-https.socket  /r/home/.config/systemd/user/carte-https.socket, enabled, active\n  \
             ok  carte.service       /r/home/.config/systemd/user/carte.service, enabled, active\n"
        );
        let mut outdated = ready(&env);
        outdated.units.get_mut(SERVICE).unwrap().file = Some("old".into());
        outdated.units.get_mut(HTTP_SOCKET).unwrap().file = None;
        outdated.port_start = Some(1024);
        let shown = report(&outdated);
        for expected in [
            "--  low ports           net.ipv4.ip_unprivileged_port_start is 1024, so systemd can't open port 80 for carte\n",
            "--  carte-http.socket   /r/home/.config/systemd/user/carte-http.socket is missing\n",
            "!!  carte.service       /r/home/.config/systemd/user/carte.service differs from what setup writes now; enabled, active\n",
        ] {
            assert!(shown.contains(expected), "missing {expected:?} in\n{shown}");
        }
        let no_systemd = report(&LinuxFacts {
            systemd: false,
            ..bare()
        });
        assert!(
            no_systemd.contains("`systemctl --user` doesn't answer"),
            "{no_systemd}"
        );
    }

    #[test]
    fn parses_systemctl_show() {
        let text = "Id=carte-http.socket\nActiveState=active\nUnitFileState=enabled\n\n\
                    Id=carte.service\nActiveState=inactive\nUnitFileState=\n";
        let units = parse_show(text);
        assert_eq!(units["carte-http.socket"]["ActiveState"], "active");
        assert_eq!(units["carte.service"]["UnitFileState"], "");
        assert_eq!(units.len(), 2);
        assert_eq!(parse_main_pid("4242\n"), Some(4242));
        assert_eq!(parse_main_pid("0\n"), None);
        assert_eq!(parse_main_pid(""), None);
    }

    #[test]
    fn parses_ss_output() {
        let output = "LISTEN 0      4096       127.0.0.1:80        0.0.0.0:*    users:((\"systemd\",pid=1201,fd=31),(\"carte\",pid=1300,fd=3))\n\
                      LISTEN 0      4096           [::1]:80           [::]:*    users:((\"systemd\",pid=1201,fd=32))\n\
                      LISTEN 0      511          0.0.0.0:443        0.0.0.0:*    users:((\"nginx\",pid=77,fd=6),(\"nginx\",pid=76,fd=6))\n\
                      UNCONN 0      0          127.0.0.1:9253       0.0.0.0:*\n";
        let user = |command: &str, pid, address: &str| PortUser {
            command: command.into(),
            pid,
            address: address.into(),
        };
        assert_eq!(
            parse_ss(output),
            [
                user("carte", 1300, "127.0.0.1:80"),
                user("systemd", 1201, "[::1]:80"),
                user("nginx", 77, "0.0.0.0:443"),
                user("nginx", 76, "0.0.0.0:443"),
                user("unknown", 0, "127.0.0.1:9253"),
            ]
        );
        assert!(parse_ss("").is_empty());
    }

    #[test]
    fn reads_small_files_from_proc_and_sysctl() {
        let if_inet6 = "fe800000000000000000000000000001 02 40 20 80     eth0\n\
                        00000000000000000000000000000001 01 80 10 80       lo\n";
        assert!(parse_if_inet6(if_inet6));
        assert!(!parse_if_inet6(
            "fe800000000000000000000000000001 02 40 20 80     eth0\n"
        ));
        let env = linux_env(Path::new("/r"));
        assert_eq!(
            parse_sysctl(&env.sysctl_contents(80), PORT_START_KEY).as_deref(),
            Some("80")
        );
        assert_eq!(parse_sysctl("# x = 1\n", PORT_START_KEY), None);
    }
}
