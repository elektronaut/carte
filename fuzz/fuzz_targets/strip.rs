#![no_main]

use carte_core::ansi::{Stripper, strip};
use libfuzzer_sys::fuzz_target;

const MAX_LINE: usize = 16 * 1024;

fn plain(text: &str) -> bool {
    text.chars().all(|c| !c.is_control() || c == '\t')
}

fuzz_target!(|data: &[u8]| {
    let Some((&split, data)) = data.split_first() else {
        return;
    };
    let whole = strip(data);
    assert!(whole.lines().all(plain), "{whole:?}");

    let mut stripper = Stripper::default();
    let mut lines = Vec::new();
    for chunk in data.chunks(usize::from(split).max(1)) {
        lines.extend(stripper.feed(chunk));
        assert!(stripper.pending_len() <= MAX_LINE);
        if chunk.len() % 7 == 0 {
            lines.extend(stripper.settle());
        }
    }
    lines.extend(stripper.close());
    for line in &lines {
        assert!(plain(line), "{line:?}");
        assert!(line.chars().count() <= MAX_LINE);
    }
    assert!(plain(&stripper.pending()));
});
