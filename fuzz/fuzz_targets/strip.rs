#![no_main]

use carte_core::ansi::{MAX_LINE, MAX_OPEN_ROWS, Stripper, strip};
use libfuzzer_sys::fuzz_target;

fn plain(text: &str) -> bool {
    text.chars().all(|c| !c.is_control() || c == '\t')
}

fuzz_target!(|data: &[u8]| {
    let Some((&[split, settles], data)) = data.split_first_chunk::<2>() else {
        return;
    };
    let whole = strip(data);
    assert!(
        whole
            .chars()
            .all(|c| !c.is_control() || matches!(c, '\t' | '\n')),
        "{whole:?}"
    );

    let mut stripper = Stripper::default();
    let mut lines = Vec::new();
    for (i, chunk) in data.chunks(usize::from(split).max(1)).enumerate() {
        lines.extend(stripper.feed(chunk));
        if settles >> (i % 8) & 1 == 1 {
            lines.extend(stripper.settle());
        }
        assert!(stripper.pending_len() <= MAX_LINE);
        assert!(
            stripper.open_rows() <= MAX_OPEN_ROWS,
            "{}",
            stripper.open_rows()
        );
    }
    lines.extend(stripper.close());
    for line in &lines {
        assert!(plain(line), "{line:?}");
        assert!(line.chars().count() <= MAX_LINE);
    }
    assert!(plain(&stripper.pending()));
});
