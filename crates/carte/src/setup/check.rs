//! Read-only checks: what `carte setup --check` reports and how setup verifies itself.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::prompt::tilde;

use super::plist::{LABEL, OLD_LABEL};
use super::{CaState, Facts, PUMA_DEV_LABEL, SetupEnv};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Missing,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub status: Status,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<Line>,
}

impl Report {
    fn push(&mut self, status: Status, label: &str, detail: impl Into<String>) {
        self.lines.push(Line {
            status,
            label: label.to_string(),
            detail: detail.into(),
        });
    }

    pub fn ok(&self) -> bool {
        !self.lines.iter().any(|l| l.status == Status::Missing)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self.lines.iter().map(|l| l.label.len()).max().unwrap_or(0);
        for line in &self.lines {
            let mark = match line.status {
                Status::Ok => "ok",
                Status::Warn => "!!",
                Status::Missing => "--",
                Status::Info => "  ",
            };
            writeln!(f, "  {mark}  {:width$}  {}", line.label, line.detail)?;
        }
        Ok(())
    }
}

/// A listening socket as `lsof` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortUser {
    pub command: String,
    pub pid: u32,
    pub address: String,
}

impl PortUser {
    fn is_carte(&self) -> bool {
        self.command == "carte"
    }

    fn loopback_only(&self) -> bool {
        self.address.starts_with("127.") || self.address.starts_with("[::1]")
    }
}

impl fmt::Display for PortUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (pid {}) on {}", self.command, self.pid, self.address)
    }
}

/// Parses `lsof -F pcn` output.
pub fn parse_lsof(output: &str) -> Vec<PortUser> {
    let mut users = Vec::new();
    let (mut pid, mut command) = (0, String::new());
    for line in output.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => command = value.to_string(),
            "n" => {
                let user = PortUser {
                    command: command.clone(),
                    pid,
                    address: value.to_string(),
                };
                if !users.contains(&user) {
                    users.push(user);
                }
            }
            _ => {}
        }
    }
    users
}

/// Who listens on `port` (`TCP`) or has it bound (`UDP`); empty when lsof isn't there.
pub fn port_users(protocol: &str, port: u16) -> Vec<PortUser> {
    let mut command = Command::new("lsof");
    command.args(["-nP", &format!("-i{protocol}:{port}"), "-Fpcn"]);
    if protocol == "TCP" {
        command.arg("-sTCP:LISTEN");
    }
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| parse_lsof(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
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

/// The `nameserver` and `port` of a resolver file.
pub fn parse_resolver(text: &str) -> (Option<&str>, Option<u16>) {
    let mut nameserver = None;
    let mut port = None;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("nameserver"), Some(value)) if nameserver.is_none() => nameserver = Some(value),
            (Some("port"), Some(value)) => port = value.parse().ok(),
            _ => {}
        }
    }
    (nameserver, port)
}

/// `carte setup --check`.
pub fn check(env: &SetupEnv, facts: &Facts) -> Report {
    let mut report = Report::default();
    let file = tilde(&env.resolver_file());
    match &facts.resolver {
        None => report.push(Status::Missing, "resolver", format!("{file} is missing")),
        Some(text) => {
            let by = if text.contains("puma-dev") {
                " (written by puma-dev)"
            } else {
                ""
            };
            match parse_resolver(text) {
                (Some("127.0.0.1"), Some(port)) if port == env.machine.dns_port => report.push(
                    Status::Ok,
                    "resolver",
                    format!("{file} points at 127.0.0.1 port {port}{by}"),
                ),
                (nameserver, port) => report.push(
                    Status::Warn,
                    "resolver",
                    format!(
                        "{file} points at {} port {}{by}; carte's DNS is 127.0.0.1 port {}",
                        nameserver.unwrap_or("?"),
                        port.map_or("53".to_string(), |p| p.to_string()),
                        env.machine.dns_port
                    ),
                ),
            }
        }
    }

    let ca = tilde(&env.ca_cert());
    match &facts.ca {
        CaState::Missing => report.push(Status::Missing, "CA", format!("{ca} doesn't exist yet")),
        CaState::Replace(why) => report.push(
            Status::Warn,
            "CA",
            format!("{ca} will be replaced when the daemon starts: {why}"),
        ),
        CaState::Untrusted => report.push(Status::Missing, "CA", format!("{ca} is not trusted")),
        CaState::Trusted => report.push(Status::Ok, "CA", format!("{ca} is trusted")),
        CaState::Unknown(err) => report.push(
            Status::Warn,
            "CA",
            format!("couldn't check whether {ca} is trusted: {err}"),
        ),
    }

    let plist = tilde(&env.agent_plist());
    let loaded = if facts.agent_loaded {
        "loaded"
    } else {
        "not loaded"
    };
    match &facts.agent_plist {
        None if facts.agent_loaded => report.push(
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
            if facts.agent_loaded {
                Status::Ok
            } else {
                Status::Missing
            },
            "LaunchAgent",
            format!("{plist}, {loaded}"),
        ),
    }
    if facts.old_agent.installed() {
        report.push(
            Status::Warn,
            "LaunchAgent",
            format!(
                "{} is installed under its old label; `carte setup` replaces it",
                if facts.old_agent.plist {
                    tilde(&env.old_agent_plist())
                } else {
                    OLD_LABEL.to_string()
                }
            ),
        );
    }
    report.push(
        if facts.daemon_running {
            Status::Ok
        } else {
            Status::Missing
        },
        "daemon",
        if facts.daemon_running {
            format!("answering on {}", tilde(&env.paths.socket()))
        } else {
            "not running".to_string()
        },
    );

    let puma_dev = &facts.puma_dev;
    if puma_dev.installed() || puma_dev.links > 0 {
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

    let machine = &env.machine;
    for (label, protocol, port) in [
        ("http port", "TCP", machine.http_port),
        ("https port", "TCP", machine.https_port),
        ("dns port", "UDP", machine.dns_port),
    ] {
        port_line(&mut report, label, port, &port_users(protocol, port));
    }
    report
}

fn port_line(report: &mut Report, label: &str, port: u16, users: &[PortUser]) {
    let list = |users: &[PortUser]| {
        users
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    };
    let label = format!("{label} {port}");
    if users.is_empty() {
        report.push(Status::Missing, &label, "nothing listening");
    } else if users.iter().all(PortUser::is_carte) {
        let status = if users.iter().all(PortUser::loopback_only) {
            Status::Ok
        } else {
            Status::Warn
        };
        report.push(status, &label, list(users));
    } else {
        report.push(Status::Warn, &label, list(users));
    }
}

/// Waits for the daemon after setup started it, then checks that it answers on its
/// socket, its ports and through DNS.
pub fn verify(env: &SetupEnv) -> Report {
    let socket = env.paths.socket();
    let deadline = Instant::now() + Duration::from_secs(15);
    while UnixStream::connect(&socket).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut report = Report::default();
    match UnixStream::connect(&socket) {
        Ok(_) => report.push(
            Status::Ok,
            "daemon",
            format!("answering on {}", tilde(&socket)),
        ),
        Err(err) => report.push(
            Status::Missing,
            "daemon",
            format!(
                "no answer on {} ({err}); see {}",
                tilde(&socket),
                tilde(&env.agent().log_path())
            ),
        ),
    }

    let machine = &env.machine;
    for (label, port) in [
        ("http port", machine.http_port),
        ("https port", machine.https_port),
    ] {
        let users = port_users("TCP", port);
        port_line(&mut report, label, port, &users);
        if users.iter().any(|u| u.is_carte() && !u.loopback_only()) {
            report.push(
                Status::Warn,
                "",
                "launchd listens on every interface: carte refuses connections from other \
                 machines, but they can still open TCP connections to this port",
            );
        }
    }

    let host = format!("carte.{}", machine.tld);
    match http_status_line(SocketAddr::from(([127, 0, 0, 1], machine.http_port)), &host) {
        Ok(line) => report.push(Status::Ok, "http", format!("http://{host} answered {line}")),
        Err(err) => report.push(Status::Missing, "http", format!("http://{host}: {err}")),
    }

    let name = format!("carte-check.{}", machine.tld);
    match (name.as_str(), 80).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            if addrs.iter().any(|a| a.ip().is_loopback()) {
                report.push(Status::Ok, "dns", format!("{name} resolves to loopback"));
            } else {
                report.push(
                    Status::Missing,
                    "dns",
                    format!("{name} resolves to {addrs:?}, not loopback"),
                );
            }
        }
        Err(err) => report.push(Status::Missing, "dns", format!("{name}: {err}")),
    }
    report
}

fn http_status_line(addr: SocketAddr, host: &str) -> std::io::Result<String> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    write!(stream, "GET / HTTP/1.0\r\nHost: {host}\r\n\r\n")?;
    let mut buf = [0; 256];
    let n = stream.read(&mut buf)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let line = text.lines().next().unwrap_or_default();
    if line.starts_with("HTTP/1.") {
        Ok(line.to_string())
    } else {
        Err(std::io::Error::other(format!(
            "not an HTTP response: {line:?}"
        )))
    }
}

/// What to do when the daemon didn't come up behind launchd's sockets.
pub fn fallback(env: &SetupEnv) -> String {
    format!(
        "- `launchctl print gui/{uid}/{LABEL}` shows whether launchd started carte and why it stopped; \
         its log is {log}.\n\
         - If launchd didn't hand over ports {http} and {https} on 127.0.0.1, carte can still run on \
         high ports: set http_port = 8080 and https_port = 8443 in {config} and run `carte setup` \
         again; URLs then carry the port.\n\
         - `carte setup --uninstall` undoes these changes.",
        uid = env.uid,
        log = tilde(&env.agent().log_path()),
        http = env.machine.http_port,
        https = env.machine.https_port,
        config = tilde(&env.paths.machine_config()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_field_output() {
        let output = "p1475\ncpuma-dev\nf11\nn*:80\nf12\nn*:80\np88\nccarte\nf3\nn127.0.0.1:443\n";
        assert_eq!(
            parse_lsof(output),
            [
                PortUser {
                    command: "puma-dev".into(),
                    pid: 1475,
                    address: "*:80".into()
                },
                PortUser {
                    command: "carte".into(),
                    pid: 88,
                    address: "127.0.0.1:443".into()
                },
            ]
        );
        assert!(parse_lsof("").is_empty());
    }

    #[test]
    fn parses_resolver_files() {
        assert_eq!(
            parse_resolver("# Generated by puma-dev\nnameserver 127.0.0.1\nport 9253\n"),
            (Some("127.0.0.1"), Some(9253))
        );
        assert_eq!(
            parse_resolver("nameserver 10.0.0.1\n"),
            (Some("10.0.0.1"), None)
        );
    }

    #[test]
    fn grades_port_users() {
        let carte = |address: &str| PortUser {
            command: "carte".into(),
            pid: 1,
            address: address.into(),
        };
        let status = |users: &[PortUser]| {
            let mut report = Report::default();
            port_line(&mut report, "http port", 80, users);
            report.lines[0].status
        };
        assert_eq!(status(&[carte("127.0.0.1:80")]), Status::Ok);
        assert_eq!(status(&[carte("*:80")]), Status::Warn);
        assert_eq!(status(&[]), Status::Missing);
        let other = PortUser {
            command: "puma-dev".into(),
            ..carte("*:80")
        };
        assert_eq!(status(&[other]), Status::Warn);
    }

    #[test]
    fn reports_missing_lines_as_failure() {
        let mut report = Report::default();
        report.push(Status::Ok, "a", "fine");
        report.push(Status::Warn, "b", "hmm");
        assert!(report.ok());
        report.push(Status::Missing, "c", "gone");
        assert!(!report.ok());
        assert_eq!(
            report.to_string(),
            "  ok  a  fine\n  !!  b  hmm\n  --  c  gone\n"
        );
    }
}
