//! carte's skill for coding agents, installed where agents look for skills.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const SKILL: &str = include_str!("../../skill/SKILL.md");

/// Marks a skill directory as carte's; holds the version that wrote it.
pub const MARKER: &str = ".managed-by-carte";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where agents that follow the Agent Skills convention find carte's skill.
pub fn dir_in(home: &Path) -> PathBuf {
    home.join(".agents/skills/carte")
}

/// The skill's directory as setup found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDir {
    Missing,
    /// Written by carte; `current` when it holds this version's skill.
    Managed {
        current: bool,
    },
    /// Something carte didn't write, which it leaves alone.
    Unmanaged,
}

/// The link from Claude Code's skills directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLink {
    /// Claude Code's directory doesn't exist, so there's nothing to link from.
    NoClaude,
    Missing,
    /// Points at carte's skill directory.
    Ours,
    /// Something else is in its place.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillFacts {
    pub dir: SkillDir,
    pub link: SkillLink,
}

impl SkillFacts {
    pub fn gather(dir: &Path, link: &Path, claude_dir: &Path) -> Self {
        SkillFacts {
            dir: dir_state(dir),
            link: link_state(dir, link, claude_dir),
        }
    }
}

fn dir_state(dir: &Path) -> SkillDir {
    if fs::symlink_metadata(dir).is_err() {
        return SkillDir::Missing;
    }
    match fs::read_to_string(dir.join(MARKER)) {
        Ok(version) => SkillDir::Managed {
            current: version.trim() == VERSION
                && fs::read_to_string(dir.join("SKILL.md")).is_ok_and(|s| s == SKILL),
        },
        Err(_) => SkillDir::Unmanaged,
    }
}

fn link_state(dir: &Path, link: &Path, claude_dir: &Path) -> SkillLink {
    if !claude_dir.is_dir() {
        return SkillLink::NoClaude;
    }
    match fs::read_link(link) {
        Ok(target) if target == dir => SkillLink::Ours,
        Ok(_) => SkillLink::Other,
        Err(_) if fs::symlink_metadata(link).is_ok() => SkillLink::Other,
        Err(_) => SkillLink::Missing,
    }
}

/// Writes the skill and its marker into `dir`.
pub fn install(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("SKILL.md"), SKILL)?;
    fs::write(dir.join(MARKER), format!("{VERSION}\n"))
}

/// Rewrites a skill carte installed earlier when this version's differs.
/// Does nothing for a directory carte doesn't manage.
pub fn refresh(dir: &Path) -> io::Result<bool> {
    if dir_state(dir) != (SkillDir::Managed { current: false }) {
        return Ok(false);
    }
    install(dir)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_has_frontmatter() {
        assert!(SKILL.starts_with("---\nname: carte\ndescription: "));
    }

    #[test]
    fn tells_managed_from_unmanaged_directories() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("agents/carte");
        assert_eq!(dir_state(&dir), SkillDir::Missing);

        install(&dir).unwrap();
        assert_eq!(dir_state(&dir), SkillDir::Managed { current: true });

        fs::write(dir.join(MARKER), "0.0.1\n").unwrap();
        assert_eq!(dir_state(&dir), SkillDir::Managed { current: false });

        fs::remove_file(dir.join(MARKER)).unwrap();
        assert_eq!(dir_state(&dir), SkillDir::Unmanaged);
    }

    #[test]
    fn refreshes_only_an_outdated_managed_skill() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("carte");
        assert!(!refresh(&dir).unwrap());
        assert!(!dir.exists());

        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "mine").unwrap();
        assert!(!refresh(&dir).unwrap());
        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), "mine");

        fs::write(dir.join(MARKER), "0.0.1\n").unwrap();
        assert!(refresh(&dir).unwrap());
        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), SKILL);
        assert!(!refresh(&dir).unwrap());
    }

    #[test]
    fn finds_the_link() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("agents/carte");
        let claude = root.path().join("claude");
        let link = claude.join("skills/carte");
        assert_eq!(link_state(&dir, &link, &claude), SkillLink::NoClaude);

        fs::create_dir_all(claude.join("skills")).unwrap();
        assert_eq!(link_state(&dir, &link, &claude), SkillLink::Missing);

        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(link_state(&dir, &link, &claude), SkillLink::Ours);

        fs::remove_file(&link).unwrap();
        fs::create_dir(&link).unwrap();
        assert_eq!(link_state(&dir, &link, &claude), SkillLink::Other);
    }
}
