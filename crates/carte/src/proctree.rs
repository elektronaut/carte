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

/// Whether a living member of process group `pgid` is in session `sid`.
pub fn group_in_session(pgid: u32, sid: u32) -> bool {
    processes().iter().any(|p| p.pgid == pgid && p.sid == sid)
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<Proc> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(count.max(0) as usize);

    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
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
    fn finds_the_groups_of_children_and_grandchildren() {
        let child = unsafe { libc::fork() };
        if child == 0 {
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
        for &group in inner.iter().chain([&child]) {
            unsafe { libc::killpg(group as i32, libc::SIGKILL) };
        }
        unsafe { libc::waitpid(child as i32, std::ptr::null_mut(), 0) };

        assert_eq!(inner.len(), 1, "{inner:?}");
        assert_ne!(inner[0], child);
        assert!(outer.contains(&child), "{outer:?}");
        assert!(outer.contains(&inner[0]), "{outer:?}");
    }
}
