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
            project: None,
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
async fn dependencies_reach_other_projects_main_stack() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app("auth", WEB);
    setup.project("[apps.acme-web]\ndepends_on = [\"auth\"]\n");
    std::fs::write(
        setup.paths.project_file("auth"),
        format!(
            "repos = \"{}/work/auth\"\nenv_provider = \"none\"\n",
            setup.root.display()
        ),
    )
    .unwrap();
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let auth = |s: &carte_api::Snapshot| {
        s.projects.iter().find(|p| p.name == "auth").unwrap().stacks[0].instances[0].clone()
    };
    wait_for(&handle, "auth started", |s| {
        auth(s).state == InstanceState::Ready
    })
    .await;
    assert_eq!(
        auth(&handle.snapshot()).holds,
        vec![carte_api::Hold::Dependent {
            app: "acme-web".into()
        }]
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
async fn a_named_project_is_addressed_apart_from_its_namesake_app() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app("acme-acme", WEB);
    setup.project("");
    let handle = setup.start();

    for app in ["web", "acme"] {
        handle.control(control(app, Action::Up)).await.unwrap();
    }
    wait_for(&handle, "both ready", |s| {
        ["acme-web", "acme-acme"]
            .iter()
            .all(|app| status(s, app, "main").state == InstanceState::Ready)
    })
    .await;

    let response = handle.control(control("acme", Action::Down)).await.unwrap();
    assert_eq!(response.affected, vec!["acme-acme@main"]);

    let mut whole = control("", Action::Down);
    whole.target.project = Some("acme".into());
    let response = handle.control(whole).await.unwrap();
    assert!(
        response.affected.contains(&"acme-web@main".to_string()),
        "{:?}",
        response.affected
    );
    wait_for(&handle, "both stopped", |s| {
        ["acme-web", "acme-acme"]
            .iter()
            .all(|app| status(s, app, "main").state == InstanceState::Stopped)
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
        project: None,
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

#[tokio::test]
async fn following_a_project_picks_up_instances_that_start_later() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_LINES=3 exec {fixture}\n");
    setup.app("acme-api", WEB);
    setup.project("");
    let handle = setup.start();

    let mut follow = handle.follow_logs();
    let target = Address {
        address: "acme".into(),
        cwd: None,
        project: None,
    };
    let (lines, filter) = handle.logs(target, 100, None).await.unwrap();
    assert!(lines.is_empty(), "{lines:?}");
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let line = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = follow.recv().await.unwrap();
            if filter.matches(&line) {
                return line;
            }
        }
    })
    .await
    .expect("a line from the instance that started");
    assert_eq!(
        (line.project.as_str(), line.app.as_str()),
        ("acme", "acme-web")
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn missing_infrastructure_fails_with_one_clear_message() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("requires = [\"tcp://127.0.0.1:1\"]\n");
    let handle = setup.start();

    let failure = handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap_err();
    assert!(
        failure.detail.contains("tcp://127.0.0.1:1 isn't reachable"),
        "{failure:?}"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn running_processes_report_memory() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    wait_for(&handle, "memory sampled", |s| {
        status(s, "acme-web", "main").memory_bytes > 0 && s.memory_bytes > 0
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn saved_policies_are_written_to_the_project_file() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("# keep me\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let response = handle
        .control(control("web/worker", Action::Pin { save: true }))
        .await
        .unwrap();
    assert!(response.messages[0].starts_with("saved to"), "{response:?}");
    let text = std::fs::read_to_string(setup.paths.project_file("acme")).unwrap();
    assert!(text.contains("# keep me"), "{text}");
    assert!(
        text.contains("[apps.acme-web.processes.worker]\npolicy = \"pinned\""),
        "{text}"
    );
    let snapshot = handle.snapshot();
    let worker = status(&snapshot, "acme-web", "main")
        .processes
        .iter()
        .find(|p| p.name == "worker")
        .unwrap();
    assert_eq!(worker.policy, carte_api::Policy::Pinned);
    assert!(!worker.temporary);
    handle.shutdown().await;
}

#[tokio::test]
async fn checkout_watchers_run_once_and_move_to_another_instance() {
    let setup = Setup::new();
    let web = setup.app("acme-web", WEB);
    setup.app(
        "acme-api",
        "web: FIXTURE_NAME=api exec {fixture}\ncss: FIXTURE_NAME=css exec {fixture}\n",
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
    setup.project("");
    let handle = setup.start();
    let css_running = |s: &carte_api::Snapshot, stack: &str| {
        status(s, "acme-api", stack)
            .processes
            .iter()
            .any(|p| p.name == "css" && p.state == ProcessState::Running)
    };

    handle
        .ensure_ready(key("acme-api", Stack::Main))
        .await
        .unwrap();
    handle
        .ensure_ready(key("acme-api", Stack::Branch("feature-x".into())))
        .await
        .unwrap();
    wait_for(&handle, "css in main", |s| css_running(s, "main")).await;
    assert!(!css_running(&handle.snapshot(), "feature-x"));

    handle
        .control(control("api@main", Action::Down))
        .await
        .unwrap();
    wait_for(&handle, "css moved to feature-x", |s| {
        css_running(s, "feature-x") && !css_running(s, "main")
    })
    .await;
    handle.shutdown().await;
}

fn share_config(setup: &Setup) {
    std::fs::write(
        setup.paths.machine_config(),
        "[share]\nprovider = \"none\"\ndomain = \"share.example\"\n",
    )
    .unwrap();
}

fn share_request(address: &str, pin: bool) -> carte_api::ShareRequest {
    carte_api::ShareRequest {
        target: Address {
            address: address.into(),
            cwd: None,
            project: None,
        },
        apps: Vec::new(),
        ttl_secs: None,
        auth: false,
        pin,
        replace: false,
    }
}

#[tokio::test]
async fn sharing_a_running_app_restarts_it_with_its_public_url() {
    let setup = Setup::new();
    setup.app(
        "acme-web",
        "web: FIXTURE_ENV=CARTE_PUBLIC_URL exec {fixture}\n",
    );
    setup.project("");
    share_config(&setup);
    let handle = setup.start();
    let web = key("acme-web", Stack::Main);

    let before = handle.ensure_ready(web.clone()).await.unwrap();
    assert!(!http_get(before.port, "/").contains("share.example"));

    handle.share(share_request("web", false)).await.unwrap();
    assert!(handle.shared_routes().contains_key(&web));
    wait_for(&handle, "a restart", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .any(|p| p.name == "web" && p.restarts > 0 && p.state == ProcessState::Running)
    })
    .await;
    let after = handle.ensure_ready(web.clone()).await.unwrap();
    let body = http_get(after.port, "/");
    assert!(body.contains("https://web--acme.share.example"), "{body}");

    handle
        .unshare(Address {
            address: "web".into(),
            cwd: None,
            project: None,
        })
        .await
        .unwrap();
    assert!(handle.shared_routes().is_empty());
    handle.shutdown().await;
}

#[tokio::test]
async fn a_single_host_provider_shares_one_app_at_a_time() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app("acme-api", WEB);
    setup.project("");
    std::fs::write(
        setup.paths.machine_config(),
        "[share]\nprovider = \"ngrok\"\ndomain = \"acme-dev.ngrok-free.app\"\n",
    )
    .unwrap();
    let handle = setup.start();

    let shared = handle.share(share_request("web", false)).await.unwrap();
    assert_eq!(shared.share.apps[0].url, "https://acme-dev.ngrok-free.app");
    let refused = handle.share(share_request("api", false)).await.unwrap_err();
    assert!(
        refused.contains("ngrok shares one app at a time, and acme-web@main is shared"),
        "{refused}"
    );
    assert!(handle.share(share_request("web", false)).await.is_ok());

    let replaced = handle
        .share(carte_api::ShareRequest {
            replace: true,
            ..share_request("api", false)
        })
        .await
        .unwrap();
    assert_eq!(replaced.messages, ["stopped sharing acme-web@main"]);
    assert_eq!(
        handle.shared_routes().keys().cloned().collect::<Vec<_>>(),
        [key("acme-api", Stack::Main)]
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn pinned_shares_start_and_hold_their_apps() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("idle_timeout = \"1s\"\n");
    share_config(&setup);
    let handle = setup.start();

    handle.share(share_request("web", true)).await.unwrap();
    wait_for(&handle, "the pinned app running", |s| {
        status(s, "acme-web", "main").state == InstanceState::Ready
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Ready,
        "a pinned share doesn't idle out"
    );

    handle
        .unshare(Address {
            address: "web".into(),
            cwd: None,
            project: None,
        })
        .await
        .unwrap();
    wait_for(&handle, "idle stop after unsharing", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    handle.shutdown().await;
}
