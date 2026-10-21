use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid duration {0:?}: use a number with ms, s, m, h or d, like \"15m\"")]
pub struct DurationError(pub String);

/// Parses durations like `"500ms"`, `"30s"`, `"15m"`, `"2h"`, `"1d"` or `"1h30m"`.
pub fn parse(input: &str) -> Result<Duration, DurationError> {
    let err = || DurationError(input.to_string());
    let mut millis = 0u64;
    let mut digits = String::new();
    let mut chars = input.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: u64 = digits.parse().map_err(|_| err())?;
        digits.clear();
        let unit = match c {
            'm' if chars.next_if_eq(&'s').is_some() => 1,
            's' => 1000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return Err(err()),
        };
        millis = n
            .checked_mul(unit)
            .and_then(|part| millis.checked_add(part))
            .ok_or_else(err)?;
    }
    if !digits.is_empty() || millis == 0 && input.trim().is_empty() {
        return Err(err());
    }
    Ok(Duration::from_millis(millis))
}

/// Formats a duration the way `parse` reads it, using the largest units, with
/// milliseconds when there are some.
pub fn format(duration: Duration) -> String {
    let mut secs = duration.as_secs();
    let millis = duration.subsec_millis();
    if secs == 0 && millis == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    for (unit, size) in [("d", 86400), ("h", 3600), ("m", 60), ("s", 1)] {
        if secs >= size {
            out.push_str(&format!("{}{unit}", secs / size));
            secs %= size;
        }
    }
    if millis > 0 {
        out.push_str(&format!("{millis}ms"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse("1m30s250ms"), Ok(Duration::from_millis(90_250)));
        assert!(parse("99999999999999999999d").is_err());
        assert!(
            parse("999999999999d").is_err(),
            "overflows the milliseconds"
        );
    }

    #[test]
    fn rejects_bare_numbers_and_unknown_units() {
        assert!(parse("15").is_err());
        assert!(parse("15x").is_err());
        assert!(parse("").is_err());
        assert!(parse("m").is_err());
    }

    #[test]
    fn formats_round_trip() {
        assert_eq!(format(Duration::from_secs(5400)), "1h30m");
        assert_eq!(format(Duration::from_secs(900)), "15m");
        assert_eq!(format(Duration::from_millis(500)), "500ms");
        assert_eq!(
            parse(&format(Duration::from_millis(500))),
            Ok(Duration::from_millis(500))
        );
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_millis(1500)), "1s500ms");
        assert_eq!(format(Duration::from_millis(61_250)), "1m1s250ms");
        assert_eq!(parse("1s500ms"), Ok(Duration::from_millis(1500)));
    }
}
