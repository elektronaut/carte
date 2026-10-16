#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "-p", "fixture-app"])
            .status()
            .expect("cargo build fixture-app");
        assert!(status.success());
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fixture-app")
    })
}

pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "carte")
        .env("GIT_AUTHOR_EMAIL", "carte@example.com")
        .env("GIT_COMMITTER_NAME", "carte")
        .env("GIT_COMMITTER_EMAIL", "carte@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A git repo with a committed Procfile.dev.
pub fn repo(dir: &Path, procfile: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("Procfile.dev"), procfile).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// A plain HTTP/1.0 GET against loopback, returning the body.
pub fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .or_else(|_| TcpStream::connect(("::1", port)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}
