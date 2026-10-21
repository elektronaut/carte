//! Daemon side of process spawning: every process runs under `carte __shim`
//! on a pty (or pipes), holding a lifeline socket to the daemon.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::termios::{self, InputFlags, SetArg, SpecialCharacterIndices};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::proctree::{descendant_groups, remaining_groups};

/// How a process's stdio is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// A pty of the given size; stdin, stdout and stderr all go through it. Flow
    /// control and the suspend characters are off, so ctrl-s, ctrl-q and ctrl-z
    /// reach the process instead of pausing or stopping it.
    Pty { cols: u16, rows: u16 },
    /// stdin from `/dev/null`, stdout and stderr merged into one pipe.
    Pipes,
}

impl Default for IoMode {
    fn default() -> Self {
        IoMode::Pty {
            cols: 200,
            rows: 50,
        }
    }
}

/// What to run. `env` is the complete environment; nothing is inherited from the daemon.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub io: IoMode,
}

impl SpawnSpec {
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        SpawnSpec {
            command: command.into(),
            cwd: cwd.into(),
            env: Vec::new(),
            io: IoMode::default(),
        }
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn io(mut self, io: IoMode) -> Self {
        self.io = io;
        self
    }
}

/// Spawns processes through the shim binary.
#[derive(Debug, Clone)]
pub struct Spawner {
    shim_exe: PathBuf,
}

impl Spawner {
    /// `shim_exe` is a carte binary; it is run as `shim_exe __shim`.
    pub fn new(shim_exe: impl Into<PathBuf>) -> Self {
        Spawner {
            shim_exe: shim_exe.into(),
        }
    }

    /// Uses the running executable as the shim.
    pub fn current() -> io::Result<Self> {
        Ok(Spawner::new(std::env::current_exe()?))
    }

    pub async fn spawn(&self, spec: &SpawnSpec) -> io::Result<ProcessHandle> {
        let (daemon_end, shim_end) = StdUnixStream::pair()?;
        let shim_fd = shim_end.as_raw_fd();

        let (output_fd, stdio, pty) = match spec.io {
            IoMode::Pty { cols, rows } => {
                let (master, slave) = open_pty()?;
                Pty::resize_fd(master.as_raw_fd(), cols, rows)?;
                let mut modes = termios::tcgetattr(&slave).map_err(io::Error::from)?;
                modes.input_flags.remove(InputFlags::IXON);
                modes.control_chars[SpecialCharacterIndices::VSUSP as usize] =
                    libc::_POSIX_VDISABLE;
                #[cfg(target_os = "macos")]
                {
                    modes.control_chars[SpecialCharacterIndices::VDSUSP as usize] =
                        libc::_POSIX_VDISABLE;
                }
                termios::tcsetattr(&slave, SetArg::TCSANOW, &modes).map_err(io::Error::from)?;
                let stdio = [slave.try_clone()?, slave.try_clone()?, slave];
                (master, stdio, true)
            }
            IoMode::Pipes => {
                let (read, write) = pipe_cloexec()?;
                let null = OwnedFd::from(std::fs::File::open("/dev/null")?);
                (read, [null, write.try_clone()?, write], false)
            }
        };
        set_nonblocking(output_fd.as_raw_fd())?;

        let [stdin, stdout, stderr] = stdio;
        let mut command = Command::new(&self.shim_exe);
        command
            .arg("__shim")
            .arg("--fd")
            .arg(shim_fd.to_string())
            .arg("--")
            .arg(&spec.command)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(false);
        // SAFETY: the closure runs between fork and exec and makes only
        // async-signal-safe calls; `last_os_error` doesn't allocate.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(shim_fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if pty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shim = command.spawn()?;
        drop(command);
        drop(shim_end);

        let mut handle = ProcessHandle {
            shim_pid: shim.id().unwrap_or_default(),
            pid: 0,
            shim,
            lifeline: Some(UnixStream::from_std({
                daemon_end.set_nonblocking(true)?;
                daemon_end
            })?),
            master: None,
            output: None,
            status: None,
        };

        // SAFETY: the AsyncFd owns `output_fd`, which stays open until it is dropped.
        let fd = unsafe { AsyncFd::register(output_fd) }.map_err(|e| e.into_parts().1)?;
        let fd = Arc::new(fd);
        if pty {
            handle.master = Some(fd.clone());
        }
        handle.output = Some(Output { fd });

        let mut pid = [0u8; 4];
        let lifeline = handle.lifeline.as_mut().expect("lifeline");
        match tokio::time::timeout(Duration::from_secs(10), lifeline.read_exact(&mut pid)).await {
            Ok(Ok(_)) => {
                handle.pid = u32::from_le_bytes(pid);
                Ok(handle)
            }
            Ok(Err(_)) => {
                let status = handle.wait().await?;
                Err(io::Error::other(format!(
                    "shim exited before starting the command ({status})"
                )))
            }
            Err(_) => {
                signal_groups(&descendant_groups(handle.shim_pid), Signal::SIGKILL);
                handle.shim.start_kill().ok();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shim did not report the command's pid",
                ))
            }
        }
    }
}

/// A running process. Dropping it closes the lifeline, and the shim then
/// kills the whole process group.
#[derive(Debug)]
pub struct ProcessHandle {
    shim: Child,
    shim_pid: u32,
    pid: u32,
    lifeline: Option<UnixStream>,
    master: Option<Arc<AsyncFd<OwnedFd>>>,
    output: Option<Output>,
    status: Option<ExitStatus>,
}

impl ProcessHandle {
    /// The command's pid (`sh -c` or what it exec'd).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The command's process group, equal to its pid.
    pub fn pgid(&self) -> u32 {
        self.pid
    }

    pub fn shim_pid(&self) -> u32 {
        self.shim_pid
    }

    /// The raw output stream (pty master or the merged stdout/stderr pipe).
    /// Returns `None` after the first call.
    ///
    /// The output must be read continuously until EOF. Unread pty output
    /// blocks the shim's exit indefinitely on macOS, because the kernel
    /// drains a controlling terminal when its session leader exits.
    pub fn take_output(&mut self) -> Option<Output> {
        self.output.take()
    }

    /// Sends a signal to the command's process group. Fails with `ESRCH` once
    /// the process has exited.
    pub fn signal_group(&mut self, signal: Signal) -> io::Result<()> {
        if self.status.is_some() || self.shim.try_wait()?.is_some() {
            return Err(Errno::ESRCH.into());
        }
        killpg(Pid::from_raw(self.pid as i32), signal).map_err(io::Error::from)
    }

    /// Waits for the shim, which exits with the command's status.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self.shim.wait().await?;
        self.status = Some(status);
        Ok(status)
    }

    /// SIGTERM to the group, then SIGKILL if it hasn't exited after `grace`.
    /// SIGCONT follows the SIGTERM, so a stopped group handles it. Groups the
    /// command's descendants run in get the same signals.
    pub async fn stop(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        if self.status.is_some() {
            return self.wait().await;
        }
        let mut others = descendant_groups(self.shim_pid);
        others.retain(|&pgid| pgid != self.pid);
        signal_groups(&others, Signal::SIGTERM);
        ignore_esrch(self.signal_group(Signal::SIGTERM))?;
        signal_groups(&others, Signal::SIGCONT);
        let _ = self.signal_group(Signal::SIGCONT);
        if let Ok(status) = tokio::time::timeout(grace, self.wait()).await {
            return status;
        }
        let mut others = remaining_groups(self.shim_pid, &others);
        others.retain(|&pgid| pgid != self.pid);
        signal_groups(&others, Signal::SIGKILL);
        ignore_esrch(self.signal_group(Signal::SIGKILL))?;
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(5), self.wait()).await {
            return status;
        }
        self.shim.start_kill()?;
        self.wait().await
    }

    /// Writes to the pty, as if typed. Fails in pipe mode.
    pub async fn write_input(&self, data: &[u8]) -> io::Result<()> {
        self.pty()
            .ok_or_else(|| io::Error::other("process has no pty"))?
            .write(data)
            .await
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.pty()
            .ok_or_else(|| io::Error::other("process has no pty"))?
            .resize(cols, rows)
    }

    /// The pty's input and size, for use beside the handle; `None` in pipe mode.
    pub fn pty(&self) -> Option<Pty> {
        self.master.clone().map(|master| Pty { master })
    }

    /// Closes the lifeline without waiting, as a daemon crash would.
    pub fn close_lifeline(&mut self) {
        self.lifeline = None;
    }
}

/// Writes to a process's pty and sets its size.
#[derive(Debug, Clone)]
pub struct Pty {
    master: Arc<AsyncFd<OwnedFd>>,
}

impl Pty {
    /// Writes all of `data`, as if typed.
    ///
    /// A process that doesn't read fills the pty, and on Linux the master keeps
    /// reporting itself writable while each write fails with `EAGAIN`, so a refused
    /// write waits a moment, longer each time, before it tries again.
    pub async fn write(&self, mut data: &[u8]) -> io::Result<()> {
        let mut backoff = Duration::from_millis(1);
        while !data.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| write_fd(fd.as_raw_fd(), data)) {
                Ok(Ok(0)) | Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_millis(100));
                }
                Ok(n) => {
                    data = &data[n?..];
                    backoff = Duration::from_millis(1);
                }
            }
        }
        Ok(())
    }

    /// Whether the process reads its input a key at a time without echo, as a line
    /// editor does, rather than a line at a time or echoed.
    pub fn reads_keys(&self) -> bool {
        nix::sys::termios::tcgetattr(self.master.get_ref()).is_ok_and(|mode| {
            use nix::sys::termios::LocalFlags;
            !mode
                .local_flags
                .intersects(LocalFlags::ICANON | LocalFlags::ECHO)
        })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        Pty::resize_fd(self.master.as_raw_fd(), cols, rows)
    }

    fn resize_fd(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize, and `size` outlives the call.
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Raw output bytes of a process. Reaches EOF when every process holding the
/// pty slave (or pipe) has exited.
#[derive(Debug)]
pub struct Output {
    fd: Arc<AsyncFd<OwnedFd>>,
}

impl AsyncRead for Output {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|fd| read_fd(fd.as_raw_fd(), unfilled)) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Poll::Ready(Ok(())),
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: reads at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes from `buf`.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn signal_groups(groups: &[u32], signal: Signal) {
    for &pgid in groups {
        let _ = killpg(Pid::from_raw(pgid as i32), signal);
    }
}

fn ignore_esrch(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other,
    }
}

/// A pty's master and slave, both opened close-on-exec.
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: posix_openpt takes no pointers.
    let master = cvt(unsafe { libc::posix_openpt(flags) })?;
    // SAFETY: `master` is a new fd that nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: grantpt and unlockpt take no pointers.
    cvt(unsafe { libc::grantpt(master.as_raw_fd()) })?;
    // SAFETY: as above.
    cvt(unsafe { libc::unlockpt(master.as_raw_fd()) })?;
    let mut name = [0 as libc::c_char; 128];
    #[cfg(target_os = "macos")]
    // SAFETY: TIOCPTYGNAME writes a NUL-terminated name of at most 128 bytes into `name`.
    cvt(unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCPTYGNAME as _,
            name.as_mut_ptr(),
        )
    })?;
    #[cfg(target_os = "linux")]
    {
        // SAFETY: ptsname_r writes at most `name.len()` bytes into `name`.
        let err = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err));
        }
    }
    // SAFETY: `name` holds a NUL-terminated path.
    let slave = cvt(unsafe { libc::open(name.as_ptr(), flags) })?;
    // SAFETY: `slave` is a new fd that nothing else owns.
    Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

#[cfg(target_os = "linux")]
fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two fds pipe2 writes.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: both are new fds that nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(not(target_os = "linux"))]
fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two fds pipe writes.
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: both are new fds that nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(read.as_raw_fd(), true)?;
    set_cloexec(write.as_raw_fd(), true)?;
    Ok((read, write))
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub(crate) fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    let flags = if on { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: F_SETFD takes an int argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL takes no argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: F_SETFL takes an int argument.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
