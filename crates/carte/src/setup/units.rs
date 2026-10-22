//! The daemon's systemd user units: a socket unit for each port and the service that
//! runs `carte daemon` with their sockets.

use std::fmt::Write;
use std::path::Path;

use super::plist::Agent;

pub const SERVICE: &str = "carte.service";
pub const HTTP_SOCKET: &str = "carte-http.socket";
pub const HTTPS_SOCKET: &str = "carte-https.socket";
/// Every unit setup installs, sockets first.
pub const UNITS: [&str; 3] = [HTTP_SOCKET, HTTPS_SOCKET, SERVICE];

impl Agent {
    /// The units setup installs, by name, in the order of [`UNITS`]. `ipv6` adds a
    /// listener on `[::1]` beside the one on 127.0.0.1.
    pub fn units(&self, ipv6: bool) -> [(&'static str, String); 3] {
        [
            (
                HTTP_SOCKET,
                socket_unit("HTTP", "http", self.http_port, ipv6),
            ),
            (
                HTTPS_SOCKET,
                socket_unit("HTTPS", "https", self.https_port, ipv6),
            ),
            (SERVICE, self.service_unit()),
        ]
    }

    pub fn service_unit(&self) -> String {
        let log = specifiers(&self.log_path().to_string_lossy());
        let sockets = format!("{HTTP_SOCKET} {HTTPS_SOCKET}");
        let mut out = String::from("# Written by `carte setup`.\n[Unit]\n");
        let _ = writeln!(out, "Description=carte, which runs your apps on demand");
        let _ = writeln!(out, "Requires={sockets}");
        let _ = writeln!(out, "After={sockets}");
        out.push_str("\n[Service]\n");
        let _ = writeln!(
            out,
            "ExecStart={} daemon",
            quote(&self.exe.to_string_lossy()).replace('$', "$$")
        );
        for line in self.environment() {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "Sockets={sockets}");
        out.push_str("Restart=always\nKillMode=mixed\nTimeoutStopSec=40\nLimitNOFILE=8192\n");
        let _ = writeln!(out, "StandardOutput=append:{log}");
        let _ = writeln!(out, "StandardError=append:{log}");
        out.push_str("\n[Install]\nWantedBy=default.target\n");
        out
    }

    /// The user's own bin directories, Homebrew on Linux, then the system's.
    pub fn linux_path_var(&self) -> String {
        let home = |rel: &str| self.home.join(rel).to_string_lossy().into_owned();
        [
            home(".local/bin"),
            home(".cargo/bin"),
            "/home/linuxbrew/.linuxbrew/bin".to_string(),
            "/usr/local/sbin".to_string(),
            "/usr/local/bin".to_string(),
            "/usr/sbin".to_string(),
            "/usr/bin".to_string(),
            "/sbin".to_string(),
            "/bin".to_string(),
        ]
        .join(":")
    }

    fn environment(&self) -> [String; 3] {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        [
            ("PATH", self.linux_path_var()),
            ("CARTE_CONFIG_DIR", path(&self.config_dir)),
            ("CARTE_STATE_HOME", path(&self.state_dir)),
        ]
        .map(|(name, value)| format!("Environment={}", quote(&format!("{name}={value}"))))
    }
}

fn socket_unit(title: &str, name: &str, port: u16, ipv6: bool) -> String {
    let mut out = format!(
        "# Written by `carte setup`.\n[Unit]\nDescription=carte's {title} port\n\n[Socket]\n"
    );
    let _ = writeln!(out, "ListenStream=127.0.0.1:{port}");
    if ipv6 {
        let _ = writeln!(out, "ListenStream=[::1]:{port}");
    }
    let _ = writeln!(out, "FileDescriptorName={name}");
    let _ = writeln!(out, "Service={SERVICE}");
    out.push_str("\n[Install]\nWantedBy=sockets.target\n");
    out
}

/// Whether an installed service runs the daemon with `state_dir` as its state
/// directory, and so answers on that directory's socket.
pub fn runs_with_state(service: &str, state_dir: &Path) -> bool {
    let line = format!(
        "Environment={}",
        quote(&format!("CARTE_STATE_HOME={}", state_dir.to_string_lossy()))
    );
    service.lines().any(|l| l == line)
}

/// `value` as one word of a unit file setting, in double quotes when it needs them.
fn quote(value: &str) -> String {
    let value = specifiers(value);
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:+,=@%".contains(c))
    {
        return value;
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `value` with systemd's `%` specifiers escaped.
fn specifiers(value: &str) -> String {
    value.replace('%', "%%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Agent {
        Agent {
            exe: "/home/me/.cargo/bin/carte".into(),
            home: "/home/me".into(),
            config_dir: "/home/me/.config/carte".into(),
            state_dir: "/home/me/.local/state/carte".into(),
            http_port: 80,
            https_port: 443,
        }
    }

    #[test]
    fn runs_the_daemon_with_socket_units() {
        let [http, https, service] = agent().units(true);
        assert_eq!(http.0, "carte-http.socket");
        assert_eq!(
            http.1,
            "# Written by `carte setup`.\n[Unit]\nDescription=carte's HTTP port\n\n[Socket]\n\
             ListenStream=127.0.0.1:80\nListenStream=[::1]:80\nFileDescriptorName=http\n\
             Service=carte.service\n\n[Install]\nWantedBy=sockets.target\n"
        );
        assert!(
            https.1.contains(
                "ListenStream=127.0.0.1:443\nListenStream=[::1]:443\nFileDescriptorName=https\n"
            ),
            "{}",
            https.1
        );
        assert_eq!(service.0, "carte.service");
        assert_eq!(
            service.1,
            "# Written by `carte setup`.\n[Unit]\nDescription=carte, which runs your apps on demand\n\
             Requires=carte-http.socket carte-https.socket\nAfter=carte-http.socket carte-https.socket\n\n\
             [Service]\nExecStart=/home/me/.cargo/bin/carte daemon\n\
             Environment=PATH=/home/me/.local/bin:/home/me/.cargo/bin:/home/linuxbrew/.linuxbrew/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
             Environment=CARTE_CONFIG_DIR=/home/me/.config/carte\n\
             Environment=CARTE_STATE_HOME=/home/me/.local/state/carte\n\
             Sockets=carte-http.socket carte-https.socket\n\
             Restart=always\nKillMode=mixed\nTimeoutStopSec=40\nLimitNOFILE=8192\n\
             StandardOutput=append:/home/me/.local/state/carte/carte.log\n\
             StandardError=append:/home/me/.local/state/carte/carte.log\n\n\
             [Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn listens_only_on_127_0_0_1_without_ipv6() {
        let [http, ..] = agent().units(false);
        assert!(
            http.1
                .contains("ListenStream=127.0.0.1:80\nFileDescriptorName=http\n")
        );
        assert!(!http.1.contains("[::1]"));
    }

    #[test]
    fn quotes_and_escapes_paths() {
        let odd = Agent {
            exe: "/home/me/my $bin/carte".into(),
            state_dir: "/home/me/100% \"state\"".into(),
            ..agent()
        };
        let service = odd.service_unit();
        assert!(
            service.contains("ExecStart=\"/home/me/my $$bin/carte\" daemon\n"),
            "{service}"
        );
        assert!(
            service.contains("Environment=\"CARTE_STATE_HOME=/home/me/100%% \\\"state\\\"\"\n"),
            "{service}"
        );
        assert!(service.contains("StandardOutput=append:/home/me/100%% \"state\"/carte.log\n"));
        assert!(runs_with_state(&service, &odd.state_dir));
        assert!(!runs_with_state(&service, Path::new("/home/me/100%")));
        assert!(runs_with_state(&agent().service_unit(), &agent().state_dir));
    }
}
