#![no_main]

use std::path::Path;

use carte_core::config::{parse_project, parse_repo_config};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let project = Path::new("/c/projects/shop.toml");
    if let Err(err) = parse_project(project, text) {
        assert_eq!(err.path(), project);
        assert!(err.to_string().starts_with("/c/projects/shop.toml"));
    }
    let repo = Path::new("/w/shop/carte.toml");
    if let Err(err) = parse_repo_config(repo, text) {
        assert_eq!(err.path(), repo);
        assert!(err.to_string().starts_with("/w/shop/carte.toml"));
    }
});
