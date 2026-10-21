#![no_main]

use carte_core::procfile::{parse_procfile, rewrite_port};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let parsed = parse_procfile(text);
    for (_, command) in &parsed.entries {
        let _ = rewrite_port(command);
    }
});
