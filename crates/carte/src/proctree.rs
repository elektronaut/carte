//! The process groups a process's descendants run in, so stopping a command
//! also reaches the groups it made for its children.

struct Proc {
    pid: u32,
    ppid: u32,
    pgid: u32,
    sid: u32,
}

/// The process groups of `root`'s living descendants and of the other
/// processes in the session `root` leads, apart from the group `root` leads.
pub fn descendant_groups(root: u32) -> Vec<u32> {
    groups_of(root, &processes())
}

/// The groups to SIGKILL once a grace is over: those `descendant_groups`
/// finds now, and those of `earlier` that still have a member in the session
/// `root` leads.
pub fn remaining_groups(root: u32, earlier: &[u32]) -> Vec<u32> {
    let procs = processes();
    let mut groups: Vec<u32> = earlier
        .iter()
        .copied()
        .filter(|&pgid| procs.iter().any(|p| p.pgid == pgid && p.sid == root))
        .collect();
    for pgid in groups_of(root, &procs) {
        if !groups.contains(&pgid) {
            groups.push(pgid);
        }
    }
    groups
}

fn groups_of(root: u32, procs: &[Proc]) -> Vec<u32> {
    let mut groups = Vec::new();
    let mut add = |pgid: u32| {
        if pgid != root && !groups.contains(&pgid) {
            groups.push(pgid);
        }
    };
    for proc in procs.iter().filter(|p| p.sid == root && p.pid != root) {
        add(proc.pgid);
    }
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for proc in procs.iter().filter(|p| p.ppid == parent && p.pid != root) {
            parents.push(proc.pid);
            add(proc.pgid);
        }
    }
    groups
}

/// The variable the shim puts in its command's environment, naming the shim.
pub const SHIM_MARKER: &str = "CARTE_SHIM";

/// Whether a living member of process group `pgid` is in session `sid`, and a
/// member's environment has `SHIM_MARKER` set to `marker`.
pub fn group_in_session(pgid: u32, sid: u32, marker: &str) -> bool {
    let procs = processes();
    procs.iter().any(|p| p.pgid == pgid && p.sid == sid) && marked(&procs, pgid, marker)
}

/// The process groups with a living member in session `sid` and a member whose
/// environment has `SHIM_MARKER` set to `marker`.
pub fn session_groups(sid: u32, marker: &str) -> Vec<u32> {
    let procs = processes();
    let mut groups = Vec::new();
    for proc in procs.iter().filter(|p| p.sid == sid) {
        if !groups.contains(&proc.pgid) && marked(&procs, proc.pgid, marker) {
            groups.push(proc.pgid);
        }
    }
    groups
}

fn marked(procs: &[Proc], pgid: u32, marker: &str) -> bool {
    procs
        .iter()
        .filter(|p| p.pgid == pgid)
        .any(|p| environ_var(p.pid, SHIM_MARKER).as_deref() == Some(marker))
}

/// `id` as a pid, process group or session to signal: `None` for 0, 1 and
/// anything that isn't a positive `pid_t`.
pub fn signal_target(id: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(id).ok().filter(|&id| id > 1)
}

/// Whether process `pid` may exist: only a definite "no such process" counts
/// as gone, and a pid that isn't a `signal_target` always counts as existing.
pub fn exists(pid: u32) -> bool {
    let Some(pid) = signal_target(pid) else {
        return true;
    };
    // SAFETY: signal 0 only checks that the process exists; `pid` went
    // through `signal_target`.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || nix::errno::Errno::last() != nix::errno::Errno::ESRCH
}

/// Whether process `pid` is stopped by a signal; `None` when it can't be read.
#[cfg(target_os = "macos")]
pub fn is_stopped(pid: u32) -> Option<bool> {
    // SAFETY: proc_bsdinfo is plain data, valid when zeroed.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `size` is the size of `info`.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbi_status == libc::SSTOP)
}

/// Whether process `pid` is stopped by a signal; `None` when it can't be read.
#[cfg(target_os = "linux")]
pub fn is_stopped(pid: u32) -> Option<bool> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let state = stat[stat.rfind(')')? + 2..].split_whitespace().next()?;
    Some(matches!(state, "T" | "t"))
}

/// The value of `name` in process `pid`'s environment as its program was
/// exec'd; `None` when it isn't set or can't be read. Changes the process made
/// since aren't seen.
pub fn environ_var(pid: u32, name: &str) -> Option<String> {
    let environ = environ(pid)?;
    let prefix = format!("{name}=");
    environ
        .split(|&b| b == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .map(|value| String::from_utf8_lossy(value).into_owned())
}

/// The NUL-separated environment area of process `pid`, as set up at exec.
#[cfg(target_os = "linux")]
fn environ(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

/// The NUL-separated environment area of process `pid`, as set up at exec, from
/// `KERN_PROCARGS2`: argc, the executable's path, padding, the arguments, then
/// the environment.
#[cfg(target_os = "macos")]
fn environ(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer only asks for the size, written to `size`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `size` is the byte length of `buf`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?).max(0) as usize;
    let mut rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[end + 1..];
    }
    Some(rest.to_vec())
}

/// What identifies the current boot: `kern.boottime` on macOS, the kernel's
/// boot id on Linux.
#[cfg(target_os = "macos")]
pub fn boot_id() -> Option<String> {
    let mut time = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is NUL-terminated, and `size` is the size of `time`.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut time as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && time.tv_sec > 0).then(|| format!("{}.{}", time.tv_sec, time.tv_usec))
}

#[cfg(target_os = "linux")]
pub fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<Proc> {
    // SAFETY: a null buffer of size 0 only asks for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `size` is the byte length of `pids`.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(count.max(0) as usize);

    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            // SAFETY: proc_bsdinfo is plain data, valid when zeroed.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            // SAFETY: `size` is the size of `info`.
            let n = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            (n == size && info.pbi_status != libc::SZOMB).then(|| Proc {
                pid: pid as u32,
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                // SAFETY: getsid takes no pointers.
                sid: unsafe { libc::getsid(pid) }.max(0) as u32,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn processes() -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let mut fields = stat[stat.rfind(')')? + 2..].split_whitespace();
            if fields.next()? == "Z" {
                return None;
            }
            Some(Proc {
                pid,
                ppid: fields.next()?.parse().ok()?,
                pgid: fields.next()?.parse().ok()?,
                sid: fields.next()?.parse().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn signal_targets_leave_out_everything_but_positive_pids_above_1() {
        assert_eq!(signal_target(0), None);
        assert_eq!(signal_target(1), None);
        assert_eq!(signal_target(2), Some(2));
        assert_eq!(signal_target(i32::MAX as u32), Some(i32::MAX));
        assert_eq!(signal_target(i32::MAX as u32 + 1), None);
        assert_eq!(signal_target(u32::MAX), None);
    }

    #[test]
    fn reads_its_own_environment() {
        let path = std::env::var("PATH").unwrap();
        assert_eq!(environ_var(std::process::id(), "PATH"), Some(path));
        assert_eq!(
            environ_var(std::process::id(), "CARTE_NOT_SET_ANYWHERE"),
            None
        );
        assert_eq!(boot_id(), boot_id());
        assert!(boot_id().is_some());
    }

    #[test]
    fn finds_the_groups_of_children_and_grandchildren() {
        // SAFETY: the child makes only async-signal-safe calls.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            // SAFETY: setpgid, fork, sleep and _exit take no pointers.
            unsafe {
                libc::setpgid(0, 0);
                if libc::fork() == 0 {
                    libc::setpgid(0, 0);
                    libc::sleep(5);
                    libc::_exit(0);
                }
                libc::sleep(5);
                libc::_exit(0);
            }
        }
        let child = child as u32;
        let deadline = Instant::now() + Duration::from_secs(5);
        let inner = loop {
            let groups = descendant_groups(child);
            if !groups.is_empty() || Instant::now() > deadline {
                break groups;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let outer = descendant_groups(std::process::id());
        for target in inner
            .iter()
            .chain([&child])
            .filter_map(|&group| signal_target(group))
        {
            // SAFETY: killpg takes no pointers; the groups are this test's
            // forked child's and its child's, through `signal_target`.
            unsafe { libc::killpg(target, libc::SIGKILL) };
        }
        // SAFETY: a null status pointer is allowed.
        unsafe { libc::waitpid(child as i32, std::ptr::null_mut(), 0) };

        assert_eq!(inner.len(), 1, "{inner:?}");
        assert_ne!(inner[0], child);
        assert!(outer.contains(&child), "{outer:?}");
        assert!(outer.contains(&inner[0]), "{outer:?}");
    }
}
