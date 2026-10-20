//! Memory and CPU use of process groups.

use std::time::Duration;

/// What a process group uses right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupUsage {
    /// The sum of the members' physical footprints, which, unlike RSS, doesn't
    /// count memory forked workers share twice.
    pub memory_bytes: u64,
    /// CPU time the members have used so far.
    pub cpu_time: Duration,
}

/// CPU use between two samples of the same group, in percent of one core.
pub fn cpu_percent(before: Duration, after: Duration, elapsed: Duration) -> f32 {
    if elapsed.is_zero() {
        return 0.0;
    }
    after.saturating_sub(before).as_secs_f64() as f32 / elapsed.as_secs_f64() as f32 * 100.0
}

#[cfg(target_os = "macos")]
pub fn group_usage(pgid: u32) -> Option<GroupUsage> {
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0 as libc::c_int; 256];
    let size = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    let bytes =
        unsafe { libc::proc_listpids(PROC_PGRP_ONLY, pgid, pids.as_mut_ptr().cast(), size) };
    if bytes <= 0 {
        return None;
    }
    pids.truncate(bytes as usize / std::mem::size_of::<libc::c_int>());

    let timebase = timebase();
    let mut usage = GroupUsage::default();
    for pid in pids.into_iter().filter(|&pid| pid > 0) {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        if rc != 0 {
            continue;
        }
        usage.memory_bytes += info.ri_phys_footprint;
        let ticks = info.ri_user_time + info.ri_system_time;
        usage.cpu_time +=
            Duration::from_nanos(ticks * u64::from(timebase.0) / u64::from(timebase.1));
    }
    Some(usage)
}

/// `rusage` CPU times are in Mach absolute time units, which aren't nanoseconds on
/// Apple silicon.
#[cfg(target_os = "macos")]
fn timebase() -> (u32, u32) {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
        return (1, 1);
    }
    (info.numer, info.denom)
}

#[cfg(target_os = "linux")]
pub fn group_usage(pgid: u32) -> Option<GroupUsage> {
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let mut usage = GroupUsage::default();
    let mut found = false;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 2..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.get(2).and_then(|f| f.parse::<u32>().ok()) != Some(pgid) {
            continue;
        }
        found = true;
        let ticks: u64 = [11, 12]
            .iter()
            .filter_map(|&i| fields.get(i)?.parse::<u64>().ok())
            .sum();
        usage.cpu_time += Duration::from_millis(ticks * 1000 / ticks_per_sec);
        if let Some(rss) = fields.get(21).and_then(|f| f.parse::<u64>().ok()) {
            usage.memory_bytes += rss * page;
        }
    }
    found.then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_our_own_group() {
        let pgid = nix::unistd::getpgrp().as_raw() as u32;
        let usage = group_usage(pgid).expect("our own process group");
        assert!(usage.memory_bytes > 0);
    }

    #[test]
    fn cpu_time_is_in_real_seconds() {
        use std::os::unix::process::CommandExt;
        let id = std::process::Command::new("yes")
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
            .id();
        let pid = id as libc::pid_t;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while group_usage(id).unwrap().cpu_time < Duration::from_millis(200)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        let measured = group_usage(id).unwrap().cpu_time.as_secs_f64();

        unsafe { libc::kill(pid, libc::SIGKILL) };
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        let actual = seconds(rusage.ru_utime) + seconds(rusage.ru_stime);
        assert!(actual >= 0.1, "yes got only {actual}s of CPU");
        assert!(
            (measured - actual).abs() <= actual * 0.1 + 0.02,
            "measured {measured}s of CPU, the kernel says {actual}s"
        );
    }

    #[test]
    fn missing_groups_are_none() {
        assert_eq!(group_usage(999_999), None);
    }

    #[test]
    fn cpu_percent_is_relative_to_wall_time() {
        let percent = cpu_percent(
            Duration::from_millis(100),
            Duration::from_millis(600),
            Duration::from_secs(1),
        );
        assert!((percent - 50.0).abs() < 0.01);
        assert_eq!(
            cpu_percent(Duration::ZERO, Duration::ZERO, Duration::ZERO),
            0.0
        );
    }
}
