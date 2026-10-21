//! System-wide pty capacity, so the supervisor can keep a reserve for the
//! user's terminals.

use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyCapacity {
    /// The system limit (`kern.tty.ptmx_max` on macOS).
    pub max: usize,
    /// Ptys currently allocated by anyone.
    pub in_use: usize,
}

impl PtyCapacity {
    /// Ptys that can still be allocated while leaving `reserve` free.
    pub fn available(&self, reserve: usize) -> usize {
        self.max.saturating_sub(self.in_use).saturating_sub(reserve)
    }
}

pub fn capacity() -> io::Result<PtyCapacity> {
    Ok(PtyCapacity {
        max: max()?,
        in_use: in_use()?,
    })
}

#[cfg(target_os = "macos")]
pub fn max() -> io::Result<usize> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, and `len` is the size of `value`.
    let ret = unsafe {
        libc::sysctlbyname(
            c"kern.tty.ptmx_max".as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value as usize)
}

/// macOS creates `/dev/ttysNNN` when a pty is allocated and removes it when
/// it is freed, so counting them gives the ptys in use.
#[cfg(target_os = "macos")]
pub fn in_use() -> io::Result<usize> {
    count_numeric_entries("/dev", "ttys")
}

#[cfg(target_os = "linux")]
pub fn max() -> io::Result<usize> {
    read_number("/proc/sys/kernel/pty/max")
}

#[cfg(target_os = "linux")]
pub fn in_use() -> io::Result<usize> {
    read_number("/proc/sys/kernel/pty/nr")
}

#[cfg(target_os = "linux")]
fn read_number(path: &str) -> io::Result<usize> {
    std::fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(target_os = "macos")]
fn count_numeric_entries(dir: &str, prefix: &str) -> io::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(prefix))
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_digit())
        {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_system_limit() {
        let cap = capacity().unwrap();
        assert!(cap.max > 0);
        assert!(cap.in_use <= cap.max);
    }

    #[test]
    fn counts_an_open_pty() {
        let _pty = nix::pty::openpty(None, None).unwrap();
        assert!(in_use().unwrap() >= 1);
    }

    #[test]
    fn available_subtracts_the_reserve() {
        let cap = PtyCapacity {
            max: 511,
            in_use: 20,
        };
        assert_eq!(cap.available(64), 427);
        assert_eq!(
            PtyCapacity {
                max: 100,
                in_use: 90
            }
            .available(64),
            0
        );
    }
}
