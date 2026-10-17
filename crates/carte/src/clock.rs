//! Wall-clock times as people read them.

/// `14:32:05` in the local time zone, for milliseconds since the Unix epoch.
pub fn local_hms(at_ms: u64) -> String {
    let secs = (at_ms / 1000) as libc::time_t;
    // SAFETY: `tm` is plain data that localtime_r fills in; both pointers are valid.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// How long ago, roughly: `12s`, `3m`, `2h`, `4d`.
pub fn ago(at_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_time_of_day() {
        let hms = local_hms(1_700_000_000_000);
        assert_eq!(hms.len(), 8);
        assert_eq!(&hms[2..3], ":");
        assert_eq!(&hms[6..], "20");
    }

    #[test]
    fn rounds_ages_down_to_one_unit() {
        let now = 10_000_000_000;
        assert_eq!(ago(now - 5_000, now), "5s");
        assert_eq!(ago(now - 185_000, now), "3m");
        assert_eq!(ago(now - 2 * 3_600_000 - 1, now), "2h");
        assert_eq!(ago(now - 3 * 86_400_000, now), "3d");
        assert_eq!(ago(now + 1_000, now), "0s");
    }
}
