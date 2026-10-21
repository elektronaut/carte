#![no_main]

use std::path::Path;

use carte_core::env::parse_env_file;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    if let Err(err) = parse_env_file(Path::new("/w/shop/.env"), text) {
        assert!(err.to_string().starts_with("/w/shop/.env:"));
    }
});
