mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
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

/// A free port below the ephemeral range, where the daemons under test pick their
/// apps' ports, so a test's listener and another daemon's app can't get the same one.
fn free_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let start = 20_000 + (std::process::id() % 400) as u16 * 25;
    loop {
        let port = 20_000 + (start - 20_000 + NEXT.fetch_add(1, Ordering::Relaxed)) % 10_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            && std::net::TcpListener::bind(("::1", port)).is_ok()
        {
            return port;
        }
    }
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
            "",
            |_| {},
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
        Self::start_configured(linked_config, "", |_| {}, "")
    }

    /// With `extra_project` added to the acme project file.
    fn start_with_project(extra_project: &str) -> Self {
        Self::start_configured(false, extra_project, |_| {}, "")
    }

    /// With `extra_project`, after `prepare` has set up more files under `work`.
    fn start_prepared(extra_project: &str, prepare: impl FnOnce(&std::path::Path)) -> Self {
        Self::start_configured(false, extra_project, prepare, "")
    }

    fn start_configured(
        linked_config: bool,
        extra_project: &str,
        prepare: impl FnOnce(&std::path::Path),
        extra_config: &str,
    ) -> Self {
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
        prepare(&root.join("work"));
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
                "tld = \"test\"\nhttp_port = {http_port}\nhttps_port = {https_port}\ndns_port = {dns_port}\nnotifications = false\n{extra_config}"
            ),
        )
        .unwrap();
        std::fs::write(
            config.join("projects/acme.toml"),
            format!(
                "root = \"{}/work\"\ninclude = \"acme-*\"\nstrip_prefix = \"acme-\"\nenv_provider = \"none\"\n{extra_project}",
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
            if let Some(status) = daemon.child.as_mut().unwrap().try_wait().unwrap() {
                let mut stderr = String::new();
                daemon
                    .child
                    .as_mut()
                    .unwrap()
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("daemon exited with {status}: {stderr}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
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
            .env("CARTE_STATE_HOME", self.root.join("state"))
            .env("CARTE_RUNTIME_HOME", self.root.join("tmp"))
            .env("NO_COLOR", "1")
            .env("CARTE_SKILL_REFRESH", "0")
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
        self.request_on("127.0.0.1", host, path)
    }

    fn request_on(&self, ip: &str, host: &str, path: &str) -> String {
        let mut stream = TcpStream::connect((ip, self.http_port)).unwrap();
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

    let marker = stdout(&daemon.run(&["logs", "web", "--mark"]));
    assert_eq!(marker.trim_end().chars().count(), 60, "{marker}");
    let logs = stdout(&daemon.run(&["logs", "web", "--since-start"]));
    assert!(logs.ends_with(&marker), "{logs}");
    let ranked = stdout(&daemon.run(&["logs", "web", "--rank", "worker line", "--top", "1"]));
    assert_eq!(ranked.lines().count(), 1, "{ranked}");
    assert!(ranked.contains(" acme-web@main/worker line "), "{ranked}");

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
fn serves_files_ahead_of_the_app_and_without_one() {
    let daemon = Daemon::start_prepared(
        "\n[apps.acme-web]\nserve_files = \"public\"\n\n[apps.docs]\npath = \"docs\"\nserve_files = \".\"\n",
        |work| {
            std::fs::create_dir_all(work.join("acme-web/public")).unwrap();
            std::fs::write(work.join("acme-web/public/robots.txt"), "User-agent: *\n").unwrap();
            std::fs::create_dir_all(work.join("docs/guide")).unwrap();
            std::fs::write(work.join("docs/index.html"), "<h1>Docs</h1>").unwrap();
            std::fs::write(work.join("docs/guide.html"), "<h1>Guide</h1>").unwrap();
        },
    );

    let robots = daemon.request("web.acme.test", "/robots.txt");
    assert!(robots.starts_with("HTTP/1.1 200"), "{robots}");
    assert!(robots.contains("User-agent: *"), "{robots}");
    assert!(robots.contains("content-type: text/plain"), "{robots}");
    let snapshot = snapshot(&daemon);
    assert!(
        snapshot.projects[0].stacks[0]
            .instances
            .iter()
            .all(|i| i.processes.iter().all(|p| p.pid.is_none())),
        "a file doesn't start the app"
    );

    let page = daemon.request("web.acme.test", "/");
    assert!(page.contains("\"name\":\"web\""), "{page}");

    let docs = daemon.request("docs.acme.test", "/");
    assert!(docs.starts_with("HTTP/1.1 200"), "{docs}");
    assert!(docs.contains("<h1>Docs</h1>"), "{docs}");
    let guide = daemon.request("docs.acme.test", "/guide");
    assert!(guide.contains("<h1>Guide</h1>"), "{guide}");
    let missing = daemon.request("docs.acme.test", "/nope");
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
}

#[test]
fn answers_on_ipv6_loopback() {
    let daemon = Daemon::start();
    let response = daemon.request_on("::1", "web.acme.test", "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
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
fn status_reports_an_app_before_and_after_it_starts() {
    let daemon = Daemon::start();
    let status = |args: &[&str]| -> carte_api::StatusResponse {
        let mut all = vec!["status", "--json"];
        all.extend_from_slice(args);
        serde_json::from_slice(&daemon.run(&all).stdout).unwrap()
    };
    let before = status(&["web"]);
    assert_eq!(before.instances.len(), 1);
    assert_eq!(before.instances[0].state, carte_api::InstanceState::Stopped);
    assert!(!before.instances[0].processes.is_empty());
    assert!(status(&["--project", "acme"]).instances.is_empty());

    let up = stdout(&daemon.run(&["up", "web", "--no-wait"]));
    assert_eq!(up, "starting acme-web@main\n");
    eventually("web ready", || {
        status(&["web"]).instances[0].state == carte_api::InstanceState::Ready
    });
    let text = stdout(&daemon.run(&["status", "web"]));
    assert!(text.starts_with("acme-web  ready\n"), "{text}");
    assert!(text.contains("processes"), "{text}");
}

#[test]
fn up_and_down_by_address() {
    let daemon = Daemon::start();
    let up = stdout(&daemon.run(&["up", "web"]));
    assert_eq!(up, "ready acme-web@main\n");
    let ls = stdout(&daemon.run(&["ls"]));
    assert!(ls.contains("ready"), "{ls}");

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
fn addresses_that_match_nothing_exit_with_not_found() {
    let daemon = Daemon::start();
    for args in [
        &["up", "nosuchapp"][..],
        &["logs", "nosuchapp"],
        &["down", "@nosuchstack"],
    ] {
        let output = daemon.command(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(4),
            "carte {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn lists_processes_before_the_first_start_and_follows_the_procfile() {
    let daemon = Daemon::start();
    let processes = || -> Vec<(String, carte_api::ProcessState)> {
        snapshot(&daemon).projects[0].stacks[0].instances[0]
            .processes
            .iter()
            .map(|p| (p.name.clone(), p.state))
            .collect()
    };
    let stopped = |name: &str| (name.to_string(), carte_api::ProcessState::Stopped);
    assert_eq!(processes(), [stopped("web"), stopped("worker")]);

    std::fs::write(
        daemon.root.join("work/acme-web/Procfile.dev"),
        format!("web: exec {}\nclock: exec {0}\n", fixture().display()),
    )
    .unwrap();
    eventually("the new process list", || {
        processes() == [stopped("web"), stopped("clock")]
    });
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

fn touch_later(path: &std::path::Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();
}

#[test]
fn restart_txt_in_a_tmp_directory_created_later_restarts_the_instance() {
    let daemon = Daemon::start_prepared("", |work| {
        std::fs::remove_dir(work.join("acme-web/tmp")).unwrap();
    });
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
    let before = web_pid(&daemon).unwrap();
    let tmp = daemon.root.join("work/acme-web/tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("restart.txt"), "").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        web_pid(&daemon),
        Some(before),
        "creating restart.txt restarted the instance"
    );

    touch_later(&tmp.join("restart.txt"));
    eventually("a new web process", || {
        web_pid(&daemon).is_some_and(|pid| pid != before)
    });
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
    let restart_txt = daemon.root.join("work/acme-web/tmp/restart.txt");
    std::fs::write(&restart_txt, "").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        web_pid(&daemon),
        Some(before),
        "creating restart.txt restarted the instance"
    );

    touch_later(&restart_txt);
    eventually("a new web process", || {
        web_pid(&daemon).is_some_and(|pid| pid != before)
    });

    let events = stdout(&daemon.run(&["events", "web"]));
    let lines: Vec<&str> = events.lines().collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.ends_with("  acme-web  restarted: tmp/restart.txt"))
            .count(),
        1,
        "{events}"
    );
    assert!(
        lines[0].ends_with("  acme-web  started: on request"),
        "{events}"
    );
    eventually("ready again after the restart", || {
        let json = stdout(&daemon.run(&["events", "--json", "-n", "1"]));
        let last: carte_api::Event = serde_json::from_str(json.trim()).unwrap();
        last.kind == carte_api::EventKind::Ready
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
            "../feature-x/acme-web",
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
            "root = \"{}/other/shop\"\nenv_provider = \"none\"\n",
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
            .and_then(|v| v.value.clone())
    };
    assert_eq!(value("PORT"), Some(port.to_string()));
    let dirs = carte::paths::Paths::under(&daemon.root)
        .dir_roots()
        .instance(&carte_core::model::InstanceKey {
            project: "acme".into(),
            app: "acme-web".into(),
            stack: carte_core::model::Stack::Main,
        });
    assert_eq!(
        value("CARTE_STATE_DIR"),
        Some(dirs.state.display().to_string())
    );
    assert_eq!(value("CARTE_RUN_DIR"), Some(dirs.run.display().to_string()));
    let elsewhere = daemon
        .command(&["env", "web", "CARTE_RUN_DIR", "--json"])
        .env("CARTE_RUNTIME_HOME", daemon.root.join("elsewhere"))
        .output()
        .unwrap();
    let elsewhere: carte_api::EnvResponse = serde_json::from_str(&stdout(&elsewhere)).unwrap();
    assert_eq!(
        elsewhere.vars[0].value,
        Some(dirs.run.display().to_string()),
        "carte env uses the daemon's run root"
    );
    for own in ["CARTE_STATE_HOME", "CARTE_RUNTIME_HOME", "CARTE_CONFIG_DIR"] {
        assert_eq!(value(own), None, "{own} leaked into the app env");
    }
    assert!(dirs.state.is_dir());
    assert!(dirs.run.is_dir());
    let record = carte::instance_dirs::read_stack_record(dirs.state.parent().unwrap()).unwrap();
    assert_eq!(record, Some(Default::default()));
}

#[test]
fn exec_gets_the_ports_of_running_services() {
    let daemon = Daemon::start();
    let fixture = fixture().display().to_string();
    repo(
        &daemon.root.join("other/shop"),
        &format!("web: exec {fixture}\n"),
    );
    std::fs::write(
        daemon.root.join("config/projects/shop.toml"),
        format!(
            "root = \"{}/other/shop\"\nenv_provider = \"none\"\n\n\
             [processes.cache]\ncommand = \"FIXTURE_NAME=cache exec {fixture}\"\nkind = \"service\"\n\
             [processes.cache.provides]\nCACHE_URL = \"http://127.0.0.1:{{{{port}}}}\"\n",
            daemon.root.display()
        ),
    )
    .unwrap();
    eventually("the shop project", || {
        snapshot(&daemon).projects.iter().any(|p| p.name == "shop")
    });
    let exec = |args: &[&str]| {
        daemon
            .command(
                &[
                    &["exec", "shop"],
                    args,
                    &["--", "sh", "-c", "echo $CACHE_URL"],
                ]
                .concat(),
            )
            .output()
            .unwrap()
    };
    let refused = exec(&[]);
    assert_eq!(refused.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("carte up shop@main`"), "{stderr}");

    daemon.run(&["up", "shop"]);
    let port = snapshot(&daemon)
        .projects
        .iter()
        .find(|p| p.name == "shop")
        .unwrap()
        .stacks[0]
        .instances[0]
        .processes
        .iter()
        .find(|p| p.name == "cache")
        .and_then(|p| p.port)
        .unwrap();
    let out = exec(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), format!("http://127.0.0.1:{port}\n"));

    daemon.run(&["down", "shop/cache"]);
    eventually("the cache to stop", || exec(&[]).status.code() == Some(5));
    let stderr = String::from_utf8_lossy(&exec(&[]).stderr).into_owned();
    assert!(stderr.contains("carte up shop@main/cache`"), "{stderr}");
    assert!(exec(&["--no-check"]).status.success());
}

#[test]
fn exec_keeps_the_app_from_idling_out_until_the_command_ends() {
    let daemon = Daemon::start_with_project("idle_timeout = \"2s\"\n");
    let web = || snapshot(&daemon).projects[0].stacks[0].instances[0].clone();
    daemon.run(&["exec", "--no-check", "web", "--", "true"]);
    assert_eq!(web().state, carte_api::InstanceState::Stopped);

    daemon.run(&["up", "web"]);
    let mut exec = daemon
        .command(&["exec", "web", "--", "sleep", "6"])
        .spawn()
        .unwrap();
    eventually("the exec hold", || {
        web().holds == [carte_api::Hold::Exec { count: 1 }]
    });
    let status = stdout(&daemon.run(&["status", "web"]));
    assert!(status.contains("held by    1 exec\n"), "{status}");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(web().state, carte_api::InstanceState::Ready);

    assert!(exec.wait().unwrap().success());
    eventually("web to idle out", || {
        web().state == carte_api::InstanceState::Stopped
    });
    assert!(web().holds.is_empty());
}

#[test]
fn the_daemon_renders_a_process_env() {
    let daemon = Daemon::start();
    let socket = carte::paths::Paths::under(&daemon.root).socket();
    let request = |address: &str| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(
            carte::client::Client::new(&socket).post::<_, carte_api::EnvResponse>(
                carte_api::paths::PROCESS_ENV,
                &carte_api::Address {
                    address: address.into(),
                    cwd: None,
                    project: Some("acme".into()),
                },
            ),
        )
    };
    let env = request("web@main/worker").unwrap();
    assert_eq!(env.instance, "acme-web@main/worker");
    let var = |key: &str| env.vars.iter().find(|v| v.key == key);
    assert_eq!(
        var("CARTE_APP").and_then(|v| v.value.as_deref()),
        Some("acme-web")
    );
    assert_eq!(var("CARTE_APP").map(|v| v.source.as_str()), Some("carte"));
    assert_eq!(
        var("PORT").and_then(|v| v.value.as_deref()),
        Some("<assigned at start>")
    );

    let env = request("web@main").unwrap();
    assert_eq!(
        env.instance, "acme-web@main/web",
        "an app means its HTTP process"
    );
    let err = request("@main").unwrap_err();
    assert!(format!("{err:#}").contains("select an app"), "{err:#}");
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
        ls.contains("public https://web--acme.share.example"),
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

/// Runs `carte mcp` in the app's checkout and sends it one request per line.
fn mcp_session(
    daemon: &Daemon,
    extra: &[&str],
    requests: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut args = vec!["mcp"];
    args.extend_from_slice(extra);
    let mut child = daemon
        .command(&args)
        .current_dir(daemon.root.join("work/acme-web"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn mcp_answers_from_the_checkout_it_runs_in() {
    use serde_json::json;
    let daemon = Daemon::start();
    let call = |id: u32, name: &str, arguments: serde_json::Value| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    };
    let replies = mcp_session(
        &daemon,
        &[],
        &[
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                              "clientInfo": {"name": "test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            call(2, "status", json!({})),
            call(3, "start", json!({})),
        ],
    );
    assert_eq!(replies.len(), 4, "{replies:?}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "carte");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let tools: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"status") && !tools.contains(&"start"),
        "{tools:?}"
    );
    assert_eq!(replies[2]["result"]["isError"], false);
    let status: carte_api::StatusResponse =
        serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status.instances.len(), 1);
    assert_eq!(status.instances[0].app, "acme-web");
    assert_eq!(replies[3]["error"]["code"], -32602);

    let replies = mcp_session(&daemon, &["--control"], &[call(4, "start", json!({}))]);
    assert_eq!(replies[0]["result"]["isError"], false, "{replies:?}");
    let ready: carte_api::StatusResponse =
        serde_json::from_slice(&daemon.run(&["status", "web", "--json"]).stdout).unwrap();
    assert_eq!(ready.instances[0].state, carte_api::InstanceState::Ready);
}

#[test]
fn prune_removes_leftovers_of_what_no_longer_exists() {
    use carte_core::model::{InstanceKey, Stack};
    let daemon = Daemon::start();
    let paths = carte::paths::Paths::under(&daemon.root);
    let roots = paths.dir_roots();
    let key = |project: &str, app: &str, stack: Stack| InstanceKey {
        project: project.into(),
        app: app.into(),
        stack,
    };
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
    let main = roots.instance(&key("acme", "acme-web", Stack::Main));
    assert!(main.state.is_dir());

    let feature = roots.instance(&key("acme", "acme-web", Stack::Branch("feature-x".into())));
    let old_app = roots.instance(&key("acme", "acme-old", Stack::Main));
    let gone = roots.instance(&key("gone", "gone-web", Stack::Main));
    let broken = roots.instance(&key("broken", "broken-web", Stack::Branch("x".into())));
    for dirs in [&feature, &old_app, &gone, &broken] {
        std::fs::create_dir_all(&dirs.state).unwrap();
        std::fs::create_dir_all(&dirs.run).unwrap();
    }
    std::fs::write(feature.state.join("dump.rdb"), vec![0; 1000]).unwrap();
    let pidfile = paths.pidfile(&key("acme", "acme-old", Stack::Main));
    std::fs::write(&pidfile, "1").unwrap();
    let unknown = roots.run.join("zzzzzzzz");
    std::fs::create_dir_all(&unknown).unwrap();
    std::fs::write(
        daemon.root.join("config/projects/broken.toml"),
        format!(
            "root = \"{}\"\ninclude = \"nothing-*\"\n",
            daemon.root.display()
        ),
    )
    .unwrap();

    let json = stdout(&daemon.run(&["prune", "--dry-run", "--json"]));
    let output: carte_api::PruneOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(output.result, None);
    let listed: Vec<PathBuf> = output
        .list
        .candidates
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(
        listed,
        [
            roots.stack_dir("acme", "feature-x"),
            feature.run.clone(),
            old_app.state.clone(),
            old_app.run.clone(),
            pidfile.clone(),
            roots.data.join("gone"),
            gone.run.clone(),
        ]
    );
    assert!(output.list.total_bytes >= 1000);
    let stacks: Vec<String> = output
        .list
        .stacks
        .iter()
        .map(|s| format!("{}@{}", s.project, s.stack))
        .collect();
    assert_eq!(stacks, ["acme@feature-x", "gone@main"]);
    assert_eq!(output.list.skipped.len(), 1, "{:?}", output.list.skipped);
    assert_eq!(output.list.skipped[0].project.as_deref(), Some("broken"));

    let refused = daemon
        .command(&["prune"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(stdout(&refused).contains("acme @feature-x"));
    assert!(feature.state.is_dir());

    let json = stdout(&daemon.run(&["prune", "--yes", "--json"]));
    let output: carte_api::PruneOutput = serde_json::from_str(&json).unwrap();
    let result = output.result.unwrap();
    assert_eq!(result.removed.len(), listed.len(), "{result:?}");
    assert!(
        result.kept.is_empty() && result.errors.is_empty(),
        "{result:?}"
    );
    for path in &listed {
        assert!(!path.exists(), "{} survived", path.display());
    }
    for kept in [&main.state, &main.run, &broken.state, &broken.run, &unknown] {
        assert!(kept.is_dir(), "{} was removed", kept.display());
    }

    let again = stdout(&daemon.run(&["prune", "--dry-run"]));
    assert!(again.starts_with("Nothing to prune.\n"), "{again}");
    assert!(again.contains("Skipped broken: "), "{again}");
}

/// The acme project with a teardown process for acme-web that runs `command`.
fn start_with_teardown(delay: &str, command: &str) -> Daemon {
    Daemon::start_with_project(&format!(
        "teardown_delay = \"{delay}\"\n\n[apps.acme-web.processes.drop]\nkind = \"teardown\"\ncommand = '{command}'\n\n[apps.acme-web.processes.drop.env]\nDB = \"acme_{{{{stack | snake}}}}\"\n"
    ))
}

fn events(daemon: &Daemon) -> Vec<carte_api::Event> {
    stdout(&daemon.run(&["events", "--json", "-n", "200"]))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The latest event of `kind` about the feature-x stack itself, or about a process.
fn teardown_event(
    daemon: &Daemon,
    kind: carte_api::EventKind,
    process: bool,
) -> Option<carte_api::Event> {
    events(daemon).into_iter().rev().find(|e| {
        e.kind == kind
            && e.subjects.first().is_some_and(|s| {
                s.stack.as_deref() == Some("feature-x") && s.process.is_some() == process
            })
    })
}

/// Adds the feature-x worktree and starts acme-web in it.
fn run_in_feature_x(daemon: &Daemon) -> carte_core::dirs::InstanceDirs {
    git(
        &daemon.root.join("work/acme-web"),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature-x",
            "../feature-x/acme-web",
        ],
    );
    eventually("the feature-x stack", || {
        snapshot(daemon).projects[0]
            .stacks
            .iter()
            .any(|s| s.name == "feature-x")
    });
    let response = daemon.request("web.feature-x.acme.test", "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let dirs = carte::paths::Paths::under(&daemon.root)
        .dir_roots()
        .instance(&carte_core::model::InstanceKey {
            project: "acme".into(),
            app: "acme-web".into(),
            stack: carte_core::model::Stack::Branch("feature-x".into()),
        });
    assert!(dirs.state.is_dir() && dirs.run.is_dir());
    dirs
}

fn remove_feature_x(daemon: &Daemon) {
    git(
        &daemon.root.join("work/acme-web"),
        &["worktree", "remove", "--force", "../feature-x/acme-web"],
    );
}

#[test]
fn teardown_runs_once_the_worktree_is_gone_and_removes_the_folders() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("teardown");
    let daemon = start_with_teardown(
        "1s",
        &format!(
            "printf \"%s|%s|%s|%s|%s\\n\" \"$CARTE_STACK\" \"$CARTE_STATE_DIR\" \"$DB\" \"$CARTE_BRANCH\" \"$(pwd)\" >> {}",
            marker.display()
        ),
    );
    assert!(
        daemon
            .request("web.acme.test", "/")
            .starts_with("HTTP/1.1 200")
    );
    let dirs = run_in_feature_x(&daemon);
    let stack_dir = dirs.state.parent().unwrap().to_path_buf();
    assert!(!marker.exists(), "teardown ran with the instance");

    remove_feature_x(&daemon);
    eventually("the teardown", || {
        teardown_event(&daemon, EventKind::TeardownFinished, false).is_some()
    });
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!(
            "feature-x|{}|acme_feature_x|feature-x|{}\n",
            dirs.state.display(),
            daemon.root.join("work/acme-web").display()
        )
    );
    let ran = teardown_event(&daemon, EventKind::TeardownFinished, true).unwrap();
    assert_eq!(ran.subjects[0].process.as_deref(), Some("drop"));
    assert_eq!(ran.exit_code, Some(0));
    assert!(teardown_event(&daemon, EventKind::TeardownScheduled, false).is_some());
    assert!(!stack_dir.exists(), "{} survived", stack_dir.display());
    assert!(!dirs.run.exists(), "{} survived", dirs.run.display());
    let main = carte::paths::Paths::under(&daemon.root)
        .dir_roots()
        .stack_dir("acme", "main");
    assert!(main.is_dir());
}

#[test]
fn a_failed_teardown_keeps_the_folders() {
    use carte_api::EventKind;
    let daemon = start_with_teardown("1s", "echo boom; exit 3");
    let dirs = run_in_feature_x(&daemon);

    remove_feature_x(&daemon);
    eventually("the failed teardown", || {
        teardown_event(&daemon, EventKind::TeardownFailed, false).is_some()
    });
    let failed = teardown_event(&daemon, EventKind::TeardownFailed, true).unwrap();
    assert_eq!(failed.exit_code, Some(3));
    assert_eq!(failed.message, "exited with code 3: boom");
    assert!(dirs.state.is_dir());
    assert!(teardown_event(&daemon, EventKind::TeardownFinished, false).is_none());

    let listed = stdout(&daemon.run(&["prune", "--dry-run"]));
    assert!(listed.contains("acme@feature-x"), "{listed}");
}

#[test]
fn a_worktree_that_comes_back_cancels_the_teardown() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("teardown");
    let daemon = start_with_teardown("3s", &format!("touch {}", marker.display()));
    let dirs = run_in_feature_x(&daemon);

    remove_feature_x(&daemon);
    eventually("the scheduled teardown", || {
        teardown_event(&daemon, EventKind::TeardownScheduled, false).is_some()
    });
    git(
        &daemon.root.join("work/acme-web"),
        &[
            "worktree",
            "add",
            "-q",
            "../feature-x/acme-web",
            "feature-x",
        ],
    );
    eventually("the cancelled teardown", || {
        teardown_event(&daemon, EventKind::TeardownCancelled, false).is_some()
    });
    std::thread::sleep(Duration::from_secs(5));
    assert!(!marker.exists(), "the teardown ran");
    assert!(dirs.state.is_dir());
}

#[test]
fn a_worktree_that_comes_back_during_the_teardown_keeps_the_folders() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let started = marker.path().join("started");
    let daemon = start_with_teardown("1s", &format!("touch {}; sleep 3", started.display()));
    let dirs = run_in_feature_x(&daemon);

    remove_feature_x(&daemon);
    eventually("the teardown process", || started.exists());
    git(
        &daemon.root.join("work/acme-web"),
        &[
            "worktree",
            "add",
            "-q",
            "../feature-x/acme-web",
            "feature-x",
        ],
    );
    eventually("the cancelled teardown", || {
        teardown_event(&daemon, EventKind::TeardownCancelled, false).is_some()
    });
    let cancelled = teardown_event(&daemon, EventKind::TeardownCancelled, false).unwrap();
    assert_eq!(
        cancelled.message,
        "it has a worktree again; kept its folders"
    );
    assert!(teardown_event(&daemon, EventKind::TeardownFinished, false).is_none());
    assert!(dirs.state.is_dir());
    assert!(dirs.run.is_dir());
}

#[test]
fn an_unreadable_stack_record_pauses_the_teardown() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("teardown");
    let daemon = start_with_teardown("2s", &format!("touch {}", marker.display()));
    let dirs = run_in_feature_x(&daemon);
    let stack_dir = dirs.state.parent().unwrap().to_path_buf();

    remove_feature_x(&daemon);
    eventually("the scheduled teardown", || {
        teardown_event(&daemon, EventKind::TeardownScheduled, false).is_some()
    });
    std::fs::write(stack_dir.join("stack.toml"), "not = [toml").unwrap();
    eventually("the paused teardown", || {
        teardown_event(&daemon, EventKind::TeardownPaused, false).is_some()
    });
    let paused = teardown_event(&daemon, EventKind::TeardownPaused, false).unwrap();
    assert!(
        paused.message.contains("couldn't read its stack.toml"),
        "{}",
        paused.message
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(teardown_event(&daemon, EventKind::TeardownCancelled, false).is_none());
    let paused_events = events(&daemon)
        .into_iter()
        .filter(|e| e.kind == EventKind::TeardownPaused)
        .count();
    assert_eq!(paused_events, 1);
    assert!(!marker.exists(), "the teardown ran");
    assert!(dirs.state.is_dir());
}

#[test]
fn a_problem_with_the_project_pauses_the_teardown_and_names_it() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("teardown");
    let daemon = start_with_teardown("2s", &format!("touch {}", marker.display()));
    let dirs = run_in_feature_x(&daemon);

    remove_feature_x(&daemon);
    eventually("the scheduled teardown", || {
        teardown_event(&daemon, EventKind::TeardownScheduled, false).is_some()
    });
    let project_file = daemon.root.join("config/projects/acme.toml");
    let text = std::fs::read_to_string(&project_file).unwrap();
    let missing = daemon.root.join("work/missing");
    let text = text.replacen(
        "include = \"",
        &format!("include = [\"{}\", \"", missing.display()),
        1,
    );
    let text = text.replacen("acme-*\"\n", "acme-*\"]\n", 1);
    std::fs::write(&project_file, text).unwrap();
    eventually("the paused teardown", || {
        teardown_event(&daemon, EventKind::TeardownPaused, false).is_some()
    });
    let paused = teardown_event(&daemon, EventKind::TeardownPaused, false).unwrap();
    assert!(
        paused
            .message
            .contains("project acme has a problem: include entry")
            && paused.message.contains("work/missing"),
        "{}",
        paused.message
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(!marker.exists(), "the teardown ran");
    assert!(dirs.state.is_dir());
}

#[test]
fn a_worktree_named_main_doesnt_pause_other_teardowns() {
    use carte_api::EventKind;
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("teardown");
    let daemon = start_with_teardown("1s", &format!("touch {}", marker.display()));
    let dirs = run_in_feature_x(&daemon);
    git(
        &daemon.root.join("work/acme-web"),
        &["worktree", "add", "-q", "-b", "other", "../main/acme-web"],
    );
    eventually("the problem with the main worktree", || {
        snapshot(&daemon)
            .problems
            .iter()
            .any(|p| p.message.contains("would be named main"))
    });

    remove_feature_x(&daemon);
    eventually("the teardown", || {
        teardown_event(&daemon, EventKind::TeardownFinished, false).is_some()
    });
    assert!(marker.exists());
    assert!(teardown_event(&daemon, EventKind::TeardownPaused, false).is_none());
    assert!(!dirs.state.exists());
}

#[test]
fn switching_or_detaching_inside_a_worktree_keeps_its_stack() {
    let daemon = start_with_teardown("1s", "true");
    run_in_feature_x(&daemon);
    let worktree = daemon.root.join("work/feature-x/acme-web");
    let branch = |daemon: &Daemon| {
        snapshot(daemon).projects[0]
            .stacks
            .iter()
            .find(|s| s.name == "feature-x")
            .map(|s| s.branch.clone())
    };
    git(&worktree, &["switch", "-q", "-c", "other"]);
    eventually("the switched branch", || {
        branch(&daemon) == Some(Some("other".into()))
    });
    git(&worktree, &["switch", "-q", "--detach"]);
    eventually("the detached worktree", || branch(&daemon) == Some(None));
    std::thread::sleep(Duration::from_secs(3));
    assert!(teardown_event(&daemon, carte_api::EventKind::TeardownScheduled, false).is_none());
    assert_eq!(branch(&daemon), Some(None));
    let snapshot = snapshot(&daemon);
    let stack = snapshot.projects[0]
        .stacks
        .iter()
        .find(|s| s.name == "feature-x")
        .unwrap();
    let web = stack
        .instances
        .iter()
        .find(|i| i.app == "acme-web")
        .unwrap();
    assert_eq!(web.state, carte_api::InstanceState::Ready, "still running");
}

#[test]
fn kube_projects_answer_from_the_proxy() {
    let daemon = Daemon::start_prepared("", |work| {
        let root = work.parent().unwrap();
        let kubeconfig = root.join("kubeconfig");
        std::fs::write(
            &kubeconfig,
            "apiVersion: v1\nkind: Config\nclusters:\n- name: prod\n  cluster:\n    server: https://127.0.0.1:1\ncontexts:\n- name: prod\n  context:\n    cluster: prod\n    user: prod\nusers:\n- name: prod\n  user:\n    token: secret\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("config/projects")).unwrap();
        std::fs::write(
            root.join("config/projects/prod.toml"),
            format!(
                "kube = {{ context = \"prod\", kubeconfig = \"{}\" }}\nenv_provider = \"none\"\n",
                kubeconfig.display()
            ),
        )
        .unwrap();
    });
    let response = daemon.request("grafana.monitoring.prod.test", "/");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(
        response.contains("Couldn't reach the cluster"),
        "{response}"
    );

    let response = daemon.request("www.grafana.monitoring.prod.test", "/");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("has no subdomains"), "{response}");

    let mut stream = TcpStream::connect(("127.0.0.1", daemon.http_port)).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: grafana.monitoring.prod.test\r\nReferer: https://evil.example/page\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("evil.example"), "{response}");

    let status = stdout(&daemon.run(&["status", "grafana.prod@monitoring"]));
    assert!(status.starts_with("grafana@monitoring  failed"), "{status}");
    assert!(status.contains("last error"), "{status}");
    let ls = stdout(&daemon.run(&["ls"]));
    assert!(ls.contains("grafana.prod@monitoring"), "{ls}");
    assert!(ls.contains("cluster: prod:"), "{ls}");
    let down = stdout(&daemon.run(&["down", "--project", "prod"]));
    assert!(!down.contains("error"), "{down}");

    for args in [
        &["env", "--project", "prod", "grafana"][..],
        &["share", "grafana.prod@monitoring"][..],
        &["pin", "grafana.prod"][..],
    ] {
        let output = daemon.command(args).output().unwrap();
        assert!(!output.status.success(), "carte {args:?} should fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("kube"), "carte {args:?}: {stderr}");
    }
}
