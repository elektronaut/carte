//! A record of running process groups, so a restarted daemon can kill what a
//! crashed one left behind. Each entry carries the leader's start time, so a
//! recycled pid is never mistaken for one of ours.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

pub struct GroupRecord {
    path: PathBuf,
    groups: BTreeMap<u32, u64>,
}

impl GroupRecord {
    pub fn new(path: PathBuf) -> Self {
        GroupRecord {
            path,
            groups: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, pgid: u32) {
        if let Some(started) = start_time(pgid) {
            self.groups.insert(pgid, started);
            self.save();
        }
    }

    pub fn remove(&mut self, pgid: u32) {
        if self.groups.remove(&pgid).is_some() {
            self.save();
        }
    }

    fn save(&self) {
        let text: String = self
            .groups
            .iter()
            .map(|(pgid, started)| format!("{pgid} {started}\n"))
            .collect();
        let tmp = self.path.with_extension("tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
        }
    }
}

/// Kills the groups recorded by a previous run whose leaders are still the same
/// processes, and returns how many it killed.
pub fn kill_leftovers(path: &Path) -> io::Result<usize> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut killed = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(Ok(pgid)), Some(Ok(started))) = (
            parts.next().map(str::parse::<u32>),
            parts.next().map(str::parse::<u64>),
        ) else {
            continue;
        };
        if start_time(pgid) == Some(started)
            && killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL).is_ok()
        {
            killed += 1;
        }
    }
    fs::remove_file(path)?;
    Ok(killed)
}

/// The process's start time in microseconds since the epoch.
#[cfg(target_os = "macos")]
pub fn start_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// The process's start time in clock ticks since boot.
#[cfg(target_os = "linux")]
pub fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 2..];
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_time_of_self_is_stable() {
        let pid = std::process::id();
        let first = start_time(pid).unwrap();
        assert_eq!(start_time(pid), Some(first));
    }

    #[test]
    fn leftovers_with_a_different_start_time_are_spared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups");
        let me = std::process::id();
        fs::write(&path, format!("{me} 1\nnot a line\n")).unwrap();
        assert_eq!(kill_leftovers(&path).unwrap(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn missing_record_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(kill_leftovers(&dir.path().join("groups")).unwrap(), 0);
    }
}
