//! A record of running process groups, so a restarted daemon can kill what a
//! crashed one left behind. The record names the boot it was made in, and each
//! entry carries the leader's start time, its session (the shim's) and the
//! marker the shim put in the command's environment, so a recycled pid is never
//! mistaken for one of ours.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::proctree::{boot_id, exists, group_in_session, session_groups};

pub struct GroupRecord {
    path: PathBuf,
    groups: BTreeMap<u32, Entry>,
}

struct Entry {
    started: u64,
    sid: u32,
    marker: String,
}

impl GroupRecord {
    pub fn new(path: PathBuf) -> Self {
        GroupRecord {
            path,
            groups: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, pgid: u32) {
        let sid = unsafe { libc::getsid(pgid as libc::pid_t) };
        if let Some(started) = start_time(pgid)
            && sid > 1
        {
            let sid = sid as u32;
            let marker = start_time(sid).map_or_else(|| "-".into(), |shim| format!("{sid}.{shim}"));
            self.groups.insert(
                pgid,
                Entry {
                    started,
                    sid,
                    marker,
                },
            );
            self.save();
        }
    }

    pub fn remove(&mut self, pgid: u32) {
        if self.groups.remove(&pgid).is_some() {
            self.save();
        }
    }

    fn save(&self) {
        let Some(boot) = boot_id() else {
            return;
        };
        let mut text = format!("boot {boot}\n");
        for (pgid, entry) in &self.groups {
            let Entry {
                started,
                sid,
                marker,
            } = entry;
            text.push_str(&format!("{pgid} {started} {sid} {marker}\n"));
        }
        let tmp = self.path.with_extension("tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
        }
    }
}

/// Kills the groups recorded by a previous run in this boot whose leaders are
/// still the same processes, and returns how many groups it killed. A group
/// whose leader is gone, and the other groups in the session of a shim that is
/// gone, go only when one of their members carries the shim's marker.
pub fn kill_leftovers(path: &Path) -> io::Result<usize> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut lines = text.lines();
    let boot = lines.next().and_then(|line| line.strip_prefix("boot "));
    if boot.is_none() || boot.map(str::to_string) != boot_id() {
        fs::remove_file(path)?;
        return Ok(0);
    }
    let mut killed = 0;
    for line in lines {
        let mut parts = line.split_whitespace();
        let (Some(Ok(pgid)), Some(Ok(started))) = (
            parts.next().map(str::parse::<u32>),
            parts.next().map(str::parse::<u64>),
        ) else {
            continue;
        };
        if pgid <= 1 {
            continue;
        }
        let sid = parts
            .next()
            .and_then(|sid| sid.parse::<u32>().ok())
            .filter(|&sid| sid > 1);
        let marker = parts.next().filter(|&marker| marker != "-");
        let session = sid.zip(marker);
        let ours = match start_time(pgid) {
            Some(leader) => leader == started,
            None => {
                !exists(pgid)
                    && session.is_some_and(|(sid, marker)| group_in_session(pgid, sid, marker))
            }
        };
        let mut groups = Vec::new();
        if ours {
            groups.push(pgid);
        }
        if let Some((sid, marker)) = session
            && !exists(sid)
        {
            groups.extend(
                session_groups(sid, marker)
                    .into_iter()
                    .filter(|&group| group > 1 && group != pgid),
            );
        }
        for group in groups {
            if killpg(Pid::from_raw(group as i32), Signal::SIGKILL).is_ok() {
                killed += 1;
            }
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
    use crate::proctree::{SHIM_MARKER, environ_var};

    #[test]
    fn start_time_of_self_is_stable() {
        let pid = std::process::id();
        let first = start_time(pid).unwrap();
        assert_eq!(start_time(pid), Some(first));
    }

    /// Sleeps for the groups `Session` makes; a no-op otherwise.
    #[test]
    fn sleeper() {
        if std::env::var_os("CARTE_TEST_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    /// Processes a test made: a session whose leader keeps running when `stays`,
    /// holding a group whose leader has exited, a group led by `led`, both
    /// sleeping with `marker` in their environment, and a group led by
    /// `unmarked` without it. Dropping it kills those of them that are still the
    /// same processes.
    struct Session {
        sid: libc::pid_t,
        stays: bool,
        marker: String,
        leaderless: libc::pid_t,
        member: libc::pid_t,
        led: libc::pid_t,
        unmarked: libc::pid_t,
        started: Vec<(libc::pid_t, u64)>,
    }

    static NEXT_MARKER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    impl Session {
        fn new(stays: bool) -> Self {
            use std::ffi::CString;

            let marker = format!(
                "test.{}.{}",
                std::process::id(),
                NEXT_MARKER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let exe = std::env::current_exe().unwrap();
            let sleep = CString::new(exe.into_os_string().into_encoded_bytes()).unwrap();
            let argv = [
                sleep.as_ptr(),
                c"--exact".as_ptr(),
                c"supervisor::groups::tests::sleeper".as_ptr(),
                std::ptr::null(),
            ];
            let entry = CString::new(format!("{SHIM_MARKER}={marker}")).unwrap();
            let marked_env = [
                entry.as_ptr(),
                c"CARTE_TEST_SLEEPER=1".as_ptr(),
                std::ptr::null(),
            ];
            let bare_env = [c"CARTE_TEST_SLEEPER=1".as_ptr(), std::ptr::null()];
            let mut inner = [0; 2];
            let mut outer = [0; 2];
            assert_eq!(unsafe { libc::pipe(inner.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { libc::pipe(outer.as_mut_ptr()) }, 0);
            let sid = unsafe { libc::fork() };
            if sid == 0 {
                unsafe {
                    libc::setsid();
                    let mut pids = [0 as libc::pid_t; 4];
                    pids[0] = libc::fork();
                    if pids[0] == 0 {
                        libc::setpgid(0, 0);
                        let member = libc::fork();
                        if member == 0 {
                            libc::execve(sleep.as_ptr(), argv.as_ptr(), marked_env.as_ptr());
                            libc::_exit(127);
                        }
                        libc::write(inner[1], (&raw const member).cast(), 4);
                        libc::_exit(0);
                    }
                    libc::setpgid(pids[0], pids[0]);
                    libc::read(inner[0], (&raw mut pids[1]).cast(), 4);
                    libc::waitpid(pids[0], std::ptr::null_mut(), 0);
                    for (i, env) in [(2, marked_env.as_ptr()), (3, bare_env.as_ptr())] {
                        pids[i] = libc::fork();
                        if pids[i] == 0 {
                            libc::setpgid(0, 0);
                            libc::execve(sleep.as_ptr(), argv.as_ptr(), env);
                            libc::_exit(127);
                        }
                        libc::setpgid(pids[i], pids[i]);
                    }
                    libc::write(outer[1], pids.as_ptr().cast(), 16);
                    if stays {
                        libc::sleep(5);
                    }
                    libc::_exit(0);
                }
            }
            let mut pids = [0 as libc::pid_t; 4];
            let n = unsafe {
                libc::close(inner[0]);
                libc::close(inner[1]);
                libc::close(outer[1]);
                let n = libc::read(outer[0], pids.as_mut_ptr().cast(), 16);
                libc::close(outer[0]);
                n
            };
            assert_eq!(n, 16);
            assert!(std::iter::once(sid).chain(pids).all(|pid| pid > 1));
            let started = [sid, pids[1], pids[2], pids[3]]
                .into_iter()
                .filter_map(|pid| Some((pid, start_time(pid as u32)?)))
                .collect();
            if !stays {
                unsafe { libc::waitpid(sid, std::ptr::null_mut(), 0) };
            }
            let session = Session {
                sid,
                stays,
                marker,
                leaderless: pids[0],
                member: pids[1],
                led: pids[2],
                unmarked: pids[3],
                started,
            };
            for pid in [session.member, session.led] {
                session.wait_for_marker(pid);
            }
            session
        }

        fn wait_for_marker(&self, pid: libc::pid_t) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while environ_var(pid as u32, SHIM_MARKER).as_deref() != Some(&self.marker) {
                assert!(std::time::Instant::now() < deadline, "{pid} has no marker");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn alive(&self, pid: libc::pid_t) -> bool {
            self.started
                .iter()
                .any(|&(p, started)| p == pid && start_time(pid as u32) == Some(started))
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            for &(pid, started) in &self.started {
                if pid > 1
                    && (pid != self.sid || self.stays)
                    && start_time(pid as u32) == Some(started)
                {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
            if self.stays {
                unsafe { libc::waitpid(self.sid, std::ptr::null_mut(), 0) };
            }
        }
    }

    fn gone(pid: libc::pid_t) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while start_time(pid as u32).is_some() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    fn kill_recorded(boot: &str, lines: &str) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups");
        fs::write(&path, format!("boot {boot}\n{lines}")).unwrap();
        let killed = kill_leftovers(&path).unwrap();
        assert!(!path.exists());
        killed
    }

    fn this_boot() -> String {
        boot_id().unwrap()
    }

    #[test]
    fn leftovers_with_a_different_start_time_are_spared() {
        let s = Session::new(true);
        let line = format!("{} 1 {} {}\nnot a line\n", s.led, s.sid, s.marker);
        assert_eq!(kill_recorded(&this_boot(), &line), 0);
        assert!(s.alive(s.led));
    }

    #[test]
    fn groups_whose_leader_exited_are_killed() {
        let s = Session::new(true);
        let line = format!("{} 1 {} {}\n", s.leaderless, s.sid, s.marker);
        assert_eq!(kill_recorded(&this_boot(), &line), 1);
        assert!(gone(s.member));
        assert!(s.alive(s.led));
        assert!(s.alive(s.unmarked));
    }

    #[test]
    fn leaderless_groups_without_the_session_or_marker_are_spared() {
        let s = Session::new(true);
        let other = Session::new(true);
        let lines = format!(
            "{l} 1 {} {}\n{l} 1 {} -\n{l} 1 {} wrong\n{l} 1 {}\n{l} 1\n",
            other.sid,
            other.marker,
            s.sid,
            s.sid,
            s.sid,
            l = s.leaderless,
        );
        assert_eq!(kill_recorded(&this_boot(), &lines), 0);
        assert!(s.alive(s.member));
    }

    #[test]
    fn a_session_whose_shim_is_gone_loses_its_marked_groups() {
        let s = Session::new(false);
        let started = start_time(s.led as u32).unwrap();
        let line = format!("{} {started} {} {}\n", s.led, s.sid, s.marker);
        assert_eq!(kill_recorded(&this_boot(), &line), 2);
        assert!(gone(s.led));
        assert!(gone(s.member));
        assert!(s.alive(s.unmarked));
    }

    #[test]
    fn a_session_whose_shim_is_gone_is_spared_with_another_marker() {
        let s = Session::new(false);
        let line = format!("{} 1 {} wrong\n", s.led, s.sid);
        assert_eq!(kill_recorded(&this_boot(), &line), 0);
        assert!(s.alive(s.led));
        assert!(s.alive(s.member));
    }

    #[test]
    fn a_record_from_another_boot_kills_nothing() {
        let s = Session::new(false);
        let started = start_time(s.led as u32).unwrap();
        let line = format!("{} {started} {} {}\n", s.led, s.sid, s.marker);
        assert_eq!(kill_recorded("another-boot", &line), 0);
        assert!(s.alive(s.led));
        assert!(s.alive(s.member));
    }

    #[test]
    fn missing_record_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(kill_leftovers(&dir.path().join("groups")).unwrap(), 0);
    }
}
