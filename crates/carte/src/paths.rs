//! Where carte keeps configuration, state and its socket.

use std::env;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use carte_core::model::InstanceKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    /// `CARTE_CONFIG_DIR` and `CARTE_STATE_DIR` win, then the XDG variables, then
    /// `~/.config/carte` and `~/.local/state/carte`.
    pub fn from_env() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dir = |own: &str, xdg: &str, fallback: &str| {
            env::var_os(own)
                .map(PathBuf::from)
                .or_else(|| env::var_os(xdg).map(|d| PathBuf::from(d).join("carte")))
                .unwrap_or_else(|| home.join(fallback).join("carte"))
        };
        Paths {
            config_dir: dir("CARTE_CONFIG_DIR", "XDG_CONFIG_HOME", ".config"),
            state_dir: dir("CARTE_STATE_DIR", "XDG_STATE_HOME", ".local/state"),
        }
    }

    pub fn under(root: &Path) -> Self {
        Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    pub fn machine_config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.config_dir.join("projects")
    }

    /// The project's file, in whichever folder it is, or where a new one goes.
    pub fn project_file(&self, name: &str) -> PathBuf {
        carte_core::discovery::project_files(&self.projects_dir())
            .ok()
            .and_then(|files| files.into_iter().find(|f| f.name == name))
            .map(|f| f.path)
            .unwrap_or_else(|| self.projects_dir().join(format!("{name}.toml")))
    }

    /// `CARTE_SOCKET` wins. Unix socket paths are limited to about 100 bytes, so a
    /// deep state directory gets a short path in the temp directory named after it.
    pub fn socket(&self) -> PathBuf {
        if let Some(path) = env::var_os("CARTE_SOCKET") {
            return PathBuf::from(path);
        }
        let path = self.state_dir.join("carte.sock");
        if path.as_os_str().len() < 100 {
            return path;
        }
        let digest = Sha256::digest(self.state_dir.as_os_str().as_encoded_bytes());
        let name: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        PathBuf::from("/tmp").join(format!("carte-{name}.sock"))
    }

    /// Pidfiles and the record of running process groups.
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// The carte CA alone (`NODE_EXTRA_CA_CERTS`).
    pub fn ca_cert(&self) -> PathBuf {
        self.state_dir.join("ca.pem")
    }

    /// System roots plus the carte CA (`SSL_CERT_FILE`).
    pub fn ca_bundle(&self) -> PathBuf {
        self.state_dir.join("ca-bundle.pem")
    }

    /// The `PIDFILE` handed to an instance's processes.
    pub fn pidfile(&self, key: &InstanceKey) -> PathBuf {
        self.run_dir()
            .join(&key.project)
            .join(format!("{}@{}.pid", key.app, key.stack.slug()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_state_dirs_get_a_short_socket_path() {
        let shallow = Paths::under(Path::new("/tmp/x"));
        assert_eq!(shallow.socket(), PathBuf::from("/tmp/x/state/carte.sock"));
        let deep = Paths::under(&Path::new("/tmp").join("d".repeat(120)));
        let socket = deep.socket();
        assert!(socket.as_os_str().len() < 100, "{}", socket.display());
        assert_eq!(socket, deep.socket());
    }

    #[test]
    fn project_files_are_found_in_folders() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.projects_dir().join("agency")).unwrap();
        std::fs::write(paths.projects_dir().join("agency/wiki.toml"), "").unwrap();
        assert_eq!(
            paths.project_file("wiki"),
            paths.projects_dir().join("agency/wiki.toml")
        );
        assert_eq!(
            paths.project_file("new"),
            paths.projects_dir().join("new.toml")
        );
    }
}
