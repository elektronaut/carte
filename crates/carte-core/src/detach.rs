//! The keys that detach `carte attach` from a process, and picking them out of what
//! is typed.

use std::fmt;
use std::str::FromStr;

/// A key sequence like `ctrl-q d`. Typing it detaches; with two different keys,
/// typing the first one twice sends it once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachKeys(Vec<u8>);

impl Default for DetachKeys {
    fn default() -> Self {
        DetachKeys(vec![0x11, b'd'])
    }
}

impl DetachKeys {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Picks the keys out of typed input.
    pub fn filter(&self) -> DetachFilter {
        DetachFilter {
            keys: self.0.clone(),
            matched: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} isn't a key sequence like \"ctrl-q d\"")]
pub struct DetachKeysError(String);

impl FromStr for DetachKeys {
    type Err = DetachKeysError;

    /// Keys separated by spaces or commas: `ctrl-<key>` (also `^<key>`) for a letter
    /// or one of `@[\]^_`, or a single character. One character alone isn't
    /// enough, since it couldn't be typed into the process.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || DetachKeysError(text.to_string());
        let mut keys = Vec::new();
        for token in text.split([' ', ',']).filter(|t| !t.is_empty()) {
            let lower = token.to_ascii_lowercase();
            let control = lower
                .strip_prefix("ctrl-")
                .or_else(|| lower.strip_prefix("ctrl+"))
                .or_else(|| (lower.len() > 1).then(|| lower.strip_prefix('^')).flatten());
            let byte = match control {
                Some(key) => match key.as_bytes() {
                    [c @ (b'a'..=b'z' | b'@' | b'[' | b'\\' | b']' | b'^' | b'_')] => {
                        c.to_ascii_uppercase() & 0x1f
                    }
                    _ => return Err(error()),
                },
                None => match token.as_bytes() {
                    [c @ 0x21..=0x7e] => *c,
                    _ => return Err(error()),
                },
            };
            keys.push(byte);
        }
        if keys.is_empty() || matches!(keys.as_slice(), [0x21..=0x7e]) {
            return Err(error());
        }
        Ok(DetachKeys(keys))
    }
}

impl fmt::Display for DetachKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self
            .0
            .iter()
            .map(|&b| match b {
                0x00..=0x1f => format!("ctrl-{}", ((b | 0x40) as char).to_ascii_lowercase()),
                _ => (b as char).to_string(),
            })
            .collect();
        f.write_str(&words.join(" "))
    }
}

/// Typed input with the detach keys taken out. A key that may start the sequence is
/// held back until the next one shows whether it does.
#[derive(Debug, Clone)]
pub struct DetachFilter {
    keys: Vec<u8>,
    matched: usize,
}

impl DetachFilter {
    /// The bytes to send on, and whether the keys were typed; what follows them is
    /// dropped.
    pub fn feed(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            if b == self.keys[self.matched] {
                self.matched += 1;
                if self.matched == self.keys.len() {
                    self.matched = 0;
                    return (out, true);
                }
                continue;
            }
            if self.matched == 1 && b == self.keys[0] {
                out.push(b);
                self.matched = 0;
                continue;
            }
            out.extend_from_slice(&self.keys[..self.matched]);
            self.matched = usize::from(b == self.keys[0]);
            if self.matched == 0 {
                out.push(b);
            }
        }
        (out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> DetachKeys {
        text.parse().unwrap()
    }

    #[test]
    fn parses_and_shows_key_sequences() {
        assert_eq!(keys("ctrl-q d"), DetachKeys::default());
        assert_eq!(keys("Ctrl-Q,d").bytes(), [0x11, b'd']);
        assert_eq!(keys("^]").bytes(), [0x1d]);
        assert_eq!(keys("ctrl-\\").bytes(), [0x1c]);
        assert_eq!(keys("~ .").bytes(), b"~.");
        assert_eq!(DetachKeys::default().to_string(), "ctrl-q d");
        assert_eq!(keys("ctrl-] ~").to_string(), "ctrl-] ~");
        for bad in ["", " , ", "ctrl-", "ctrl-1", "ctrl-qq", "dd", "é", "d", "~"] {
            assert!(bad.parse::<DetachKeys>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_sequence_detaches_and_drops_what_follows() {
        let mut filter = DetachKeys::default().filter();
        assert_eq!(filter.feed(b"ls\r"), (b"ls\r".to_vec(), false));
        assert_eq!(filter.feed(b"c\x11dmore"), (b"c".to_vec(), true));
    }

    #[test]
    fn the_sequence_may_span_reads() {
        let mut filter = DetachKeys::default().filter();
        assert_eq!(filter.feed(b"x\x11"), (b"x".to_vec(), false));
        assert_eq!(filter.feed(b"d"), (Vec::new(), true));
    }

    #[test]
    fn the_first_key_twice_sends_it_once() {
        let mut filter = DetachKeys::default().filter();
        assert_eq!(filter.feed(b"\x11\x11a"), (b"\x11a".to_vec(), false));
        assert_eq!(filter.feed(b"\x11"), (Vec::new(), false));
        assert_eq!(filter.feed(b"\x11"), (b"\x11".to_vec(), false));
    }

    #[test]
    fn a_false_start_is_sent_on() {
        let mut filter = DetachKeys::default().filter();
        assert_eq!(filter.feed(b"\x11x"), (b"\x11x".to_vec(), false));
        let mut filter = keys("~ .").filter();
        assert_eq!(filter.feed(b"~~."), (b"~.".to_vec(), false));
        assert_eq!(filter.feed(b"~."), (Vec::new(), true));
        let mut filter = keys("ctrl-a ctrl-b ctrl-c").filter();
        assert_eq!(filter.feed(b"\x01\x02x"), (b"\x01\x02x".to_vec(), false));
        assert_eq!(
            filter.feed(b"\x01\x02\x01\x02\x03"),
            (b"\x01\x02".to_vec(), true)
        );
    }

    #[test]
    fn a_single_key_detaches_at_once() {
        let mut filter = keys("ctrl-]").filter();
        assert_eq!(filter.feed(b"ab\x1d"), (b"ab".to_vec(), true));
    }
}
