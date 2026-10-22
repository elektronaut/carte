//! DNS for a TLD other than `.localhost` on Linux, through systemd-resolved: a DNS
//! delegate from systemd 258, which routes only carte's TLD to its DNS server, or on
//! older versions a drop-in that does the same where that leaves other lookups alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::check::{Report, Status};
use super::{Plan, SetupEnv, Step};

pub const DELEGATE_DIR: &str = "/etc/systemd/dns-delegate.d";
pub const DROP_IN_DIR: &str = "/etc/systemd/resolved.conf.d";

/// How carte's files start; one without it isn't carte's, whatever its name.
const HEADER: &str = "# Written by `carte setup`";

/// The first systemd with DNS delegates.
const DELEGATES_SINCE: u32 = 258;

/// A file of carte's that sends a TLD's lookups to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsFile {
    Delegate,
    DropIn,
}

impl DnsFile {
    pub fn path(&self, tld: &str) -> PathBuf {
        match self {
            DnsFile::Delegate => Path::new(DELEGATE_DIR).join(format!("carte-{tld}.dns-delegate")),
            DnsFile::DropIn => Path::new(DROP_IN_DIR).join(format!("carte-{tld}.conf")),
        }
    }

    pub fn contents(&self, tld: &str, port: u16) -> String {
        let header = format!("{HEADER}: sends .{tld} lookups to carte's DNS server.\n");
        match self {
            DnsFile::Delegate => {
                format!("{header}[Delegate]\nDNS=127.0.0.1:{port}\nDomains={tld}\n")
            }
            DnsFile::DropIn => format!("{header}[Resolve]\nDNS=127.0.0.1:{port}\nDomains=~{tld}\n"),
        }
    }
}

/// What `resolvectl status` says about where lookups go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routing {
    /// The global DNS servers.
    pub global: Vec<String>,
    /// The global search and routing domains.
    pub global_domains: Vec<String>,
    /// Whether a link that is a default route has DNS servers.
    pub default_route: bool,
}

/// systemd-resolved and carte's files for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsFacts {
    /// Whether systemd-resolved answers `resolvectl status`.
    pub resolved: bool,
    pub systemd_version: Option<u32>,
    pub routing: Routing,
    /// Whether `/etc/resolv.conf` sends lookups to systemd-resolved's stub.
    pub stub: bool,
    /// carte's files, by TLD.
    pub files: BTreeMap<String, (DnsFile, String)>,
    /// Files with carte's names that don't start with its header.
    pub unowned: Vec<PathBuf>,
}

impl DnsFacts {
    /// Whether a global drop-in routes only its TLD to carte: with global servers it
    /// would replace or join them, global domains would go to carte too, and without
    /// a default-route link nothing else would answer. What carte's own drop-ins add
    /// to the global settings doesn't count; another server at carte's address does.
    pub fn drop_in_is_safe(&self) -> bool {
        let mut servers = Vec::new();
        let mut domains = Vec::new();
        for (kind, text) in self.files.values() {
            if *kind == DnsFile::DropIn {
                servers.extend(setting(text, "DNS"));
                domains.extend(setting(text, "Domains"));
            }
        }
        let routing = &self.routing;
        routing.default_route
            && routing.global.iter().all(|server| servers.contains(server))
            && routing
                .global_domains
                .iter()
                .all(|domain| domains.contains(domain))
    }

    pub fn gather() -> Self {
        let output = |program: &str, args: &[&str]| {
            cfg!(target_os = "linux")
                .then(|| {
                    Command::new(program)
                        .args(args)
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                })
                .flatten()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let status = output("resolvectl", &["status", "--no-pager"]);
        let mut files = BTreeMap::new();
        let mut unowned = Vec::new();
        for (kind, dir, prefix, suffix) in [
            (DnsFile::DropIn, DROP_IN_DIR, "carte-", ".conf"),
            (DnsFile::Delegate, DELEGATE_DIR, "carte-", ".dns-delegate"),
        ] {
            for entry in fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(tld) = name
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                else {
                    continue;
                };
                match fs::read_to_string(entry.path()) {
                    Ok(text) if text.starts_with(HEADER) => {
                        files.insert(tld.to_string(), (kind.clone(), text));
                    }
                    _ => unowned.push(entry.path()),
                }
            }
        }
        DnsFacts {
            resolved: status.is_some(),
            systemd_version: output("systemctl", &["--version"])
                .and_then(|text| parse_systemd_version(&text)),
            routing: status.as_deref().map(parse_resolvectl).unwrap_or_default(),
            stub: fs::read_to_string("/etc/resolv.conf").is_ok_and(|text| uses_stub(&text)),
            files,
            unowned,
        }
    }

    pub fn delegates(&self) -> bool {
        self.systemd_version
            .is_some_and(|version| version >= DELEGATES_SINCE)
    }
}

/// The version in the first line of `systemctl --version`, as in `systemd 257 (257.9-1)`.
pub fn parse_systemd_version(text: &str) -> Option<u32> {
    let mut words = text.lines().next()?.split_whitespace();
    (words.next()? == "systemd").then_some(())?;
    let version = words.next()?;
    let digits = version
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(version.len());
    version[..digits].parse().ok()
}

/// The global servers and default-route links in `resolvectl status`.
pub fn parse_resolvectl(text: &str) -> Routing {
    let mut sections: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    let mut last_key = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) && !line.contains(": ") {
            sections.push((line.trim().to_string(), BTreeMap::new()));
            continue;
        }
        let Some((_, props)) = sections.last_mut() else {
            continue;
        };
        if let Some((key, value)) = line.split_once(": ") {
            last_key = key.trim().to_string();
            props.insert(last_key.clone(), value.trim().to_string());
        } else if let Some(value) = props.get_mut(&last_key) {
            value.push(' ');
            value.push_str(line.trim());
        }
    }
    let servers = |props: &BTreeMap<String, String>| -> Vec<String> {
        props
            .get("DNS Servers")
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    };
    let mut routing = Routing::default();
    for (name, props) in &sections {
        if name == "Global" {
            routing.global = servers(props);
            routing.global_domains = props
                .get("DNS Domain")
                .map(|v| v.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
        } else if name.starts_with("Link ") {
            let default_route = props
                .get("Protocols")
                .is_some_and(|p| p.split_whitespace().any(|w| w == "+DefaultRoute"))
                || props
                    .get("DefaultRoute setting")
                    .is_some_and(|v| v == "yes")
                || props.get("Default Route").is_some_and(|v| v == "yes");
            if default_route && !servers(props).is_empty() {
                routing.default_route = true;
            }
        }
    }
    routing
}

/// The values a unit-file style setting has, split on whitespace.
fn setting(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .flat_map(|(_, value)| value.split_whitespace().map(str::to_string))
        .collect()
}

/// Whether `/etc/resolv.conf` names systemd-resolved's stub resolver.
pub fn uses_stub(text: &str) -> bool {
    text.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("nameserver")
            && matches!(words.next(), Some("127.0.0.53" | "127.0.0.54"))
    })
}

/// The file that should send the TLD to carte here, or why there is none.
fn wanted(env: &SetupEnv, dns: &DnsFacts) -> Result<DnsFile, String> {
    let tld = &env.machine.tld;
    let port = env.machine.dns_port;
    if !dns.resolved {
        return Err(format!(
            "systemd-resolved isn't running, so setup can't send .{tld} lookups to carte; point \
             them at 127.0.0.1 port {port} yourself, such as with dnsmasq's \
             `server=/{tld}/127.0.0.1#{port}`"
        ));
    }
    if dns.delegates() {
        return Ok(DnsFile::Delegate);
    }
    if dns.drop_in_is_safe() {
        return Ok(DnsFile::DropIn);
    }
    Err(format!(
        "systemd-resolved before 258 can only send .{tld} lookups to carte through its global \
         servers, which here would take over other lookups too; add DNS=127.0.0.1:{port} and \
         Domains=~{tld} to the network connection whose DNS servers you use (for \
         NetworkManager: `nmcli connection modify <name> +ipv4.dns 127.0.0.1:{port} \
         +ipv4.dns-search ~{tld}`), or use dnsmasq's `server=/{tld}/127.0.0.1#{port}`"
    ))
}

/// Has systemd-resolved read its files again. Whether it can reload varies by version
/// and distribution (Ubuntu's 255 can't), so this reloads where it can and restarts
/// elsewhere, which keeps the links' DNS settings.
fn reload() -> Step {
    Step::sudo(
        "Have systemd-resolved read its configuration again",
        "systemctl",
        &["reload-or-restart", "systemd-resolved"],
    )
}

/// The steps that send the TLD to carte and take out carte's files for other TLDs.
pub(super) fn install_steps(env: &SetupEnv, dns: &DnsFacts, plan: &mut Plan) {
    let tld = &env.machine.tld;
    let port = env.machine.dns_port;
    let wanted = env.needs_resolver().then(|| wanted(env, dns));
    let keep = match &wanted {
        Some(Ok(kind)) => Some(kind),
        _ => None,
    };
    let mut changed = false;
    for (file_tld, (kind, _)) in &dns.files {
        if file_tld == tld && keep == Some(kind) {
            continue;
        }
        let why = if file_tld != tld {
            format!("carte serves .{tld}, not .{file_tld}")
        } else if let Some(keep) = keep {
            format!("setup writes {} instead", keep.path(tld).display())
        } else if !dns.resolved {
            "systemd-resolved isn't running".to_string()
        } else if env.needs_resolver() {
            "with this machine's other DNS servers it sends other lookups to carte too".to_string()
        } else {
            ".localhost resolves without it".to_string()
        };
        plan.steps.push(Step::sudo(
            format!("Remove {}: {why}", kind.path(file_tld).display()),
            "rm",
            &["-f", &kind.path(file_tld).to_string_lossy()],
        ));
        changed = true;
    }
    match wanted {
        Some(Ok(kind)) => {
            let contents = kind.contents(tld, port);
            if dns.files.get(tld).map(|(k, text)| (k, text.as_str())) != Some((&kind, &contents)) {
                let path = kind.path(tld);
                if dns.unowned.contains(&path) {
                    plan.notes.push(format!(
                        "{} wasn't written by carte; setup replaces it",
                        path.display()
                    ));
                }
                if let Some(dir) = path.parent() {
                    plan.steps.push(Step::sudo(
                        format!("Create {}", dir.display()),
                        "mkdir",
                        &["-p", &dir.to_string_lossy()],
                    ));
                }
                plan.steps.push(
                    Step::sudo(
                        format!("Send .{tld} lookups to carte's DNS server on port {port}"),
                        "tee",
                        &[&path.to_string_lossy()],
                    )
                    .with_stdin(contents),
                );
                changed = true;
            }
        }
        Some(Err(note)) => plan.notes.push(note),
        None => {}
    }
    if changed && dns.resolved {
        plan.steps.push(reload());
    }
}

/// The steps that take out all of carte's files.
pub(super) fn uninstall_steps(dns: &DnsFacts, plan: &mut Plan) {
    for (tld, (kind, _)) in &dns.files {
        plan.steps.push(Step::sudo(
            format!("Stop sending .{tld} lookups to carte"),
            "rm",
            &["-f", &kind.path(tld).to_string_lossy()],
        ));
    }
    if !dns.files.is_empty() && dns.resolved {
        plan.steps.push(reload());
    }
}

/// The DNS lines in `carte setup --check`.
pub(super) fn check(report: &mut Report, env: &SetupEnv, dns: &DnsFacts) {
    let tld = &env.machine.tld;
    let port = env.machine.dns_port;
    if !env.needs_resolver() {
        report.push(
            Status::Ok,
            "resolver",
            ".localhost resolves to loopback without one",
        );
    } else if !dns.resolved {
        report.push(
            Status::Missing,
            "resolver",
            format!(
                "systemd-resolved isn't running; point .{tld} lookups at 127.0.0.1 port {port} yourself"
            ),
        );
    } else {
        match dns.files.get(tld) {
            None => report.push(
                Status::Missing,
                "resolver",
                match wanted(env, dns) {
                    Ok(kind) if dns.unowned.contains(&kind.path(tld)) => format!(
                        "{} isn't carte's, as it lacks carte's header; `carte setup` replaces it",
                        kind.path(tld).display()
                    ),
                    Ok(kind) => format!("{} is missing", kind.path(tld).display()),
                    Err(_) => format!(
                        "nothing sends .{tld} lookups to carte; `carte setup` says what to configure"
                    ),
                },
            ),
            Some((kind, text)) if *text != kind.contents(tld, port) => report.push(
                Status::Warn,
                "resolver",
                format!(
                    "{} differs from what setup writes now",
                    kind.path(tld).display()
                ),
            ),
            Some((DnsFile::DropIn, _)) if !dns.drop_in_is_safe() => report.push(
                Status::Warn,
                "resolver",
                format!(
                    "{} makes carte's DNS server a global one, and with this machine's other \
                     DNS servers that sends other lookups to it, or .{tld} lookups elsewhere",
                    DnsFile::DropIn.path(tld).display()
                ),
            ),
            Some((kind, _)) => report.push(
                Status::Ok,
                "resolver",
                format!(
                    "{} sends .{tld} to 127.0.0.1 port {port}",
                    kind.path(tld).display()
                ),
            ),
        }
        if !dns.stub {
            report.push(
                Status::Warn,
                "resolver",
                "/etc/resolv.conf doesn't name systemd-resolved's stub (127.0.0.53), so \
                 programs reading it skip carte's DNS",
            );
        }
    }
    for (file_tld, (kind, _)) in &dns.files {
        if !env.needs_resolver() || file_tld != tld {
            report.push(
                Status::Warn,
                "resolver",
                format!(
                    "{} isn't needed for .{tld}; `carte setup` removes it",
                    kind.path(file_tld).display()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::linux::LinuxFacts;
    use crate::setup::linux::tests::{on_linux, ready};
    use crate::setup::tests::{commands, env, with_test_tld};
    use crate::setup::{
        Action, CaState, Facts, Platform, plan_install, plan_uninstall, previous_tlds,
    };

    const STATUS_257: &str = "Global
           Protocols: -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
    resolv.conf mode: stub

Link 2 (eth0)
    Current Scopes: DNS
         Protocols: +DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
Current DNS Server: 10.0.2.3
       DNS Servers: 10.0.2.3 fd00::3
                    192.168.1.1
";

    const STATUS_GLOBAL: &str = "Global
           Protocols: -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
    resolv.conf mode: foreign
Current DNS Server: 192.168.1.1
       DNS Servers: 192.168.1.1
        DNS Domain: corp.example

Link 2 (eth0)
    Current Scopes: none
         Protocols: -DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
";

    fn linux_env() -> SetupEnv {
        SetupEnv {
            platform: Platform::Linux,
            ..with_test_tld(env(Path::new("/r")))
        }
    }

    fn dns(version: u32, status: &str) -> DnsFacts {
        DnsFacts {
            resolved: true,
            systemd_version: Some(version),
            routing: parse_resolvectl(status),
            stub: true,
            files: BTreeMap::new(),
            unowned: Vec::new(),
        }
    }

    fn with_dns(env: &SetupEnv, dns: DnsFacts) -> Facts {
        Facts {
            ca: CaState::Trusted,
            ..on_linux(LinuxFacts { dns, ..ready(env) })
        }
    }

    fn file(kind: DnsFile, tld: &str) -> (String, (DnsFile, String)) {
        let text = kind.contents(tld, 9253);
        (tld.to_string(), (kind, text))
    }

    #[test]
    fn writes_a_dns_delegate_from_systemd_258() {
        let env = linux_env();
        let plan = plan_install(&env, &with_dns(&env, dns(258, STATUS_GLOBAL)));
        assert_eq!(
            commands(&plan),
            [
                "sudo mkdir -p /etc/systemd/dns-delegate.d",
                "sudo tee /etc/systemd/dns-delegate.d/carte-test.dns-delegate",
                "sudo systemctl reload-or-restart systemd-resolved",
                "verify",
            ]
        );
        let Action::Run { stdin, .. } = &plan.steps[1].action else {
            panic!()
        };
        assert_eq!(
            stdin.as_deref(),
            Some(
                "# Written by `carte setup`: sends .test lookups to carte's DNS server.\n\
                 [Delegate]\nDNS=127.0.0.1:9253\nDomains=test\n"
            )
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let done = DnsFacts {
            files: [file(DnsFile::Delegate, "test")].into(),
            ..dns(258, STATUS_GLOBAL)
        };
        assert_eq!(
            commands(&plan_install(&env, &with_dns(&env, done))),
            ["verify"]
        );
    }

    #[test]
    fn writes_a_drop_in_before_258_only_where_it_is_safe() {
        let env = linux_env();
        let plan = plan_install(&env, &with_dns(&env, dns(257, STATUS_257)));
        assert_eq!(
            commands(&plan),
            [
                "sudo mkdir -p /etc/systemd/resolved.conf.d",
                "sudo tee /etc/systemd/resolved.conf.d/carte-test.conf",
                "sudo systemctl reload-or-restart systemd-resolved",
                "verify",
            ]
        );
        let Action::Run { stdin, .. } = &plan.steps[1].action else {
            panic!()
        };
        assert!(
            stdin
                .as_deref()
                .unwrap()
                .ends_with("[Resolve]\nDNS=127.0.0.1:9253\nDomains=~test\n")
        );

        let plan = plan_install(&env, &with_dns(&env, dns(257, STATUS_GLOBAL)));
        assert_eq!(commands(&plan), ["verify"]);
        assert!(
            plan.notes[0].contains("systemd-resolved before 258")
                && plan.notes[0].contains("server=/test/127.0.0.1#9253"),
            "{:?}",
            plan.notes
        );

        let none = DnsFacts::default();
        let plan = plan_install(&env, &with_dns(&env, none));
        assert_eq!(commands(&plan), ["verify"]);
        assert!(
            plan.notes[0].starts_with("systemd-resolved isn't running"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn replaces_a_drop_in_with_a_delegate_and_removes_other_tlds() {
        let env = linux_env();
        let facts = DnsFacts {
            files: [
                file(DnsFile::DropIn, "test"),
                file(DnsFile::Delegate, "dev"),
            ]
            .into(),
            ..dns(258, STATUS_257)
        };
        let plan = plan_install(&env, &with_dns(&env, facts.clone()));
        assert_eq!(
            commands(&plan),
            [
                "sudo rm -f /etc/systemd/dns-delegate.d/carte-dev.dns-delegate",
                "sudo rm -f /etc/systemd/resolved.conf.d/carte-test.conf",
                "sudo mkdir -p /etc/systemd/dns-delegate.d",
                "sudo tee /etc/systemd/dns-delegate.d/carte-test.dns-delegate",
                "sudo systemctl reload-or-restart systemd-resolved",
                "verify",
            ]
        );
        assert_eq!(
            plan.steps[0].description,
            "Remove /etc/systemd/dns-delegate.d/carte-dev.dns-delegate: carte serves .test, not .dev"
        );

        let mut localhost = env.clone();
        localhost.machine.tld = "localhost".into();
        let facts = with_dns(&localhost, facts);
        assert_eq!(
            commands(&plan_install(&localhost, &facts)),
            [
                "sudo rm -f /etc/systemd/dns-delegate.d/carte-dev.dns-delegate",
                "sudo rm -f /etc/systemd/resolved.conf.d/carte-test.conf",
                "sudo systemctl reload-or-restart systemd-resolved",
                "verify",
            ]
        );
        assert_eq!(previous_tlds(&localhost, &facts), ["dev", "test"]);
        let uninstall = commands(&plan_uninstall(&localhost, &facts));
        let first = uninstall
            .iter()
            .position(|c| c.contains("carte-dev"))
            .unwrap();
        assert_eq!(
            uninstall[first..first + 3],
            [
                "sudo rm -f /etc/systemd/dns-delegate.d/carte-dev.dns-delegate",
                "sudo rm -f /etc/systemd/resolved.conf.d/carte-test.conf",
                "sudo systemctl reload-or-restart systemd-resolved",
            ]
        );
    }

    #[test]
    fn removes_files_without_reloading_a_resolved_that_isnt_running() {
        let env = linux_env();
        let facts = DnsFacts {
            files: [file(DnsFile::DropIn, "test")].into(),
            ..DnsFacts::default()
        };
        let plan = plan_install(&env, &with_dns(&env, facts.clone()));
        assert_eq!(
            commands(&plan),
            [
                "sudo rm -f /etc/systemd/resolved.conf.d/carte-test.conf",
                "verify"
            ]
        );
        assert_eq!(
            plan.steps[0].description,
            "Remove /etc/systemd/resolved.conf.d/carte-test.conf: systemd-resolved isn't running"
        );
        let uninstall = commands(&plan_uninstall(&env, &with_dns(&env, facts)));
        assert!(
            uninstall.contains(&"sudo rm -f /etc/systemd/resolved.conf.d/carte-test.conf".into())
        );
        assert!(!uninstall.iter().any(|c| c.contains("systemd-resolved")));
    }

    #[test]
    fn replaces_a_file_with_carte_s_name_that_isnt_carte_s() {
        let env = linux_env();
        let path = DnsFile::Delegate.path("test");
        let facts = DnsFacts {
            unowned: vec![path.clone()],
            ..dns(258, STATUS_GLOBAL)
        };
        let plan = plan_install(&env, &with_dns(&env, facts.clone()));
        assert!(commands(&plan).contains(&format!("sudo tee {}", path.display())));
        assert_eq!(
            plan.notes,
            [
                "/etc/systemd/dns-delegate.d/carte-test.dns-delegate wasn't written by carte; setup replaces it"
            ]
        );
        let mut report = Report::default();
        check(&mut report, &env, &facts);
        assert!(
            report.to_string().contains(
                "--  resolver  /etc/systemd/dns-delegate.d/carte-test.dns-delegate isn't carte's, as it lacks carte's header; `carte setup` replaces it\n"
            ),
            "{report}"
        );
        assert!(plan_uninstall(&env, &with_dns(&env, facts)).steps.iter().all(|s| {
            !matches!(&s.action, Action::Run { command, .. } if command.args.iter().any(|a| a.contains("carte-test")))
        }));
    }

    #[test]
    fn reports_the_resolver() {
        let env = linux_env();
        let report = |dns: &DnsFacts| {
            let mut report = Report::default();
            check(&mut report, &env, dns);
            report.to_string()
        };
        let delegate = DnsFacts {
            files: [file(DnsFile::Delegate, "test")].into(),
            ..dns(258, STATUS_GLOBAL)
        };
        assert_eq!(
            report(&delegate),
            "  ok  resolver  /etc/systemd/dns-delegate.d/carte-test.dns-delegate sends .test to 127.0.0.1 port 9253\n"
        );
        assert!(report(&dns(258, STATUS_GLOBAL)).contains(
            "--  resolver  /etc/systemd/dns-delegate.d/carte-test.dns-delegate is missing\n"
        ));
        let unsafe_drop_in = DnsFacts {
            files: [file(DnsFile::DropIn, "test")].into(),
            stub: false,
            ..dns(257, STATUS_GLOBAL)
        };
        let shown = report(&unsafe_drop_in);
        assert!(
            shown.contains("!!  resolver  /etc/systemd/resolved.conf.d/carte-test.conf makes carte's DNS server a global one"),
            "{shown}"
        );
        assert!(
            shown.contains("/etc/resolv.conf doesn't name systemd-resolved's stub"),
            "{shown}"
        );
        assert!(
            report(&DnsFacts::default()).contains("--  resolver  systemd-resolved isn't running")
        );
    }

    #[test]
    fn reads_resolvectl_and_systemctl() {
        assert_eq!(
            parse_resolvectl(STATUS_257),
            Routing {
                global: Vec::new(),
                global_domains: Vec::new(),
                default_route: true,
            }
        );
        assert_eq!(
            parse_resolvectl(STATUS_GLOBAL),
            Routing {
                global: vec!["192.168.1.1".into()],
                global_domains: vec!["corp.example".into()],
                default_route: false,
            }
        );
        let older = "Global\n       LLMNR setting: no\n\nLink 2 (eth0)\nDefaultRoute setting: yes\n         DNS Servers: 10.0.2.3\n";
        assert!(parse_resolvectl(older).default_route);
        let routed = |global: &[&str], domains: &[&str], files| DnsFacts {
            routing: Routing {
                global: global.iter().map(|s| s.to_string()).collect(),
                global_domains: domains.iter().map(|s| s.to_string()).collect(),
                default_route: true,
            },
            files,
            ..DnsFacts::default()
        };
        assert!(routed(&[], &[], BTreeMap::new()).drop_in_is_safe());
        assert!(
            !routed(&["127.0.0.1:9253"], &[], BTreeMap::new()).drop_in_is_safe(),
            "another server at carte's address isn't carte's"
        );
        assert!(
            routed(
                &["127.0.0.1:9253"],
                &["~test"],
                [file(DnsFile::DropIn, "test")].into()
            )
            .drop_in_is_safe()
        );
        let old_port = (
            "test".to_string(),
            (DnsFile::DropIn, DnsFile::DropIn.contents("test", 5353)),
        );
        assert!(
            routed(&["127.0.0.1:5353"], &["~test"], [old_port].into()).drop_in_is_safe(),
            "carte's drop-in for another port doesn't count"
        );
        assert!(!routed(&["127.0.0.1:5353"], &[], BTreeMap::new()).drop_in_is_safe());
        assert!(!routed(&[], &["corp.example"], BTreeMap::new()).drop_in_is_safe());
        assert!(!dns(257, STATUS_GLOBAL).drop_in_is_safe());
        assert_eq!(
            parse_systemd_version("systemd 258~rc1 (258~rc1-1)\n"),
            Some(258)
        );

        assert_eq!(
            parse_systemd_version("systemd 257 (257.9-1~deb13u1)\n+PAM +AUDIT\n"),
            Some(257)
        );
        assert_eq!(
            parse_systemd_version("systemd 258 (258.1-1.fc43)\n"),
            Some(258)
        );
        assert_eq!(parse_systemd_version("other 1\n"), None);
        assert!(uses_stub(
            "# managed\nnameserver 127.0.0.53\noptions edns0 trust-ad\n"
        ));
        assert!(!uses_stub("nameserver 192.168.1.1\n"));
    }
}
