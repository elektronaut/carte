//! Where carte keeps configuration, state and its socket.

use std::env;
use std::path::{Path, PathBuf};

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

    pub fn project_file(&self, name: &str) -> PathBuf {
        self.projects_dir().join(format!("{name}.toml"))
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("carte.sock")
    }

    /// Pidfiles and the record of running process groups.
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }
}
