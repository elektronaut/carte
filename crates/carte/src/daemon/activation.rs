//! Sockets handed over by launchd through the LaunchAgent's `Sockets` entries, or
//! by systemd through socket activation.

use std::io;
use std::net::{SocketAddr, TcpListener};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::os::fd::RawFd;

use tracing::{info, warn};

use crate::proxy::{Listener, ListenerOptions, Proxy};

/// The proxy's listeners on the sockets `activated` as `name`, or on sockets
/// bound to `addrs` when there are none. The first address must bind; the others
/// are skipped with a warning when they can't.
pub async fn proxy_listeners(
    proxy: &Proxy,
    name: &str,
    activated: Vec<TcpListener>,
    addrs: &[SocketAddr],
    options: ListenerOptions,
) -> io::Result<Vec<Listener>> {
    if activated.is_empty() {
        let mut listeners = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            match proxy.bind(*addr, options.clone()).await {
                Ok(listener) => listeners.push(listener),
                Err(err) if i > 0 => warn!("not listening on {addr}: {err}"),
                Err(err) => return Err(err),
            }
        }
        return Ok(listeners);
    }
    let manager = if cfg!(target_os = "macos") {
        "launchd"
    } else {
        "systemd"
    };
    activated
        .into_iter()
        .map(|socket| {
            let listener = proxy.listen(socket, options.clone())?;
            if let Ok(local) = listener.local_addr() {
                info!("listening on {local} (socket {name:?} from {manager})");
                if let Some(port) = addrs.first().map(SocketAddr::port)
                    && local.port() != port
                {
                    warn!(
                        "socket {name:?} from {manager} is on port {}, but URLs use port {port} from the machine config",
                        local.port()
                    );
                }
            }
            Ok(listener)
        })
        .collect()
}

/// The listening sockets launchd holds for `name`; empty when the process wasn't
/// started by launchd or its job has no such socket.
#[cfg(target_os = "macos")]
pub fn activate_socket(name: &str) -> io::Result<Vec<TcpListener>> {
    use std::ffi::CString;

    use libc::{c_char, c_int, size_t};

    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            cnt: *mut size_t,
        ) -> c_int;
    }

    let name = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut fds: *mut c_int = std::ptr::null_mut();
    let mut count: size_t = 0;
    // SAFETY: `name` is a valid C string and both out-pointers point at live locals.
    let err = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
    match err {
        0 => {}
        libc::ESRCH | libc::ENOENT => return Ok(Vec::new()),
        err => return Err(io::Error::from_raw_os_error(err)),
    }
    if fds.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success launchd returns a malloc'ed array of `count` descriptors that
    // the caller owns, and frees nothing itself.
    let raw = unsafe { std::slice::from_raw_parts(fds, count) }.to_vec();
    // SAFETY: `fds` came from malloc in launchd and is freed exactly once.
    unsafe { libc::free(fds.cast()) };
    listeners(raw)
}

#[cfg(target_os = "linux")]
static SYSTEMD_SOCKETS: std::sync::Mutex<Vec<(String, RawFd)>> = std::sync::Mutex::new(Vec::new());

/// Takes the sockets systemd passed through `LISTEN_PID`, `LISTEN_FDS` and
/// `LISTEN_FDNAMES` for [`activate_socket`], and unsets those variables so the
/// processes the daemon starts don't see them. Call before starting any threads.
#[cfg(target_os = "linux")]
pub fn take_systemd_sockets() {
    let keys = ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"];
    let [pid, fds, names] = keys.map(|key| std::env::var(key).ok());
    for key in keys {
        // SAFETY: the caller runs no other threads yet.
        unsafe { std::env::remove_var(key) };
    }
    let count = match systemd_socket_count(std::process::id(), pid.as_deref(), fds.as_deref()) {
        Ok(count) => count,
        Err(err) => {
            warn!("ignoring the sockets from systemd: {err}");
            return;
        }
    };
    let mut sockets = match systemd_sockets(count, names.as_deref()) {
        Ok(sockets) => sockets,
        Err(err) => {
            warn!("ignoring the sockets from systemd: {err}");
            Vec::new()
        }
    };
    let mut open = Vec::new();
    for fd in (3..).take(count) {
        match crate::process::set_cloexec(fd, true) {
            Ok(()) => open.push(fd),
            Err(err) => warn!("ignoring socket {fd} from systemd: {err}"),
        }
    }
    sockets.retain(|(_, fd)| open.contains(fd));
    *SYSTEMD_SOCKETS.lock().unwrap_or_else(|e| e.into_inner()) = sockets;
}

#[cfg(not(target_os = "linux"))]
pub fn take_systemd_sockets() {}

/// Warns about the sockets systemd passed that no [`activate_socket`] took.
#[cfg(target_os = "linux")]
pub fn warn_unclaimed_sockets() {
    let sockets = std::mem::take(&mut *SYSTEMD_SOCKETS.lock().unwrap_or_else(|e| e.into_inner()));
    for (name, fd) in sockets {
        warn!(
            "ignoring socket {name:?} from systemd (fd {fd}); \
             set FileDescriptorName=http or https in its socket unit"
        );
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warn_unclaimed_sockets() {}

/// How many descriptors systemd passed to `pid` from `LISTEN_PID` and `LISTEN_FDS`;
/// none when the variables are missing or meant for another process.
#[cfg(any(target_os = "linux", test))]
fn systemd_socket_count(
    pid: u32,
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
) -> Result<usize, String> {
    let (Some(listen_pid), Some(listen_fds)) = (listen_pid, listen_fds) else {
        return Ok(0);
    };
    if listen_pid.parse::<u32>().ok() != Some(pid) {
        return Ok(0);
    }
    listen_fds
        .parse()
        .ok()
        .filter(|count| *count <= 1024)
        .ok_or_else(|| format!("LISTEN_FDS={listen_fds:?} isn't a number of sockets"))
}

/// The `count` descriptors systemd passed, from fd 3 on, with their names from
/// `LISTEN_FDNAMES`.
#[cfg(any(target_os = "linux", test))]
fn systemd_sockets(
    count: usize,
    listen_fdnames: Option<&str>,
) -> Result<Vec<(String, RawFd)>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = match listen_fdnames {
        Some(names) => names.split(':').collect(),
        None => vec!["unknown"; count],
    };
    if names.len() != count {
        return Err(format!(
            "LISTEN_FDNAMES has {} names for {count} sockets",
            names.len()
        ));
    }
    Ok(names
        .into_iter()
        .zip(3..)
        .map(|(name, fd)| (name.to_string(), fd))
        .collect())
}

/// The listening sockets systemd passed as `name`, each taken once; empty when
/// the daemon wasn't socket-activated or systemd passed no such socket.
#[cfg(target_os = "linux")]
pub fn activate_socket(name: &str) -> io::Result<Vec<TcpListener>> {
    let fds: Vec<RawFd> = {
        let mut sockets = SYSTEMD_SOCKETS.lock().unwrap_or_else(|e| e.into_inner());
        let (named, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *sockets)
            .into_iter()
            .partition(|(n, _)| n == name);
        *sockets = rest;
        named.into_iter().map(|(_, fd)| fd).collect()
    };
    if let Some(fd) = fds.iter().find(|fd| !is_listening_tcp(**fd)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket {name:?} from systemd (fd {fd}) isn't a listening TCP socket"),
        ));
    }
    listeners(fds)
}

/// Whether `fd` is a listening IPv4 or IPv6 stream socket.
#[cfg(target_os = "linux")]
fn is_listening_tcp(fd: RawFd) -> bool {
    let option = |name| {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `value` and `len` are live locals of the sizes passed.
        let ret = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw mut value).cast(),
                &mut len,
            )
        };
        (ret == 0).then_some(value)
    };
    option(libc::SO_ACCEPTCONN) == Some(1)
        && option(libc::SO_TYPE) == Some(libc::SOCK_STREAM)
        && matches!(
            option(libc::SO_DOMAIN),
            Some(libc::AF_INET | libc::AF_INET6)
        )
}

/// Takes ownership of the listening sockets `fds` and marks them close-on-exec.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn listeners(fds: Vec<RawFd>) -> io::Result<Vec<TcpListener>> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let listeners: Vec<TcpListener> = fds
        .into_iter()
        // SAFETY: each descriptor is an open listening socket now owned by us alone.
        .map(|fd| unsafe { TcpListener::from_raw_fd(fd) })
        .collect();
    for listener in &listeners {
        crate::process::set_cloexec(listener.as_raw_fd(), true)?;
    }
    Ok(listeners)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn activate_socket(_name: &str) -> io::Result<Vec<TcpListener>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn handed_over_sockets_are_closed_on_exec() {
        // SAFETY: socket takes no pointers.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        assert_eq!(
            // SAFETY: F_GETFD takes no argument.
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let listeners = listeners(vec![fd]).unwrap();
        // SAFETY: F_GETFD takes no argument.
        let flags = unsafe { libc::fcntl(listeners[0].as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
    }

    #[test]
    fn finds_no_sockets_without_activation() {
        assert!(activate_socket("http").unwrap().is_empty());
    }

    fn named(pairs: &[(&str, RawFd)]) -> Vec<(String, RawFd)> {
        pairs.iter().map(|(n, fd)| (n.to_string(), *fd)).collect()
    }

    #[test]
    fn systemd_sockets_need_both_variables() {
        assert_eq!(systemd_socket_count(42, None, None), Ok(0));
        assert_eq!(systemd_socket_count(42, Some("42"), None), Ok(0));
        assert_eq!(systemd_socket_count(42, None, Some("1")), Ok(0));
        assert_eq!(systemd_socket_count(42, Some("42"), Some("2")), Ok(2));
    }

    #[test]
    fn systemd_sockets_for_another_process_are_ignored() {
        assert_eq!(systemd_socket_count(42, Some("43"), Some("1")), Ok(0));
        assert_eq!(systemd_socket_count(42, Some("nova"), Some("1")), Ok(0));
        assert_eq!(systemd_socket_count(42, Some("43"), Some("many")), Ok(0));
    }

    #[test]
    fn systemd_socket_count_must_be_a_number() {
        assert!(systemd_socket_count(42, Some("42"), Some("-1")).is_err());
        assert!(systemd_socket_count(42, Some("42"), Some("many")).is_err());
        assert!(systemd_socket_count(42, Some("42"), Some("100000")).is_err());
        assert_eq!(systemd_socket_count(42, Some("42"), Some("0")), Ok(0));
    }

    #[test]
    fn systemd_sockets_are_named_from_fd_3() {
        assert_eq!(
            systemd_sockets(2, Some("http:https")),
            Ok(named(&[("http", 3), ("https", 4)]))
        );
        assert_eq!(
            systemd_sockets(3, Some("https:http:http")),
            Ok(named(&[("https", 3), ("http", 4), ("http", 5)]))
        );
    }

    #[test]
    fn unnamed_systemd_sockets_are_unknown() {
        assert_eq!(
            systemd_sockets(2, None),
            Ok(named(&[("unknown", 3), ("unknown", 4)]))
        );
        assert_eq!(
            systemd_sockets(2, Some("http:")),
            Ok(named(&[("http", 3), ("", 4)]))
        );
    }

    #[test]
    fn systemd_sockets_must_match_their_names() {
        assert!(systemd_sockets(1, Some("http:https")).is_err());
        assert!(systemd_sockets(3, Some("http:https")).is_err());
        assert_eq!(systemd_sockets(0, None), Ok(Vec::new()));
        assert_eq!(systemd_sockets(0, Some("http")), Ok(Vec::new()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_listening_tcp_sockets_are_taken() {
        use std::os::fd::{FromRawFd, OwnedFd};

        let listening = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(is_listening_tcp(listening.as_raw_fd()));
        let connected = std::net::TcpStream::connect(listening.local_addr().unwrap()).unwrap();
        assert!(!is_listening_tcp(connected.as_raw_fd()));
        // SAFETY: socket takes no pointers.
        let unbound = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(unbound >= 0);
        // SAFETY: the test just created `unbound` and nothing else owns it.
        let unbound = unsafe { OwnedFd::from_raw_fd(unbound) };
        assert!(!is_listening_tcp(unbound.as_raw_fd()));
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(!is_listening_tcp(udp.as_raw_fd()));
        let dir = tempfile::tempdir().unwrap();
        let unix = std::os::unix::net::UnixListener::bind(dir.path().join("s")).unwrap();
        assert!(!is_listening_tcp(unix.as_raw_fd()));
    }
}
