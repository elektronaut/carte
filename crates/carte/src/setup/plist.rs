//! The daemon's LaunchAgent property list.

use std::fmt::Write;
use std::path::{Path, PathBuf};

pub const LABEL: &str = "no.elektronaut.carte";
/// The label earlier installs used.
pub const OLD_LABEL: &str = "carte.daemon";

/// Everything the LaunchAgent needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub exe: PathBuf,
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub http_port: u16,
    pub https_port: u16,
}

impl Agent {
    pub fn log_path(&self) -> PathBuf {
        self.state_dir.join("carte.log")
    }

    /// Homebrew, the user's own bin directories, then the system's.
    pub fn path_var(&self) -> String {
        let home = |rel: &str| self.home.join(rel).to_string_lossy().into_owned();
        [
            "/opt/homebrew/bin".to_string(),
            "/opt/homebrew/sbin".to_string(),
            "/usr/local/bin".to_string(),
            home(".local/bin"),
            home(".cargo/bin"),
            "/usr/bin".to_string(),
            "/bin".to_string(),
            "/usr/sbin".to_string(),
            "/sbin".to_string(),
        ]
        .join(":")
    }

    pub fn to_plist(&self) -> String {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let log = path(&self.log_path());
        let mut out = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        key_string(&mut out, 1, "Label", LABEL);
        key(&mut out, 1, "ProgramArguments");
        line(&mut out, 1, "<array>");
        string(&mut out, 2, &path(&self.exe));
        string(&mut out, 2, "daemon");
        line(&mut out, 1, "</array>");
        key(&mut out, 1, "EnvironmentVariables");
        line(&mut out, 1, "<dict>");
        key_string(&mut out, 2, "PATH", &self.path_var());
        key_string(&mut out, 2, "CARTE_CONFIG_DIR", &path(&self.config_dir));
        key_string(&mut out, 2, "CARTE_STATE_DIR", &path(&self.state_dir));
        line(&mut out, 1, "</dict>");
        key(&mut out, 1, "KeepAlive");
        line(&mut out, 1, "<true/>");
        key(&mut out, 1, "RunAtLoad");
        line(&mut out, 1, "<true/>");
        key(&mut out, 1, "ProcessType");
        string(&mut out, 1, "Interactive");
        key(&mut out, 1, "SoftResourceLimits");
        line(&mut out, 1, "<dict>");
        key(&mut out, 2, "NumberOfFiles");
        line(&mut out, 2, "<integer>8192</integer>");
        line(&mut out, 1, "</dict>");
        key_string(&mut out, 1, "StandardOutPath", &log);
        key_string(&mut out, 1, "StandardErrorPath", &log);
        key(&mut out, 1, "Sockets");
        line(&mut out, 1, "<dict>");
        socket(&mut out, "http", self.http_port);
        socket(&mut out, "https", self.https_port);
        line(&mut out, 1, "</dict>");
        out.push_str("</dict>\n</plist>\n");
        out
    }
}

fn socket(out: &mut String, name: &str, port: u16) {
    key(out, 2, name);
    line(out, 2, "<dict>");
    key_string(out, 3, "SockNodeName", "127.0.0.1");
    key_string(out, 3, "SockServiceName", &port.to_string());
    key_string(out, 3, "SockType", "stream");
    key_string(out, 3, "SockFamily", "IPv4");
    line(out, 2, "</dict>");
}

fn line(out: &mut String, depth: usize, text: &str) {
    let _ = writeln!(out, "{}{text}", "  ".repeat(depth));
}

fn key(out: &mut String, depth: usize, name: &str) {
    line(out, depth, &format!("<key>{}</key>", escape(name)));
}

fn string(out: &mut String, depth: usize, value: &str) {
    line(out, depth, &format!("<string>{}</string>", escape(value)));
}

fn key_string(out: &mut String, depth: usize, name: &str, value: &str) {
    key(out, depth, name);
    string(out, depth, value);
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Agent {
        Agent {
            exe: "/opt/homebrew/bin/carte".into(),
            home: "/Users/me".into(),
            config_dir: "/Users/me/.config/carte".into(),
            state_dir: "/Users/me/.local/state/R&D <carte>".into(),
            http_port: 80,
            https_port: 443,
        }
    }

    #[test]
    fn runs_the_daemon_with_launchd_sockets() {
        let plist = agent().to_plist();
        for expected in [
            "  <key>Label</key>\n  <string>no.elektronaut.carte</string>\n",
            "  <array>\n    <string>/opt/homebrew/bin/carte</string>\n    <string>daemon</string>\n  </array>\n",
            "<key>KeepAlive</key>\n  <true/>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<string>/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/Users/me/.local/bin:/Users/me/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>",
            "<key>CARTE_STATE_DIR</key>\n    <string>/Users/me/.local/state/R&amp;D &lt;carte&gt;</string>",
            "<key>StandardErrorPath</key>\n  <string>/Users/me/.local/state/R&amp;D &lt;carte&gt;/carte.log</string>",
            "    <key>http</key>\n    <dict>\n      <key>SockNodeName</key>\n      <string>127.0.0.1</string>\n      \
             <key>SockServiceName</key>\n      <string>80</string>\n      <key>SockType</key>\n      \
             <string>stream</string>\n      <key>SockFamily</key>\n      <string>IPv4</string>\n    </dict>\n",
            "<key>https</key>\n    <dict>\n      <key>SockNodeName</key>\n      <string>127.0.0.1</string>\n      \
             <key>SockServiceName</key>\n      <string>443</string>",
        ] {
            assert!(plist.contains(expected), "missing {expected:?} in\n{plist}");
        }
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
        assert!(plist.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn is_a_valid_property_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no.elektronaut.carte.plist");
        std::fs::write(&path, agent().to_plist()).unwrap();
        let output = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}
