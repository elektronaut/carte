//! The `carte __shim` mode: runs one command in its own process group and
//! takes the group down when the daemon goes away.
//!
//! The daemon starts the shim as a session leader (on the pty slave, or with
//! pipes) and hands it one end of a socketpair, the lifeline. The shim:
//!
//! 1. forks `sh -c <command>` into a new process group and, on a tty, makes
//!    that group the terminal's foreground group;
//! 2. writes the command's pid (= pgid) to the lifeline as 4 little-endian bytes;
//! 3. forwards SIGTERM, SIGINT and SIGHUP to the group, and continues it when
//!    it stops itself with a job-control signal, as programs do on ctrl-z;
//! 4. on EOF from the lifeline, sends TERM to the group and to the groups its
//!    descendants made, waits a short grace, sends KILL and exits;
//! 5. when the command exits, cleans up what is left of those groups the same
//!    way and exits with the command's status (re-raising its signal if it was
//!    killed).
//!
//! On Linux the shim is a child subreaper, so descendants that daemonise stay
//! its descendants.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;

const GROUP_GRACE: Duration = Duration::from_secs(2);
const FORWARDED: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let saved = Errno::last_raw();
    let byte = sig as u8;
    unsafe {
        libc::write(
            SIGNAL_PIPE.load(Ordering::Relaxed),
            (&raw const byte).cast(),
            1,
        )
    };
    Errno::set_raw(saved);
}

/// Runs the shim and returns the exit code to use; exits by re-raising the
/// command's signal when it was killed by one.
pub fn run(lifeline: RawFd, command: &str) -> i32 {
    match run_inner(lifeline, command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("carte shim: {e}");
            125
        }
    }
}

fn run_inner(lifeline: RawFd, command: &str) -> io::Result<i32> {
    close_stray_fds(lifeline);
    set_cloexec(lifeline)?;
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
    }

    let mut fds = [0; 2];
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    let [sig_read, sig_write] = fds;
    for fd in fds {
        set_cloexec(fd)?;
        cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) })?;
    }
    SIGNAL_PIPE.store(sig_write, Ordering::Relaxed);
    for sig in FORWARDED.into_iter().chain([libc::SIGCHLD]) {
        install_handler(sig)?;
    }

    let tty = unsafe { libc::isatty(0) } == 1;
    let (sh, arg0) = shell();
    let command =
        CString::new(command).map_err(|_| io::Error::other("command contains a NUL byte"))?;
    let argv = [
        arg0.as_ptr(),
        c"-c".as_ptr(),
        command.as_ptr(),
        std::ptr::null(),
    ];

    let pid = cvt(unsafe { libc::fork() })?;
    if pid == 0 {
        unsafe { exec_child(tty, sh.as_ptr(), argv.as_ptr()) }
    }

    unsafe {
        libc::setpgid(pid, pid);
        if tty {
            libc::tcsetpgrp(0, pid);
        }
    }
    let _ = write_all(lifeline, &(pid as u32).to_le_bytes());

    let Some(status) = supervise(pid, lifeline, sig_read, poll)? else {
        return Ok(1);
    };
    if group_alive(pid) || !descendant_groups().is_empty() {
        let mut reaped = Some(status);
        terminate_group(pid, &mut reaped);
    }

    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::kill(libc::getpid(), sig);
        }
        return Ok(128 + sig);
    }
    Ok(libc::WEXITSTATUS(status))
}

fn poll(fds: &mut [libc::pollfd]) -> libc::c_int {
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) }
}

/// Forwards signals to the group until the command exits, and returns its
/// status. Takes the group down and returns `None` when the lifeline closes,
/// and takes it down before returning an error.
fn supervise(
    pid: libc::pid_t,
    lifeline: RawFd,
    sig_read: RawFd,
    poll: fn(&mut [libc::pollfd]) -> libc::c_int,
) -> io::Result<Option<libc::c_int>> {
    let mut status = None;
    while status.is_none() {
        let mut pfds = [
            libc::pollfd {
                fd: lifeline,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sig_read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if poll(&mut pfds) < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            terminate_group(pid, &mut status);
            return Err(err);
        }

        if pfds[1].revents != 0 {
            let mut sigs = [0u8; 64];
            let n = unsafe { libc::read(sig_read, sigs.as_mut_ptr().cast(), sigs.len()) };
            for &sig in &sigs[..n.max(0) as usize] {
                if FORWARDED.contains(&libc::c_int::from(sig)) {
                    unsafe { libc::killpg(pid, sig.into()) };
                }
            }
        }

        if pfds[0].revents != 0 && lifeline_closed(lifeline) {
            terminate_group(pid, &mut status);
            return Ok(None);
        }

        reap(pid, &mut status);
    }
    Ok(status)
}

/// Runs in the forked child; only async-signal-safe calls.
/// The shell that runs commands, and its `argv[0]`: bash in POSIX mode, as macOS's
/// `/bin/sh` is, wherever Linux has bash.
fn shell() -> (&'static CStr, &'static CStr) {
    if cfg!(target_os = "linux") && std::path::Path::new("/bin/bash").exists() {
        (c"/bin/bash", c"sh")
    } else {
        (c"/bin/sh", c"/bin/sh")
    }
}

unsafe fn exec_child(tty: bool, path: *const libc::c_char, argv: *const *const libc::c_char) -> ! {
    unsafe {
        libc::setpgid(0, 0);
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        if tty {
            let mut ttou = std::mem::zeroed();
            libc::sigemptyset(&mut ttou);
            libc::sigaddset(&mut ttou, libc::SIGTTOU);
            libc::sigprocmask(libc::SIG_BLOCK, &ttou, std::ptr::null_mut());
            libc::tcsetpgrp(0, libc::getpid());
        }
        let mut default: libc::sigaction = std::mem::zeroed();
        default.sa_sigaction = libc::SIG_DFL;
        for sig in [
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGCHLD,
            libc::SIGPIPE,
        ] {
            libc::sigaction(sig, &default, std::ptr::null_mut());
        }
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        libc::execv(path, argv);
        libc::_exit(127)
    }
}

fn lifeline_closed(fd: RawFd) -> bool {
    let mut buf = [0u8; 64];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        return false;
    }
    !(n < 0 && matches!(Errno::last(), Errno::EINTR | Errno::EAGAIN))
}

/// Reaps every exited child, the leader's status going to `leader`; on Linux
/// the shim also adopts the command's orphaned descendants. A leader stopped by
/// SIGTSTP, SIGTTIN or SIGTTOU has its group continued, and after SIGTTIN or
/// SIGTTOU has the terminal back first.
fn reap(pid: libc::pid_t, leader: &mut Option<libc::c_int>) {
    loop {
        let mut status = 0;
        match unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::WUNTRACED) } {
            p if p == pid && libc::WIFSTOPPED(status) => match libc::WSTOPSIG(status) {
                libc::SIGTSTP => unsafe {
                    libc::killpg(pid, libc::SIGCONT);
                },
                libc::SIGTTIN | libc::SIGTTOU => unsafe {
                    give_terminal(pid);
                    libc::killpg(pid, libc::SIGCONT);
                },
                _ => {}
            },
            p if p == pid => *leader = Some(status),
            p if p > 0 => {}
            -1 if Errno::last() == Errno::ECHILD => {
                leader.get_or_insert(0);
                return;
            }
            _ => return,
        }
    }
}

fn group_alive(pgid: libc::pid_t) -> bool {
    unsafe { libc::killpg(pgid, 0) == 0 || Errno::last() == Errno::EPERM }
}

/// Makes `pgid` the terminal's foreground group again, when stdin is one.
unsafe fn give_terminal(pgid: libc::pid_t) {
    unsafe {
        if libc::isatty(0) != 1 {
            return;
        }
        let mut ttou = std::mem::zeroed();
        let mut old = std::mem::zeroed();
        libc::sigemptyset(&mut ttou);
        libc::sigaddset(&mut ttou, libc::SIGTTOU);
        libc::sigprocmask(libc::SIG_BLOCK, &ttou, &mut old);
        libc::tcsetpgrp(0, pgid);
        libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
    }
}

/// The groups of the shim's descendants other than its own.
fn descendant_groups() -> Vec<libc::pid_t> {
    crate::proctree::descendant_groups(std::process::id())
        .into_iter()
        .map(|pgid| pgid as libc::pid_t)
        .collect()
}

fn signal_groups(groups: &[libc::pid_t], sig: libc::c_int) {
    for &pgid in groups {
        unsafe { libc::killpg(pgid, sig) };
    }
}

/// TERM and CONT to the group and to every group the shim's descendants are
/// in, a short grace, then KILL; reaps the leader on the way.
fn terminate_group(pgid: libc::pid_t, reaped: &mut Option<libc::c_int>) {
    let mut groups = descendant_groups();
    if !groups.contains(&pgid) {
        groups.push(pgid);
    }
    signal_groups(&groups, libc::SIGTERM);
    signal_groups(&groups, libc::SIGCONT);
    let deadline = Instant::now() + GROUP_GRACE;
    loop {
        reap(pgid, reaped);
        if reaped.is_some() && !groups.iter().any(|&g| group_alive(g)) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let earlier: Vec<u32> = groups.iter().map(|&g| g as u32).collect();
    let groups: Vec<libc::pid_t> = crate::proctree::remaining_groups(std::process::id(), &earlier)
        .into_iter()
        .map(|g| g as libc::pid_t)
        .collect();
    signal_groups(&groups, libc::SIGKILL);
    if reaped.is_none() {
        let mut status = 0;
        unsafe { libc::waitpid(pgid, &mut status, 0) };
        *reaped = Some(status);
    }
}

fn install_handler(sig: libc::c_int) -> io::Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        cvt(libc::sigaction(sig, &action, std::ptr::null_mut())).map(drop)
    }
}

/// Closes every inherited fd above stderr except the lifeline: a lifeline
/// leaked into this shim by a concurrent spawn would keep another shim alive.
fn close_stray_fds(keep: RawFd) {
    let fds: Vec<RawFd> = std::fs::read_dir("/dev/fd")
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    for fd in fds {
        if fd > 2 && fd != keep {
            unsafe { libc::close(fd) };
        }
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            if Errno::last() == Errno::EINTR {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn failing_poll(_: &mut [libc::pollfd]) -> libc::c_int {
        Errno::set_raw(libc::EINVAL);
        -1
    }

    /// Plays the shim for `a_failing_poll_takes_the_group_down`, in a process of
    /// its own as it reaps any child; a no-op otherwise.
    #[test]
    fn supervise_with_a_failing_poll() {
        if std::env::var_os("CARTE_TEST_FAILING_POLL").is_none() {
            return;
        }
        let mut child = Command::new("sleep")
            .arg("20")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let (lifeline, _daemon_end) = UnixStream::pair().unwrap();
        let (sig_read, _sig_write) = std::io::pipe().unwrap();
        let err = supervise(
            pid,
            lifeline.as_raw_fd(),
            sig_read.as_raw_fd(),
            failing_poll,
        )
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        assert!(!group_alive(pid));
        assert!(child.try_wait().is_err());
    }

    #[test]
    fn a_failing_poll_takes_the_group_down() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "shim::tests::supervise_with_a_failing_poll"])
            .env("CARTE_TEST_FAILING_POLL", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
