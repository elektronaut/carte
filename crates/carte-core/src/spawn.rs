//! Plain child processes that inherit nothing but stdin, stdout and stderr.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// A command for `program` whose child marks every descriptor above stderr
/// close-on-exec before it execs, a socketpair or pipe another thread was
/// still setting up included.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    // SAFETY: the closure runs between fork and exec and makes only
    // async-signal-safe calls.
    unsafe {
        command.pre_exec(|| {
            cloexec_above_stderr();
            Ok(())
        });
    }
    command
}

/// Marks every descriptor above stderr close-on-exec.
/// Runs between fork and exec, so only async-signal-safe calls.
pub fn cloexec_above_stderr() {
    #[cfg(target_os = "linux")]
    // SAFETY: close_range takes no pointers.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == 0
    {
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` outlives the call.
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.min(65536) as libc::c_int
    } else {
        1024
    };
    for fd in 3..max {
        // SAFETY: F_SETFD takes an int argument.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn a_child_inherits_no_descriptor_above_stderr() {
        let (read, write) = std::io::pipe().unwrap();
        for fd in [read.as_raw_fd(), write.as_raw_fd()] {
            // SAFETY: F_SETFD takes an int argument.
            unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        }
        let fd = write.as_raw_fd();
        let output = command("sh")
            .arg("-c")
            .arg(format!("[ -e /dev/fd/{fd} ] && echo open || echo closed"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "closed");
        drop((read, write));
    }
}
