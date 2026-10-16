mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use carte::paths::Paths;
use carte::process::Spawner;
use carte::supervisor::{self, Handle, Options};
use carte_api::{Action, Address, ControlRequest, InstanceState, ProcessState};
use carte_core::discovery;
use carte_core::model::{InstanceKey, Stack};
use common::{fixture, git, http_get, repo};
use nix::sys::signal::kill;
use nix::unistd::Pid;

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths::under(&root);
        std::fs::create_dir_all(paths.projects_dir()).unwrap();
        Setup {
            _dir: dir,
            root,
            paths,
        }
    }

    fn app(&self, name: &str, procfile: &str) -> PathBuf {
        let dir = self.root.join("work").join(name);
        repo(
            &dir,
            &procfile.replace("{fixture}", &fixture().display().to_string()),
        );
        dir
    }

    fn project(&self, toml: &str) {
        let repos = self.root.join("work").display().to_string();
        let text = format!(
            "repos = \"{repos}/acme-*\"\nstrip_prefix = \"acme-\"\nenv_provider = \"none\"\n{toml}"
        );
        std::fs::write(self.paths.project_file("acme"), text).unwrap();
    }

    fn start(&self) -> Handle {
        let world = discovery::load_world(&self.paths.config_dir);
        assert!(world.problems.is_empty(), "{:?}", world.problems);
        let options = Options {
            grace: Duration::from_millis(500),
            boot_timeout: Duration::from_secs(10),
            tick: Duration::from_millis(100),
            ..Options::new(&self.paths)
        };
        supervisor::start(
            world,
            self.paths.clone(),
            Spawner::new(env!("CARGO_BIN_EXE_carte")),
            options,
        )
    }
}

fn key(app: &str, stack: Stack) -> InstanceKey {
    InstanceKey {
        project: "acme".into(),
        app: app.into(),
        stack,
    }
}

fn status<'a>(
    snapshot: &'a carte_api::Snapshot,
    app: &str,
    stack: &str,
) -> &'a carte_api::InstanceStatus {
    snapshot.projects[0]
        .stacks
        .iter()
        .find(|s| s.name == stack)
        .unwrap()
        .instances
        .iter()
        .find(|i| i.app == app)
        .unwrap()
}

async fn wait_for(handle: &Handle, what: &str, check: impl Fn(&carte_api::Snapshot) -> bool) {
    let mut watch = handle.watch();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if check(&watch.borrow_and_update()) {
                return;
            }
            watch.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for {what}: {:#?}",
        handle.snapshot()
    );
}

fn control(address: &str, action: Action) -> ControlRequest {
    ControlRequest {
        target: Address {
            address: address.into(),
            cwd: None,
        },
        action,
    }
}

fn alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

const WEB: &str = "web: FIXTURE_NAME=web FIXTURE_ENV=CARTE_URL,CARTE_CHECKOUT,CARTE_STACK,CARTE_APP exec {fixture}\nworker: FIXTURE_NAME=worker exec {fixture}\n";

#[tokio::test]
async fn lazy_start_serves_http_with_injected_env() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();

    let upstream = handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let body = http_get(upstream.port, "/");
    assert!(body.contains("\"name\":\"web\""), "{body}");
    assert!(body.contains("https://web.acme.test"), "{body}");
    assert!(body.contains("\"CARTE_CHECKOUT\":\"main\""), "{body}");

    wait_for(&handle, "worker running", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .all(|p| p.state == ProcessState::Running)
    })
    .await;
    let snapshot = handle.snapshot();
    let web = status(&snapshot, "acme-web", "main");
    assert_eq!(web.state, InstanceState::Ready);
    let pids: Vec<u32> = web.processes.iter().filter_map(|p| p.pid).collect();
    assert_eq!(pids.len(), 2);

    handle.shutdown().await;
    for pid in pids {
        assert!(!alive(pid), "{pid} survived shutdown");
    }
}

#[tokio::test]
async fn a_crash_on_boot_fails_the_request_with_logs() {
    let setup = Setup::new();
    setup.app(
        "acme-web",
        "web: FIXTURE_LINES=2 FIXTURE_CRASH=3 exec {fixture}\n",
    );
    setup.project("");
    let handle = setup.start();

    let failure = handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap_err();
    assert_eq!(failure.title, "acme-web@main crashed");
    assert!(failure.detail.contains("exited with code 3"), "{failure:?}");
    assert!(!failure.log.is_empty(), "{failure:?}");
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Crashed
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn idle_instances_stop() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("idle_timeout = \"1s\"\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    wait_for(&handle, "idle stop", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn connections_keep_an_instance_alive() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("idle_timeout = \"1s\"\n");
    let handle = setup.start();
    let web = key("acme-web", Stack::Main);

    handle.ensure_ready(web.clone()).await.unwrap();
    handle.activity(web.clone(), supervisor::Activity::ConnectionOpened);
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Ready
    );
    handle.activity(web, supervisor::Activity::ConnectionClosed);
    wait_for(&handle, "idle stop", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn dependencies_start_with_their_dependent_in_the_same_stack() {
    let setup = Setup::new();
    let web = setup.app("acme-web", WEB);
    setup.app(
        "acme-api",
        "web: FIXTURE_NAME=api FIXTURE_ENV=CARTE_URL,CARTE_CHECKOUT exec {fixture}\n",
    );
    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature-x",
            "../acme-web-feature-x",
        ],
    );
    setup.project("[apps.acme-web]\ndepends_on = [\"api\"]\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Branch("feature-x".into())))
        .await
        .unwrap();
    let api = handle
        .ensure_ready(key("acme-api", Stack::Branch("feature-x".into())))
        .await
        .unwrap();
    let body = http_get(api.port, "/");
    assert!(body.contains("https://api.feature-x.acme.test"), "{body}");
    assert!(body.contains("\"CARTE_CHECKOUT\":\"main\""), "{body}");

    let snapshot = handle.snapshot();
    let web_x = status(&snapshot, "acme-web", "feature-x");
    assert_eq!(web_x.checkout, "feature-x");
    assert!(
        web_x
            .checkout_path
            .ends_with(Path::new("acme-web-feature-x"))
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn control_starts_and_stops_by_address() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();

    let response = handle.control(control("web", Action::Up)).await.unwrap();
    assert_eq!(response.affected, vec!["acme-web@main"]);
    wait_for(&handle, "ready", |s| {
        status(s, "acme-web", "main").state == InstanceState::Ready
    })
    .await;

    handle
        .control(control("web/worker", Action::Down))
        .await
        .unwrap();
    wait_for(&handle, "worker stopped", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .any(|p| p.name == "worker" && p.state == ProcessState::Stopped)
    })
    .await;
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Ready
    );

    handle.control(control("web", Action::Down)).await.unwrap();
    wait_for(&handle, "stopped", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn disabled_processes_stay_down_until_started_by_hand() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("[apps.acme-web.processes.worker]\npolicy = \"disabled\"\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let snapshot = handle.snapshot();
    let worker = status(&snapshot, "acme-web", "main")
        .processes
        .iter()
        .find(|p| p.name == "worker")
        .unwrap();
    assert_eq!(worker.state, ProcessState::Stopped);

    handle
        .control(control("web/worker", Action::Up))
        .await
        .unwrap();
    wait_for(&handle, "worker running", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .any(|p| p.name == "worker" && p.state == ProcessState::Running)
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn logs_are_kept_and_filtered() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_LINES=3 exec {fixture}\n");
    setup.project("");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let target = Address {
        address: "web".into(),
        cwd: None,
    };
    let (lines, filter) = handle.logs(target.clone(), 100, None).await.unwrap();
    assert!(lines.iter().any(|l| l.text.contains("ready")), "{lines:?}");
    assert!(lines.iter().all(|l| filter.matches(l)));
    let (lines, _) = handle
        .logs(target, 100, Some("line 2".into()))
        .await
        .unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    handle.shutdown().await;
}
