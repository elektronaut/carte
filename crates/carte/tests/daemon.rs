mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{fixture, repo};

struct Daemon {
    _dir: tempfile::TempDir,
    root: PathBuf,
    http_port: u16,
    https_port: u16,
    child: Option<Child>,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fixture = fixture().display().to_string();
        repo(
            &root.join("work/acme-web"),
            &format!(
                "web: FIXTURE_NAME=web FIXTURE_ENV=CARTE_URL exec {fixture}\nworker: FIXTURE_NAME=worker FIXTURE_LINES=2 exec {fixture}\n"
            ),
        );
        let config = root.join("config");
        std::fs::create_dir_all(config.join("projects")).unwrap();
        let (http_port, https_port, dns_port) = (free_port(), free_port(), free_port());
        std::fs::write(
            config.join("config.toml"),
            format!(
                "tld = \"test\"\nhttp_port = {http_port}\nhttps_port = {https_port}\ndns_port = {dns_port}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            config.join("projects/acme.toml"),
            format!(
                "repos = \"{}/work/acme-*\"\nstrip_prefix = \"acme-\"\nenv_provider = \"none\"\n",
                root.display()
            ),
        )
        .unwrap();
        let mut daemon = Daemon {
            _dir: dir,
            root,
            http_port,
            https_port,
            child: None,
        };
        let child = daemon
            .command(&["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        daemon.child = Some(child);
        let socket = daemon.root.join("state/carte.sock");
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "daemon didn't start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        daemon
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carte"));
        command
            .args(args)
            .env("CARTE_CONFIG_DIR", self.root.join("config"))
            .env("CARTE_STATE_DIR", self.root.join("state"))
            .env("NO_COLOR", "1")
            .current_dir(&self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "carte {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn request(&self, host: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.http_port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn stop(&mut self) -> String {
        let mut child = self.child.take().unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(child.wait().unwrap().success(), "{stderr}");
        stderr
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn boots_an_app_on_the_first_request_and_stops_it_on_sigterm() {
    let mut daemon = Daemon::start();

    let response = daemon.request("web.acme.test", "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"name\":\"web\""), "{response}");
    assert!(response.contains("https://web.acme.test"), "{response}");

    let missing = daemon.request("nope.acme.test", "/");
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");

    let ls = stdout(&daemon.run(&["ls"]));
    assert!(ls.contains("acme-web"), "{ls}");
    assert!(ls.contains("ready"), "{ls}");

    let json = stdout(&daemon.run(&["ls", "--json"]));
    let snapshot: carte_api::Snapshot = serde_json::from_str(&json).unwrap();
    let pids: Vec<u32> = snapshot.projects[0].stacks[0].instances[0]
        .processes
        .iter()
        .filter_map(|p| p.pid)
        .collect();
    assert!(!pids.is_empty());

    let logs = stdout(&daemon.run(&["logs", "web", "-n", "50"]));
    assert!(logs.contains("acme-web@main/worker line 1"), "{logs}");

    daemon.stop();
    for pid in pids {
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err(),
            "{pid} survived"
        );
    }
    assert!(!daemon.root.join("state/carte.sock").exists());
}

#[test]
fn serves_https_with_the_local_ca() {
    let daemon = Daemon::start();
    let ca = daemon.root.join("state/ca.pem");
    let url = format!("https://web.acme.test:{}/", daemon.https_port);
    let resolve = format!("web.acme.test:{}:127.0.0.1", daemon.https_port);
    let output = Command::new("curl")
        .args(["-sS", "--max-time", "30", "--cacert"])
        .arg(&ca)
        .args(["--resolve", &resolve, &url])
        .output()
        .unwrap();
    let body = stdout(&output);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(body.contains("\"name\":\"web\""), "{body}");
}

#[test]
fn up_and_down_by_address() {
    let daemon = Daemon::start();
    let up = stdout(&daemon.run(&["up", "web"]));
    assert!(up.contains("starting acme-web@main"), "{up}");

    let started = Instant::now();
    loop {
        let ls = stdout(&daemon.run(&["ls"]));
        if ls.contains("ready") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(15), "{ls}");
        std::thread::sleep(Duration::from_millis(100));
    }

    daemon.run(&["down", "web"]);
    let started = Instant::now();
    loop {
        let ls = stdout(&daemon.run(&["ls"]));
        if ls.contains("Nothing is running") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(15), "{ls}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_second_daemon_refuses_to_start() {
    let daemon = Daemon::start();
    let output = daemon.command(&["daemon"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
}
