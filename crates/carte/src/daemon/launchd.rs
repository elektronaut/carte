//! Sockets handed over by launchd through the LaunchAgent's `Sockets` entries.

use std::io;
use std::net::{SocketAddr, TcpListener};

use tracing::{info, warn};

use crate::proxy::{Listener, ListenerOptions, Proxy};

/// The proxy's listeners on the sockets `activated` from launchd as `name`, or
/// on sockets bound to `addrs` when there are none. The first address must bind;
/// the others are skipped with a warning when they can't.
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
    activated
        .into_iter()
        .map(|socket| {
            let listener = proxy.listen(socket, options.clone())?;
            if let Ok(local) = listener.local_addr() {
                info!("listening on {local} (socket {name:?} from launchd)");
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

/// Takes ownership of the listening sockets `fds` and marks them close-on-exec.
#[cfg(any(target_os = "macos", test))]
fn listeners(fds: Vec<std::os::fd::RawFd>) -> io::Result<Vec<TcpListener>> {
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

#[cfg(not(target_os = "macos"))]
pub fn activate_socket(_name: &str) -> io::Result<Vec<TcpListener>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn handed_over_sockets_are_closed_on_exec() {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let listeners = listeners(vec![fd]).unwrap();
        let flags = unsafe { libc::fcntl(listeners[0].as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
    }

    #[test]
    fn finds_no_sockets_outside_launchd() {
        assert!(activate_socket("http").unwrap().is_empty());
    }
}
