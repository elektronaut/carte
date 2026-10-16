mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{fixture, git, repo};

struct Daemon {
    _dir: tempfile::TempDir,
    root: PathBuf,
    http_port: u16,
    https_port: u16,
    share_port: Option<u16>,
    /// Searched before the inherited `PATH`, for fake tunnel programs.
    bin: Option<PathBuf>,
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
        Self::start_with(false)
    }

    /// With a `[share]` section whose provider is `none`, so no real tunnel runs.
    fn start_sharing() -> Self {
        Self::start_sharing_with("none", "share.example")
    }

    /// Sharing through ngrok, with a fake `ngrok` that records its arguments in
    /// `bin/ngrok.args` and reports a tunnel the way the real agent logs it.
    fn start_ngrok() -> Self {
        Self::start_sharing_with("ngrok", "acme-dev.ngrok-free.app")
    }

    fn start_sharing_with(provider: &str, domain: &str) -> Self {
        let share_port = free_port();
        let mut daemon = Self::start_configured(
            false,
            &format!(
                "\n[share]\nprovider = \"{provider}\"\ndomain = \"{domain}\"\nport = {share_port}\n"
            ),
        );
        daemon.share_port = Some(share_port);
        daemon
    }

    /// With `linked_config`, the config directory is reached through a symlink, like
    /// a config under `/tmp` on macOS.
    fn start_with(linked_config: bool) -> Self {
        Self::start_configured(linked_config, "")
    }

    fn start_configured(linked_config: bool, extra_config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fixture = fixture().display().to_string();
        repo(
            &root.join("work/acme-web"),
            &format!(
                "web: FIXTURE_NAME=web FIXTURE_ENV=CARTE_URL,CARTE_PUBLIC_URL exec {fixture}\nworker: FIXTURE_NAME=worker FIXTURE_LINES=2 exec {fixture}\n"
            ),
        );
        std::fs::create_dir_all(root.join("work/acme-web/tmp")).unwrap();
        let config = root.join("config");
        if linked_config {
            std::fs::create_dir_all(root.join("real-config/projects")).unwrap();
            std::os::unix::fs::symlink(root.join("real-config"), &config).unwrap();
        } else {
            std::fs::create_dir_all(config.join("projects")).unwrap();
        }
        let (http_port, https_port, dns_port) = (free_port(), free_port(), free_port());
        std::fs::write(
            config.join("config.toml"),
            format!(
                "tld = \"test\"\nhttp_port = {http_port}\nhttps_port = {https_port}\ndns_port = {dns_port}\n{extra_config}"
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
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ngrok = bin.join("ngrok");
        std::fs::write(
            &ngrok,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/ngrok.args\"\necho 't=2026-10-17T10:00:00+0200 lvl=info msg=\"started tunnel\" obj=tunnels name=command_line'\nexec sleep 600\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ngrok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut daemon = Daemon {
            _dir: dir,
            root,
            http_port,
            https_port,
            share_port: None,
            bin: Some(bin),
            child: None,
        };
        let child = daemon
            .command(&["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        daemon.child = Some(child);
        let socket = carte::paths::Paths::under(&daemon.root).socket();
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
        if let Some(bin) = &self.bin {
            let inherited = std::env::var_os("PATH").unwrap_or_default();
            let path = std::env::join_paths(
                std::iter::once(bin.clone()).chain(std::env::split_paths(&inherited)),
            )
            .unwrap();
            command.env("PATH", path);
        }
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
    assert!(!carte::paths::Paths::under(&daemon.root).socket().exists());
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

fn snapshot(daemon: &Daemon) -> carte_api::Snapshot {
    serde_json::from_str(&stdout(&daemon.run(&["ls", "--json"]))).unwrap()
}

fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn web_pid(daemon: &Daemon) -> Option<u32> {
    snapshot(daemon).projects[0].stacks[0].instances[0]
        .processes
        .iter()
        .find(|p| p.name == "web" && p.state == carte_api::ProcessState::Running)
        .and_then(|p| p.pid)
}

#[test]
fn touching_restart_txt_restarts_the_instance() {
    let daemon = Daemon::start();
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
    let before = web_pid(&daemon).unwrap();
    std::fs::write(daemon.root.join("work/acme-web/tmp/restart.txt"), "").unwrap();
    eventually("a new web process", || {
        web_pid(&daemon).is_some_and(|pid| pid != before)
    });
}

#[test]
fn new_worktrees_become_stacks() {
    let daemon = Daemon::start();
    git(
        &daemon.root.join("work/acme-web"),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature-x",
            "../acme-web-feature-x",
        ],
    );
    eventually("the feature-x stack", || {
        snapshot(&daemon).projects[0]
            .stacks
            .iter()
            .any(|s| s.name == "feature-x")
    });
    let response = daemon.request("web.feature-x.acme.test", "/");
    assert!(
        response.contains("https://web.feature-x.acme.test"),
        "{response}"
    );
}

#[test]
fn config_changes_are_picked_up() {
    let daemon = Daemon::start();
    repo(
        &daemon.root.join("other/shop"),
        &format!("web: exec {}\n", fixture().display()),
    );
    std::fs::write(
        daemon.root.join("config/projects/shop.toml"),
        format!(
            "repos = \"{}/other/shop\"\nenv_provider = \"none\"\n",
            daemon.root.display()
        ),
    )
    .unwrap();
    eventually("the shop project", || {
        snapshot(&daemon).projects.iter().any(|p| p.name == "shop")
    });
    assert!(daemon.request("shop.test", "/").starts_with("HTTP/1.1 200"));
}

#[test]
fn env_shows_the_assigned_port_while_running() {
    let daemon = Daemon::start();
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
    let port = snapshot(&daemon).projects[0].stacks[0].instances[0]
        .processes
        .iter()
        .find(|p| p.name == "web")
        .and_then(|p| p.port)
        .unwrap();
    let env: carte_api::EnvResponse =
        serde_json::from_str(&stdout(&daemon.run(&["env", "web", "--json"]))).unwrap();
    let value = |key: &str| {
        env.vars
            .iter()
            .find(|v| v.key == key)
            .map(|v| v.value.clone())
    };
    assert_eq!(value("PORT"), Some(port.to_string()));
    assert_eq!(value("CARTE_STATE_DIR"), None);
}

#[test]
fn config_changes_are_seen_through_a_symlinked_config_dir() {
    let daemon = Daemon::start_with(true);
    std::fs::remove_file(daemon.root.join("config/projects/acme.toml")).unwrap();
    let started = Instant::now();
    while !snapshot(&daemon).projects.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the removed project is still there; only the 60 s rescan would find it"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Daemon {
    /// A request as the tunnel would send it to the share listener.
    fn public_request(&self, host: &str, auth: Option<&str>) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.share_port.unwrap())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let auth = auth
            .map(|a| format!("Authorization: Basic {a}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}

#[test]
fn shares_reach_only_shared_apps_and_carry_the_public_url() {
    let daemon = Daemon::start_sharing();
    let unshared = daemon.public_request("web--acme.share.example", None);
    assert!(unshared.starts_with("HTTP/1.1 404"), "{unshared}");

    let out = stdout(&daemon.run(&["share", "web"]));
    assert!(out.contains("https://web--acme.share.example"), "{out}");
    let shared = daemon.public_request("web--acme.share.example", None);
    assert!(shared.starts_with("HTTP/1.1 200"), "{shared}");
    assert!(
        shared.contains("\"CARTE_PUBLIC_URL\":\"https://web--acme.share.example\""),
        "{shared}"
    );
    let ls = stdout(&daemon.run(&["ls"]));
    assert!(
        ls.contains("public: acme-web@main  https://web--acme.share.example"),
        "{ls}"
    );
    let snapshot = snapshot(&daemon);
    assert_eq!(snapshot.shares[0].requests.len(), 1);
    assert_eq!(
        snapshot.projects[0].stacks[0].instances[0]
            .public_url
            .as_deref(),
        Some("https://web--acme.share.example")
    );

    daemon.run(&["unshare", "web"]);
    let after = daemon.public_request("web--acme.share.example", None);
    assert!(after.starts_with("HTTP/1.1 404"), "{after}");
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
}

#[test]
fn password_protected_shares_ask_for_credentials() {
    use base64::Engine;
    let daemon = Daemon::start_sharing();
    let json = stdout(&daemon.run(&["share", "web", "--auth", "--json"]));
    let response: carte_api::ShareResponse = serde_json::from_str(&json).unwrap();
    let credentials = response.credentials.unwrap();

    let refused = daemon.public_request("web--acme.share.example", None);
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    assert!(
        refused.contains("www-authenticate: Basic realm=\"carte\""),
        "{refused}"
    );
    let wrong = base64::engine::general_purpose::STANDARD.encode("carte:nope");
    assert!(
        daemon
            .public_request("web--acme.share.example", Some(&wrong))
            .starts_with("HTTP/1.1 401")
    );

    let right = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", credentials.user, credentials.password));
    let allowed = daemon.public_request("web--acme.share.example", Some(&right));
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
    let requests = &snapshot(&daemon).shares[0].requests;
    assert_eq!(requests.iter().filter(|r| r.refused).count(), 2);
}

#[test]
fn ngrok_carries_the_shared_app_on_its_dev_domain() {
    let daemon = Daemon::start_ngrok();
    let out = stdout(&daemon.run(&["share", "web"]));
    assert!(out.contains("https://acme-dev.ngrok-free.app"), "{out}");
    let shared = daemon.public_request("acme-dev.ngrok-free.app", None);
    assert!(shared.starts_with("HTTP/1.1 200"), "{shared}");
    assert!(
        shared.contains("\"CARTE_PUBLIC_URL\":\"https://acme-dev.ngrok-free.app\""),
        "{shared}"
    );
    eventually("the tunnel connected", || {
        snapshot(&daemon)
            .tunnel
            .is_some_and(|t| t.state == carte_api::TunnelState::Connected)
    });
    let args = std::fs::read_to_string(daemon.root.join("bin/ngrok.args")).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "http http://127.0.0.1:{} --url https://acme-dev.ngrok-free.app --log stdout --log-format logfmt",
            daemon.share_port.unwrap()
        )
    );

    daemon.run(&["unshare", "web"]);
    eventually("the tunnel stopped", || {
        snapshot(&daemon)
            .tunnel
            .is_some_and(|t| t.state == carte_api::TunnelState::Idle)
    });
    let after = daemon.public_request("acme-dev.ngrok-free.app", None);
    assert!(after.starts_with("HTTP/1.1 404"), "{after}");
}

#[test]
fn sharing_without_a_share_section_explains_itself() {
    let daemon = Daemon::start();
    let output = daemon.command(&["share", "web"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sharing isn't set up"), "{stderr}");
}
