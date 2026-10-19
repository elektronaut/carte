mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use carte::paths::Paths;
use carte::process::Spawner;
use carte::supervisor::{self, Handle, Options};
use carte_api::{Action, Address, ControlRequest, InstanceState, LogsRequest, ProcessState};
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
        let work = self.root.join("work").display().to_string();
        let text = format!(
            "root = \"{work}\"\ninclude = \"acme-*\"\nstrip_prefix = \"acme-\"\nenv_provider = \"none\"\n{toml}"
        );
        std::fs::write(self.paths.project_file("acme"), text).unwrap();
    }

    fn start(&self) -> Handle {
        self.start_with_backoff(Duration::from_millis(100))
    }

    fn start_with_backoff(&self, restart_backoff: Duration) -> Handle {
        let world = discovery::load_world(&self.paths.config_dir);
        assert!(world.problems.is_empty(), "{:?}", world.problems);
        let options = Options {
            grace: Duration::from_millis(500),
            boot_timeout: Duration::from_secs(10),
            tick: Duration::from_millis(100),
            restart_backoff,
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
        wait: false,
    }
}

fn waiting(address: &str, action: Action) -> ControlRequest {
    ControlRequest {
        wait: true,
        ..control(address, action)
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
    assert!(body.contains("https://web.acme.localhost"), "{body}");
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
async fn a_starting_instance_reports_what_it_waits_for() {
    let setup = Setup::new();
    setup.app(
        "acme-web",
        "web: FIXTURE_BOOT_DELAY_MS=1500 exec {fixture}\nworker: exec {fixture}\n",
    );
    setup.project("[apps.acme-web.processes.web]\nready = \"/up\"\n");
    let handle = setup.start();

    handle.control(control("web", Action::Up)).await.unwrap();
    wait_for(&handle, "the web process to be waited on", |s| {
        status(s, "acme-web", "main")
            .boot
            .as_ref()
            .is_some_and(|b| b.waiting_for.starts_with("web to answer /up on port "))
    })
    .await;
    wait_for(&handle, "ready", |s| {
        status(s, "acme-web", "main").state == InstanceState::Ready
    })
    .await;
    assert_eq!(status(&handle.snapshot(), "acme-web", "main").boot, None);
    handle.shutdown().await;
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
async fn waiting_up_answers_once_ready_or_failed() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app(
        "acme-broken",
        "web: FIXTURE_LINES=2 FIXTURE_CRASH=3 exec {fixture}\n",
    );
    setup.app("acme-jobs", "worker: exec {fixture}\n");
    setup.project("");
    let handle = setup.start();

    let response = handle.control(waiting("web", Action::Up)).await.unwrap();
    assert!(response.failures.is_empty(), "{response:?}");
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Ready
    );

    let response = handle.control(waiting("jobs", Action::Up)).await.unwrap();
    assert!(response.failures.is_empty(), "{response:?}");

    let response = handle.control(waiting("broken", Action::Up)).await.unwrap();
    assert_eq!(response.affected, ["acme-broken@main"]);
    let [failure] = response.failures.as_slice() else {
        panic!("{response:?}");
    };
    assert_eq!(failure.instance, "acme-broken@main");
    assert!(failure.detail.contains("exited with code 3"), "{failure:?}");
    assert!(!failure.log.is_empty(), "{failure:?}");
    handle.shutdown().await;
}

#[tokio::test]
async fn waiting_restart_answers_with_the_new_process_ready() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();
    handle.control(waiting("web", Action::Up)).await.unwrap();
    let pid = |handle: &Handle| status(&handle.snapshot(), "acme-web", "main").processes[0].pid;
    let before = pid(&handle);
    assert!(before.is_some());

    let response = handle
        .control(waiting("web", Action::Restart))
        .await
        .unwrap();
    assert!(response.failures.is_empty(), "{response:?}");
    wait_for(&handle, "the snapshot to catch up", |s| {
        status(s, "acme-web", "main").state == InstanceState::Ready
    })
    .await;
    let after = pid(&handle);
    assert!(after.is_some() && after != before, "{before:?} {after:?}");
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
async fn exec_holds_last_through_a_restart_and_end_with_a_stop() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();
    let web = key("acme-web", Stack::Main);
    let target = |address: &str| Address {
        address: address.into(),
        cwd: None,
        project: None,
    };
    let holds = |handle: &Handle| status(&handle.snapshot(), "acme-web", "main").holds.clone();
    let execs = |count| vec![carte_api::Hold::Exec { count }];

    assert!(handle.hold(target("web")).await.is_err());
    handle.ensure_ready(web.clone()).await.unwrap();
    let (key, first) = handle.hold(target("web")).await.unwrap();
    assert_eq!(key, web);
    wait_for(&handle, "the hold", |s| {
        status(s, "acme-web", "main").holds == execs(1)
    })
    .await;

    handle
        .control(waiting("web", Action::Restart))
        .await
        .unwrap();
    assert_eq!(holds(&handle), execs(1));

    handle.control(control("web", Action::Down)).await.unwrap();
    wait_for(&handle, "the stop", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    assert!(holds(&handle).is_empty());

    handle.ensure_ready(web.clone()).await.unwrap();
    let (_, second) = handle.hold(target("web")).await.unwrap();
    handle.release(web.clone(), first);
    handle.release(web.clone(), first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(holds(&handle), execs(1));
    handle.release(web, second);
    wait_for(&handle, "the release", |s| {
        status(s, "acme-web", "main").holds.is_empty()
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
            "../feature-x/acme-web",
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
    assert!(
        body.contains("https://api.feature-x.acme.localhost"),
        "{body}"
    );
    assert!(body.contains("\"CARTE_CHECKOUT\":\"main\""), "{body}");

    let snapshot = handle.snapshot();
    let web_x = status(&snapshot, "acme-web", "feature-x");
    assert_eq!(web_x.checkout, "feature-x");
    assert!(
        web_x
            .checkout_path
            .ends_with(Path::new("feature-x/acme-web"))
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
            "root = \"{}/work/auth\"\nenv_provider = \"none\"\n",
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
async fn naming_the_main_stack_starts_every_app_on_main() {
    let setup = Setup::new();
    let web = setup.app("acme-web", WEB);
    setup.app("acme-api", "web: FIXTURE_NAME=api exec {fixture}\n");
    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature-x",
            "../feature-x/acme-web",
        ],
    );
    setup.project("");
    let handle = setup.start();

    let mut whole = control("", Action::Up);
    whole.target.project = Some("acme".into());
    let error = handle.control(whole).await.unwrap_err();
    assert!(error.contains("name an app or a stack"), "{error}");

    let mut main = control("@main", Action::Up);
    main.target.cwd = Some(web.clone());
    let response = handle.control(main).await.unwrap();
    assert_eq!(response.affected, ["acme-api@main", "acme-web@main"]);
    wait_for(&handle, "both ready", |s| {
        ["acme-web", "acme-api"]
            .iter()
            .all(|app| status(s, app, "main").state == InstanceState::Ready)
    })
    .await;
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "feature-x").state,
        InstanceState::Stopped
    );

    let mut restart = control("@main", Action::Restart);
    restart.target.project = Some("acme".into());
    let response = handle.control(restart).await.unwrap();
    assert_eq!(response.affected, ["acme-api@main", "acme-web@main"]);
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
    let (lines, filter) = handle.logs(request(&target)).await.unwrap();
    assert!(lines.iter().any(|l| l.text.contains("ready")), "{lines:?}");
    assert!(lines.iter().all(|l| filter.matches(l)));
    let (lines, _) = handle
        .logs(LogsRequest {
            search: Some("line 2".into()),
            ..request(&target)
        })
        .await
        .unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (ranked, _) = handle
        .logs(LogsRequest {
            rank: Some("2".into()),
            top: Some(5),
            ..request(&target)
        })
        .await
        .unwrap();
    assert!(ranked[0].text.contains("line 2"), "{ranked:?}");
    handle.shutdown().await;
}

fn request(target: &Address) -> LogsRequest {
    LogsRequest {
        target: target.clone(),
        ..LogsRequest::default()
    }
}

#[tokio::test]
async fn logs_since_the_latest_run_or_a_time() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_LINES=3 exec {fixture}\n");
    setup.project("");
    let handle = setup.start();
    let target = Address {
        address: "web".into(),
        cwd: None,
        project: None,
    };
    let count =
        |lines: &[carte_api::LogLine]| lines.iter().filter(|l| l.text.contains("line 2")).count();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    handle
        .control(control("web", Action::Restart))
        .await
        .unwrap();
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let mut all = Vec::new();
    for _ in 0..50 {
        (all, _) = handle.logs(request(&target)).await.unwrap();
        if count(&all) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(count(&all), 2, "{all:?}");

    let (latest, _) = handle
        .logs(LogsRequest {
            since_start: true,
            ..request(&target)
        })
        .await
        .unwrap();
    assert_eq!(count(&latest), 1, "{latest:?}");

    let (none, _) = handle
        .logs(LogsRequest {
            since_ms: Some(all.last().unwrap().at_ms + 1),
            ..request(&target)
        })
        .await
        .unwrap();
    assert!(none.is_empty(), "{none:?}");
    handle.shutdown().await;
}

#[tokio::test]
async fn markers_show_in_every_log_of_their_scope() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_LINES=3 exec {fixture}\n");
    setup.app("acme-api", WEB);
    setup.project("");
    let handle = setup.start();
    let address = |address: &str| Address {
        address: address.into(),
        cwd: None,
        project: None,
    };
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();

    let mut follow = handle.follow_logs();
    let marker = handle.mark(address("web")).await.unwrap();
    assert!(marker.marker);
    assert_eq!(
        (
            marker.app.as_str(),
            marker.stack.as_str(),
            marker.process.as_str()
        ),
        ("acme-web", "main", "")
    );
    let followed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = follow.recv().await.unwrap();
            if line.marker {
                return line;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(followed, marker);

    for scope in ["web", "web/web", "acme"] {
        let (lines, filter) = handle.logs(request(&address(scope))).await.unwrap();
        assert!(lines.contains(&marker), "{scope}: {lines:?}");
        assert!(filter.matches(&marker), "{scope}");
    }
    let (lines, filter) = handle.logs(request(&address("api"))).await.unwrap();
    assert!(!lines.contains(&marker), "{lines:?}");
    assert!(!filter.matches(&marker));
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
    let (lines, filter) = handle.logs(request(&target)).await.unwrap();
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
async fn processes_show_and_take_policies_before_the_first_start() {
    let setup = Setup::new();
    let dir = setup.app("acme-web", WEB);
    setup.project("[apps.acme-web.processes.worker]\npolicy = \"disabled\"\n");
    let handle = setup.start();
    let processes = |handle: &Handle| -> Vec<(String, ProcessState, carte_api::Policy, bool)> {
        status(&handle.snapshot(), "acme-web", "main")
            .processes
            .iter()
            .map(|p| (p.name.clone(), p.state, p.policy, p.temporary))
            .collect()
    };
    let p = |name: &str, policy, temporary| {
        (name.to_string(), ProcessState::Stopped, policy, temporary)
    };
    use carte_api::Policy::{Auto, Disabled};
    assert_eq!(
        processes(&handle),
        [p("web", Auto, false), p("worker", Disabled, false)]
    );

    handle
        .control(control("web/worker", Action::Enable { save: false }))
        .await
        .unwrap();
    handle
        .control(control("web/web", Action::Disable { save: false }))
        .await
        .unwrap();
    assert_eq!(
        processes(&handle),
        [p("web", Disabled, true), p("worker", Auto, true)]
    );
    let error = handle
        .control(control("web/clock", Action::Disable { save: false }))
        .await
        .unwrap_err();
    assert!(error.contains("no process named clock"), "{error}");

    handle
        .control(control("web/worker", Action::Enable { save: true }))
        .await
        .unwrap();
    wait_for(&handle, "the saved policy", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .any(|p| p.name == "worker" && p.policy == Auto && !p.temporary)
    })
    .await;
    std::fs::write(
        dir.join("Procfile.dev"),
        "web: exec {fixture}\nclock: exec {fixture}\n"
            .replace("{fixture}", &fixture().display().to_string()),
    )
    .unwrap();
    handle.reload(discovery::load_world(&setup.paths.config_dir));
    wait_for(&handle, "the new process list", |s| {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .any(|p| p.name == "clock")
    })
    .await;
    assert_eq!(
        processes(&handle),
        [p("web", Disabled, true), p("clock", Auto, false)]
    );
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Stopped
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn running_instances_keep_the_processes_they_started_with() {
    let setup = Setup::new();
    let dir = setup.app("acme-web", WEB);
    setup.project("");
    let handle = setup.start();
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();

    std::fs::write(
        dir.join("Procfile.dev"),
        format!("web: exec {}\n", fixture().display()),
    )
    .unwrap();
    let world = discovery::load_world(&setup.paths.config_dir);
    handle.reload(world.clone());
    while *handle.world() != world {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let names = |s: &carte_api::Snapshot| -> Vec<String> {
        status(s, "acme-web", "main")
            .processes
            .iter()
            .map(|p| p.name.clone())
            .collect()
    };
    assert_eq!(names(&handle.snapshot()), ["web", "worker"]);

    handle.control(control("web", Action::Down)).await.unwrap();
    wait_for(&handle, "the new list once stopped", |s| {
        names(s) == ["web"]
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
            "../feature-x/acme-web",
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

fn event_lines(events: &[carte_api::Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{} {}", e.subject_label(), e.summary()))
        .collect()
}

async fn wait_for_events(handle: &Handle, what: &str, check: impl Fn(&[String]) -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (events, _) = handle.events(None, 100).await.unwrap();
            if check(&event_lines(&events)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let (events, _) = handle.events(None, 100).await.unwrap();
    assert!(
        result.is_ok(),
        "timed out waiting for {what}: {:#?}",
        event_lines(&events)
    );
}

#[tokio::test]
async fn events_tell_what_happened_and_why() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app(
        "acme-api",
        "web: FIXTURE_NAME=api exec {fixture}\nworker: FIXTURE_CRASH=4 exec {fixture}\n",
    );
    setup.project(
        "[apps.acme-web]\ndepends_on = [\"api\"]\n[apps.acme-api.processes.worker]\nrestart = \"never\"\n",
    );
    let handle = setup.start();
    let mut follow = handle.follow_events();

    let web = key("acme-web", Stack::Main);
    handle.ensure_ready(web.clone()).await.unwrap();
    wait_for_events(&handle, "both ready and the worker crash", |lines| {
        lines.iter().any(|l| l.starts_with("acme-web ready: in "))
            && lines.iter().any(|l| l.starts_with("acme-api ready: in "))
            && lines
                .iter()
                .any(|l| l == "acme-api/worker crashed: worker exited with code 4")
    })
    .await;
    let (events, _) = handle.events(None, 100).await.unwrap();
    let lines = event_lines(&events);
    assert_eq!(lines[0], "acme-web started: on request");
    assert_eq!(lines[1], "acme-api started: for acme-web");
    assert_eq!(
        events[1].subjects.len(),
        2,
        "a dependency's start is about both apps"
    );
    let crash = events
        .iter()
        .find(|e| e.kind == carte_api::EventKind::Crashed)
        .unwrap();
    assert_eq!(crash.exit_code, Some(4));
    assert!(events.iter().any(|e| e.boot_ms.is_some()));
    assert_eq!(follow.recv().await.unwrap(), events[0]);

    handle
        .control(waiting("web", Action::Restart))
        .await
        .unwrap();
    handle
        .control(control("web", Action::Pin { save: false }))
        .await
        .unwrap();
    handle.control(control("api", Action::Down)).await.unwrap();
    wait_for_events(&handle, "the manual actions", |lines| {
        let has = |line: &str| lines.iter().any(|l| l == line);
        has("acme-web restarted: by hand")
            && has("acme-web policy changed: pinned until the daemon restarts")
            && has("acme-api stopped: by hand")
    })
    .await;

    let api = Address {
        address: "api".into(),
        cwd: None,
        project: None,
    };
    let (api_events, filter) = handle.events(Some(api), 100).await.unwrap();
    assert_eq!(filter.app.as_deref(), Some("acme-api"));
    let api_lines = event_lines(&api_events);
    assert!(
        api_lines.iter().all(|l| l.starts_with("acme-api")
            || l == "acme-web started: on request"
            || l.starts_with("acme-web ")),
        "{api_lines:#?}"
    );
    assert!(
        !api_lines.iter().any(|l| l == "acme-web restarted: by hand"),
        "{api_lines:#?}"
    );
    let latest = status(&handle.snapshot(), "acme-api", "main")
        .last_event
        .clone()
        .unwrap();
    assert_eq!(latest.summary(), "stopped: by hand");
    handle.shutdown().await;
}

#[tokio::test]
async fn idle_stops_and_config_changes_are_events() {
    let setup = Setup::new();
    let web = setup.app("acme-web", WEB);
    setup.project("idle_timeout = \"1s\"\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    wait_for_events(&handle, "the idle stop", |lines| {
        lines.iter().any(|l| l == "acme-web stopped: idle for 1s")
    })
    .await;

    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature-x",
            "../feature-x/acme-web",
        ],
    );
    setup.project("idle_timeout = \"2s\"\n");
    handle.reload(discovery::load_world(&setup.paths.config_dir));
    wait_for_events(&handle, "the reload", |lines| {
        lines
            .iter()
            .any(|l| l == "acme config reloaded: project file")
            && lines.iter().any(|l| {
                l.starts_with("acme-web@feature-x worktree added: ")
                    && l.ends_with("feature-x/acme-web")
            })
    })
    .await;
    handle.shutdown().await;
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn process<'a>(
    snapshot: &'a carte_api::Snapshot,
    app: &str,
    name: &str,
) -> &'a carte_api::ProcessStatus {
    status(snapshot, app, "main")
        .processes
        .iter()
        .find(|p| p.name == name)
        .unwrap()
}

#[tokio::test]
async fn services_then_tasks_then_the_rest_start_in_order() {
    let setup = Setup::new();
    let path = setup.root.join("order.log");
    let log = path.display();
    setup.app(
        "acme-web",
        &format!("web: echo web >> {log} && exec {{fixture}}\n"),
    );
    setup.project(&format!(
        r#"
[apps.acme-web.processes.redis]
command = 'sleep 0.5 && echo redis >> {log} && exec {fixture}'
kind = "service"

[apps.acme-web.processes.cache]
command = 'sleep 30'
kind = "service"
policy = "disabled"

[apps.acme-web.processes.migrate]
command = 'sleep 0.3 && echo migrate >> {log}'
kind = "task"

[apps.acme-web.processes.seed]
command = 'echo seed >> {log}'
kind = "task"
after = ["migrate"]
"#,
        fixture = fixture().display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&path), ["redis", "migrate", "seed", "web"]);
    let snapshot = handle.snapshot();
    assert_eq!(
        process(&snapshot, "acme-web", "redis").state,
        ProcessState::Running
    );
    assert_eq!(
        process(&snapshot, "acme-web", "cache").state,
        ProcessState::Stopped
    );
    assert_eq!(
        process(&snapshot, "acme-web", "migrate").state,
        ProcessState::Done
    );
    assert_eq!(
        process(&snapshot, "acme-web", "seed").state,
        ProcessState::Done
    );
    assert_eq!(
        process(&snapshot, "acme-web", "web").state,
        ProcessState::Running
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn a_failing_service_or_task_fails_the_start_before_later_tiers() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\nworker: exec {fixture}\n");
    setup.app("acme-cache", "web: exec {fixture}\n");
    setup.project(&format!(
        r#"
[apps.acme-web.processes.redis]
command = 'exec {fixture}'
kind = "service"

[apps.acme-web.processes.migrate]
command = 'echo boom && exit 4'
kind = "task"

[apps.acme-cache.processes.redis]
command = 'echo no redis today && exit 2'
kind = "service"
"#,
        fixture = fixture().display()
    ));
    let handle = setup.start();

    let failure = handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap_err();
    assert_eq!(failure.title, "acme-web@main crashed");
    assert!(
        failure.detail.contains("migrate exited with code 4"),
        "{failure:?}"
    );
    assert!(
        failure.log.iter().any(|l| l.contains("boom")),
        "{failure:?}"
    );
    wait_for(&handle, "redis to stop", |s| {
        process(s, "acme-web", "redis").pid.is_none()
    })
    .await;
    let snapshot = handle.snapshot();
    let web = status(&snapshot, "acme-web", "main");
    assert_eq!(web.state, InstanceState::Crashed);
    assert!(
        web.last_error
            .as_deref()
            .is_some_and(|e| e.contains("migrate exited with code 4")),
        "{web:?}"
    );
    for name in ["web", "worker"] {
        let p = process(&snapshot, "acme-web", name);
        assert_eq!((p.state, p.pid), (ProcessState::Stopped, None), "{name}");
    }

    let failure = handle
        .control(waiting("cache", Action::Up))
        .await
        .unwrap()
        .failures;
    let [failure] = failure.as_slice() else {
        panic!("{failure:?}");
    };
    assert!(
        failure.detail.contains("redis exited with code 2"),
        "{failure:?}"
    );
    assert!(
        failure.log.iter().any(|l| l.contains("no redis today")),
        "{failure:?}"
    );
    let snapshot = handle.snapshot();
    assert_eq!(process(&snapshot, "acme-cache", "web").pid, None);
    handle.shutdown().await;
}

#[tokio::test]
async fn tasks_run_on_each_start_and_not_when_a_process_restarts() {
    let setup = Setup::new();
    let log = setup.root.join("runs.log");
    setup.app("acme-web", "web: exec {fixture}\nworker: exec {fixture}\n");
    setup.project(&format!(
        "[apps.acme-web.processes.migrate]\ncommand = 'echo run >> {}'\nkind = \"task\"\n",
        log.display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&log), ["run"]);
    let worker = process(&handle.snapshot(), "acme-web", "worker").pid;

    handle
        .control(control("web/worker", Action::Restart))
        .await
        .unwrap();
    wait_for(&handle, "the worker to restart", |s| {
        let p = process(s, "acme-web", "worker");
        p.state == ProcessState::Running && p.pid.is_some() && p.pid != worker
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lines(&log), ["run"]);
    let snapshot = handle.snapshot();
    assert_eq!(
        process(&snapshot, "acme-web", "migrate").state,
        ProcessState::Done
    );
    assert_eq!(
        status(&snapshot, "acme-web", "main").state,
        InstanceState::Ready
    );

    handle.control(control("web", Action::Down)).await.unwrap();
    wait_for(&handle, "stopped", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&log), ["run", "run"]);
    handle.shutdown().await;
}

#[tokio::test]
async fn services_stop_after_everything_else() {
    let setup = Setup::new();
    let path = setup.root.join("stop.log");
    let log = path.display();
    setup.app(
        "acme-web",
        &format!(
            "web: exec {{fixture}}\nworker: trap 'sleep 0.2; echo worker >> {log}; exit 0' TERM; while :; do sleep 0.05; done\n"
        ),
    );
    setup.project(&format!(
        r#"
[apps.acme-web.processes.redis]
command = '''trap 'echo redis >> {log}; exit 0' TERM; {fixture} & wait'''
kind = "service"
"#,
        fixture = fixture().display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    handle.control(control("web", Action::Down)).await.unwrap();
    wait_for(&handle, "stopped", |s| {
        status(s, "acme-web", "main").state == InstanceState::Stopped
    })
    .await;
    assert_eq!(lines(&path), ["worker", "redis"]);
    handle.shutdown().await;
}

/// The env a fixture on `port` echoes, once it answers.
async fn echoed_env(port: u16) -> String {
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let body = http_get(port, "/");
            return body.split_once("\"env\":").unwrap().1.to_string();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nothing answers on port {port}");
}

#[tokio::test]
async fn a_project_process_provides_env_to_the_apps_it_is_declared_for() {
    let setup = Setup::new();
    for app in ["acme-web", "acme-api", "acme-admin"] {
        setup.app(
            app,
            "web: FIXTURE_ENV=REDIS_URL,REDIS_PORT exec {fixture}\n",
        );
    }
    setup.project(&format!(
        r#"
[processes.redis]
apps = ["web", "acme-api"]
command = 'exec {}'
kind = "service"

[processes.redis.provides]
REDIS_URL = "unix://{{{{run_dir}}}}/redis.sock"
REDIS_PORT = "{{{{port}}}}"

[apps.acme-api.processes.redis]
policy = "disabled"
"#,
        fixture().display()
    ));
    let handle = setup.start();

    for app in ["acme-web", "acme-api", "acme-admin"] {
        handle.ensure_ready(key(app, Stack::Main)).await.unwrap();
    }
    let snapshot = handle.snapshot();
    let redis = process(&snapshot, "acme-web", "redis");
    assert_eq!(redis.state, ProcessState::Running);
    let run_dir = setup
        .paths
        .dir_roots()
        .instance(&key("acme-web", Stack::Main))
        .run;
    let env = echoed_env(process(&snapshot, "acme-web", "web").port.unwrap()).await;
    assert!(
        env.contains(&format!(
            "\"REDIS_URL\":\"unix://{}/redis.sock\"",
            run_dir.display()
        )),
        "{env}"
    );
    assert!(
        env.contains(&format!("\"REDIS_PORT\":\"{}\"", redis.port.unwrap())),
        "{env}"
    );

    assert_eq!(
        process(&snapshot, "acme-api", "redis").state,
        ProcessState::Stopped
    );
    let env = echoed_env(process(&snapshot, "acme-api", "web").port.unwrap()).await;
    assert!(env.contains("\"REDIS_URL\":\"unix://"), "{env}");

    let admin = status(&snapshot, "acme-admin", "main");
    assert!(admin.processes.iter().all(|p| p.name != "redis"));
    let env = echoed_env(process(&snapshot, "acme-admin", "web").port.unwrap()).await;
    assert!(env.contains("\"REDIS_URL\":null"), "{env}");
    handle.shutdown().await;
}

#[tokio::test]
async fn a_crashed_service_restarts_on_its_own_port_until_stopped_by_hand() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\n");
    setup.project(&format!(
        "[apps.acme-web.processes.redis]\ncommand = 'FIXTURE_ENV=PORT,CARTE_RUN_DIR,CARTE_STATE_DIR exec {}'\nkind = \"service\"\n",
        fixture().display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let before = process(&handle.snapshot(), "acme-web", "redis").clone();
    let env = echoed_env(before.port.unwrap()).await;
    assert!(env.contains("\"CARTE_RUN_DIR\":\"/"), "{env}");
    let pid = before.pid.unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "redis to come back", |s| {
        let p = process(s, "acme-web", "redis");
        p.state == ProcessState::Running && p.pid.is_some_and(|p| p != pid)
    })
    .await;
    let snapshot = handle.snapshot();
    let after = process(&snapshot, "acme-web", "redis");
    assert_eq!(after.port, before.port);
    assert_eq!(after.restarts, before.restarts + 1);
    assert_eq!(echoed_env(after.port.unwrap()).await, env);
    assert_eq!(
        status(&snapshot, "acme-web", "main").state,
        InstanceState::Ready
    );
    wait_for_events(&handle, "the restart", |lines| {
        lines
            .iter()
            .any(|l| l == "acme-web/redis restarted: after a crash")
    })
    .await;

    handle
        .control(control("web/redis", Action::Down))
        .await
        .unwrap();
    wait_for(&handle, "redis stopped", |s| {
        process(s, "acme-web", "redis").state == ProcessState::Stopped
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        process(&handle.snapshot(), "acme-web", "redis").state,
        ProcessState::Stopped
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn a_process_that_keeps_crashing_stays_down_until_started_by_hand() {
    let setup = Setup::new();
    let marker = setup.root.join("started");
    let path = setup.root.join("crashes.log");
    setup.app("acme-web", "web: exec {fixture}\n");
    setup.project(&format!(
        r#"
[apps.acme-web.processes.worker]
command = 'if [ -e {marker} ]; then echo crash >> {log}; exit 1; fi; touch {marker}; exec {fixture}'
"#,
        marker = marker.display(),
        log = path.display(),
        fixture = fixture().display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let pid = process(&handle.snapshot(), "acme-web", "worker")
        .pid
        .unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for_events(&handle, "carte to give up", |lines| {
        lines.iter().any(|l| {
            l == "acme-web/worker crashed: worker crashed 5 times within a minute; it stays down"
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lines(&path), ["crash"; 4]);
    let snapshot = handle.snapshot();
    let worker = process(&snapshot, "acme-web", "worker");
    assert!(
        matches!(worker.state, ProcessState::Exited { code: Some(1), .. }),
        "{worker:?}"
    );
    assert!(worker.gave_up);
    assert_eq!(
        status(&snapshot, "acme-web", "main").state,
        InstanceState::Ready
    );

    std::fs::remove_file(&marker).unwrap();
    handle
        .control(control("web/worker", Action::Up))
        .await
        .unwrap();
    wait_for(&handle, "the worker to run again", |s| {
        let p = process(s, "acme-web", "worker");
        p.state == ProcessState::Running && !p.gave_up
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn a_crashed_instance_restarts_once_its_processes_have_stopped_services_last() {
    let setup = Setup::new();
    let path = setup.root.join("stop.log");
    let log = path.display();
    setup.app(
        "acme-web",
        &format!(
            "web: exec {{fixture}}\nworker: trap 'sleep 0.3; echo worker >> {log}; exit 0' TERM; while :; do sleep 0.05; done\n"
        ),
    );
    setup.project(&format!(
        r#"
[apps.acme-web.processes.redis]
command = '''echo start >> {log}; trap 'echo redis >> {log}; exit 0' TERM; {fixture} & wait'''
kind = "service"
"#,
        fixture = fixture().display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let web = process(&handle.snapshot(), "acme-web", "web").pid.unwrap();
    kill(Pid::from_raw(web as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "the crash", |s| {
        status(s, "acme-web", "main").state == InstanceState::Crashed
    })
    .await;

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&path), ["start", "worker", "redis", "start"]);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_disabled_process_started_while_stopping_starts_with_the_restart() {
    let setup = Setup::new();
    setup.app(
        "acme-web",
        "web: exec {fixture}\nworker: trap 'sleep 0.3; exit 0' TERM; while :; do sleep 0.05; done\nextra: exec {fixture}\n",
    );
    setup.project("[apps.acme-web.processes.extra]\npolicy = \"disabled\"\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    handle.control(control("web", Action::Down)).await.unwrap();
    handle
        .control(control("web/extra", Action::Up))
        .await
        .unwrap();
    wait_for(&handle, "extra running after the restart", |s| {
        status(s, "acme-web", "main").state == InstanceState::Ready
            && process(s, "acme-web", "extra").state == ProcessState::Running
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn restarting_a_task_while_starting_keeps_later_tiers_waiting() {
    let setup = Setup::new();
    let path = setup.root.join("order.log");
    let log = path.display();
    setup.app(
        "acme-web",
        &format!("web: echo web >> {log} && exec {{fixture}}\n"),
    );
    setup.project(&format!(
        r#"
[apps.acme-web.processes.migrate]
command = 'sleep 1; echo migrate >> {log}'
kind = "task"
"#
    ));
    let handle = setup.start();

    handle.control(control("web", Action::Up)).await.unwrap();
    wait_for(&handle, "the task to run", |s| {
        process(s, "acme-web", "migrate").state == ProcessState::Running
    })
    .await;
    handle
        .control(control("web/migrate", Action::Restart))
        .await
        .unwrap();
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&path), ["migrate", "web"]);
    handle.shutdown().await;
}

#[tokio::test]
async fn restarting_a_stopping_task_while_starting_keeps_later_tiers_waiting() {
    let setup = Setup::new();
    let path = setup.root.join("order.log");
    let marker = setup.root.join("ran");
    let log = path.display();
    setup.app(
        "acme-web",
        &format!("web: echo web >> {log} && exec {{fixture}}\n"),
    );
    setup.project(&format!(
        r#"
[apps.acme-web.processes.migrate]
command = '''if [ -e {marker} ]; then sleep 0.3; echo migrate >> {log}; exit 0; fi; trap 'sleep 0.3; exit 0' TERM; touch {marker}; while :; do sleep 0.05; done'''
kind = "task"
"#,
        marker = marker.display()
    ));
    let handle = setup.start();

    handle.control(control("web", Action::Up)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    handle
        .control(control("web/migrate", Action::Down))
        .await
        .unwrap();
    handle
        .control(control("web/migrate", Action::Restart))
        .await
        .unwrap();
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    assert_eq!(lines(&path), ["migrate", "web"]);
    handle.shutdown().await;
}

#[tokio::test]
async fn stopping_a_service_while_starting_fails_the_start() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\n");
    setup.project(&format!(
        r#"
[apps.acme-web.processes.redis]
command = 'exec {fixture}'
kind = "service"

[apps.acme-web.processes.migrate]
command = 'sleep 30'
kind = "task"
"#,
        fixture = fixture().display()
    ));
    let handle = setup.start();

    let waiter = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.ensure_ready(key("acme-web", Stack::Main)).await })
    };
    wait_for(&handle, "the task to run", |s| {
        process(s, "acme-web", "migrate").state == ProcessState::Running
    })
    .await;
    handle
        .control(control("web/redis", Action::Down))
        .await
        .unwrap();
    let failure = waiter.await.unwrap().unwrap_err();
    assert!(
        failure
            .detail
            .contains("redis was stopped while acme-web@main was starting"),
        "{failure:?}"
    );
    let snapshot = handle.snapshot();
    assert_eq!(
        status(&snapshot, "acme-web", "main").state,
        InstanceState::Crashed
    );
    assert_eq!(process(&snapshot, "acme-web", "web").pid, None);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_crashed_worker_comes_back_on_its_own_port() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\nworker: exec {fixture}\n");
    setup.project("");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let before = process(&handle.snapshot(), "acme-web", "worker").clone();
    let pid = before.pid.unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "the worker to come back", |s| {
        let p = process(s, "acme-web", "worker");
        p.state == ProcessState::Running && p.pid.is_some_and(|p| p != pid)
    })
    .await;
    let snapshot = handle.snapshot();
    let after = process(&snapshot, "acme-web", "worker");
    assert_eq!(after.port, before.port);
    assert_eq!(after.restarts, before.restarts + 1);
    assert!(!after.gave_up);
    wait_for_events(&handle, "the restart", |lines| {
        lines
            .iter()
            .any(|l| l == "acme-web/worker restarted: after a crash")
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn a_clean_exit_or_restart_never_keeps_a_process_down_after_it_exits() {
    let setup = Setup::new();
    let log = setup.root.join("runs.log");
    setup.app(
        "acme-web",
        &format!(
            "web: exec {{fixture}}\nonce: sleep 0.3; echo once >> {log}\nkept: exec {{fixture}}\n",
            log = log.display()
        ),
    );
    setup.project("[apps.acme-web.processes.kept]\nrestart = \"never\"\n");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let pid = process(&handle.snapshot(), "acme-web", "kept").pid.unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "both to exit", |s| {
        matches!(
            process(s, "acme-web", "once").state,
            ProcessState::Exited { code: Some(0), .. }
        ) && matches!(
            process(s, "acme-web", "kept").state,
            ProcessState::Exited {
                signal: Some(9),
                ..
            }
        )
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let snapshot = handle.snapshot();
    assert_eq!(lines(&log), ["once"]);
    for name in ["once", "kept"] {
        let p = process(&snapshot, "acme-web", name);
        assert!(
            matches!(p.state, ProcessState::Exited { .. }) && p.pid.is_none() && !p.gave_up,
            "{p:?}"
        );
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn a_clean_exit_is_recorded_as_an_exit_not_a_crash() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\nonce: sleep 0.3; exit 0\n");
    setup.project("");
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    wait_for_events(&handle, "the exit", |lines| {
        lines
            .iter()
            .any(|l| l == "acme-web/once exited: once exited with code 0")
    })
    .await;
    let (events, _) = handle.events(None, 100).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == carte_api::EventKind::Crashed),
        "{:#?}",
        event_lines(&events)
    );
    let snapshot = handle.snapshot();
    let web = status(&snapshot, "acme-web", "main");
    assert_eq!(web.state, InstanceState::Ready);
    assert_eq!(web.last_error, None);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_start_deferred_until_processes_stop_keeps_its_reason() {
    let setup = Setup::new();
    setup.app("acme-web", WEB);
    setup.app(
        "acme-api",
        "web: FIXTURE_NAME=api exec {fixture}\nworker: trap 'sleep 0.5; exit 0' TERM; while :; do sleep 0.05; done\n",
    );
    setup.project("[apps.acme-web]\ndepends_on = [\"api\"]\n");
    let handle = setup.start();
    let api = key("acme-api", Stack::Main);

    handle.ensure_ready(api.clone()).await.unwrap();
    handle.control(control("api", Action::Down)).await.unwrap();
    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    handle.ensure_ready(api.clone()).await.unwrap();

    handle.control(control("api", Action::Down)).await.unwrap();
    handle.control(control("api", Action::Up)).await.unwrap();
    wait_for(&handle, "api to start again", |s| {
        status(s, "acme-api", "main").state == InstanceState::Ready
    })
    .await;

    let (events, _) = handle.events(None, 100).await.unwrap();
    let starts: Vec<String> = event_lines(&events)
        .into_iter()
        .filter(|l| l.starts_with("acme-api started: "))
        .collect();
    assert_eq!(
        starts,
        [
            "acme-api started: on request",
            "acme-api started: for acme-web",
            "acme-api started: by hand",
        ]
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn stopping_a_crashed_process_cancels_its_restart() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\nworker: exec {fixture}\n");
    setup.project("");
    let backoff = Duration::from_secs(2);
    let handle = setup.start_with_backoff(backoff);

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let pid = process(&handle.snapshot(), "acme-web", "worker")
        .pid
        .unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "the worker to exit", |s| {
        matches!(
            process(s, "acme-web", "worker").state,
            ProcessState::Exited { .. }
        )
    })
    .await;
    handle
        .control(control("web/worker", Action::Down))
        .await
        .unwrap();
    tokio::time::sleep(backoff * 3 / 2).await;
    let worker = process(&handle.snapshot(), "acme-web", "worker").clone();
    assert!(worker.pid.is_none(), "{worker:?}");
    assert_ne!(worker.state, ProcessState::Running);
    let (events, _) = handle.events(None, 100).await.unwrap();
    let lines = event_lines(&events);
    assert!(
        !lines
            .iter()
            .any(|l| l == "acme-web/worker restarted: after a crash"),
        "{lines:#?}"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn status_counts_down_to_a_crashed_process_restarting() {
    let setup = Setup::new();
    setup.app("acme-web", "web: exec {fixture}\nworker: exec {fixture}\n");
    setup.project("");
    let handle = setup.start_with_backoff(Duration::from_secs(30));

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    let pid = process(&handle.snapshot(), "acme-web", "worker")
        .pid
        .unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "the restart to wait", |s| {
        process(s, "acme-web", "worker").restart_in_secs.is_some()
    })
    .await;
    let worker = process(&handle.snapshot(), "acme-web", "worker").clone();
    assert!(matches!(worker.restart_in_secs, Some(1..=30)), "{worker:?}");

    handle
        .control(control("web/worker", Action::Up))
        .await
        .unwrap();
    wait_for(&handle, "the worker to run again", |s| {
        process(s, "acme-web", "worker").state == ProcessState::Running
    })
    .await;
    let worker = process(&handle.snapshot(), "acme-web", "worker").clone();
    assert_eq!(worker.restart_in_secs, None);

    let pid = worker.pid.unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "the second restart to wait", |s| {
        process(s, "acme-web", "worker")
            .restart_in_secs
            .is_some_and(|secs| secs > 30)
    })
    .await;
    handle
        .control(control("web/worker", Action::Down))
        .await
        .unwrap();
    wait_for(&handle, "the restart to be cancelled", |s| {
        process(s, "acme-web", "worker").restart_in_secs.is_none()
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test]
async fn a_task_that_fails_after_the_start_isnt_restarted() {
    let setup = Setup::new();
    let marker = setup.root.join("migrated");
    let log = setup.root.join("failures.log");
    setup.app("acme-web", "web: exec {fixture}\n");
    setup.project(&format!(
        r#"
[apps.acme-web.processes.migrate]
command = 'if [ -e {marker} ]; then echo fail >> {log}; exit 1; fi; touch {marker}'
kind = "task"
"#,
        marker = marker.display(),
        log = log.display()
    ));
    let handle = setup.start();

    handle
        .ensure_ready(key("acme-web", Stack::Main))
        .await
        .unwrap();
    handle
        .control(control("web/migrate", Action::Restart))
        .await
        .unwrap();
    wait_for(&handle, "the task to fail", |s| {
        matches!(
            process(s, "acme-web", "migrate").state,
            ProcessState::Exited { code: Some(1), .. }
        )
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(lines(&log), ["fail"]);
    assert_eq!(
        status(&handle.snapshot(), "acme-web", "main").state,
        InstanceState::Ready
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn a_crashed_checkout_watcher_comes_back_once_in_the_instance_that_ran_it() {
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
            "../feature-x/acme-web",
        ],
    );
    setup.project("");
    let handle = setup.start_with_backoff(Duration::from_millis(500));
    let css = |s: &carte_api::Snapshot, stack: &str| {
        status(s, "acme-api", stack)
            .processes
            .iter()
            .find(|p| p.name == "css")
            .cloned()
            .unwrap()
    };

    handle
        .ensure_ready(key("acme-api", Stack::Main))
        .await
        .unwrap();
    handle
        .ensure_ready(key("acme-api", Stack::Branch("feature-x".into())))
        .await
        .unwrap();
    wait_for(&handle, "css in main", |s| {
        css(s, "main").state == ProcessState::Running
    })
    .await;
    let pid = css(&handle.snapshot(), "main").pid.unwrap();
    kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for(&handle, "css to come back in main", |s| {
        let main = css(s, "main");
        main.state == ProcessState::Running && main.pid.is_some_and(|p| p != pid)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = handle.snapshot();
    assert_eq!(css(&snapshot, "main").restarts, 1);
    assert!(css(&snapshot, "feature-x").pid.is_none());
    handle.shutdown().await;
}

#[tokio::test]
async fn one_parked_copy_takes_over_a_checkout_watcher_when_its_owner_stops() {
    let setup = Setup::new();
    let web = setup.app("acme-web", WEB);
    setup.app(
        "acme-api",
        "web: FIXTURE_NAME=api exec {fixture}\ncss: FIXTURE_NAME=css exec {fixture}\n",
    );
    for branch in ["feature-x", "feature-y"] {
        git(
            &web,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &format!("../{branch}/acme-web"),
            ],
        );
    }
    setup.project("");
    let backoff = Duration::from_millis(300);
    let handle = setup.start_with_backoff(backoff);
    let css = |s: &carte_api::Snapshot, stack: &str| {
        status(s, "acme-api", stack)
            .processes
            .iter()
            .find(|p| p.name == "css")
            .cloned()
            .unwrap()
    };
    let branches = ["feature-x", "feature-y"];

    handle
        .ensure_ready(key("acme-api", Stack::Main))
        .await
        .unwrap();
    for branch in branches {
        handle
            .ensure_ready(key("acme-api", Stack::Branch(branch.into())))
            .await
            .unwrap();
        handle
            .control(control(&format!("api@{branch}/css"), Action::Up))
            .await
            .unwrap();
    }
    wait_for(&handle, "css in every stack", |s| {
        ["main", "feature-x", "feature-y"]
            .iter()
            .all(|stack| css(s, stack).state == ProcessState::Running)
    })
    .await;
    for branch in branches {
        let pid = css(&handle.snapshot(), branch).pid.unwrap();
        kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    }
    tokio::time::sleep(backoff * 3).await;
    let snapshot = handle.snapshot();
    for branch in branches {
        assert!(css(&snapshot, branch).pid.is_none());
    }

    handle
        .control(control("api@main", Action::Down))
        .await
        .unwrap();
    let running = |s: &carte_api::Snapshot| {
        branches
            .iter()
            .filter(|branch| css(s, branch).state == ProcessState::Running)
            .count()
    };
    wait_for(&handle, "css taken over", |s| running(s) > 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(running(&handle.snapshot()), 1);
    handle.shutdown().await;
}

/// Opens `first` then `second` by request, then stops the project.
async fn open_in_turn(handle: &Handle, first: &str, second: &str) {
    for app in [first, second] {
        handle.request(key(app, Stack::Main), None).await.unwrap();
    }
    handle.control(control("acme", Action::Down)).await.unwrap();
    wait_for(handle, "everything stopped", |s| {
        [first, second]
            .iter()
            .all(|app| status(s, app, "main").state == InstanceState::Stopped)
    })
    .await;
}

#[tokio::test]
async fn apps_that_usually_open_next_are_warmed() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_NAME=web exec {fixture}\n");
    setup.app("acme-billing", "web: FIXTURE_NAME=billing exec {fixture}\n");
    setup.project("");
    let handle = setup.start();

    for _ in 0..3 {
        open_in_turn(&handle, "acme-web", "acme-billing").await;
    }
    let warms = |s: &carte_api::Snapshot| status(s, "acme-web", "main").warms.clone();
    assert_eq!(
        warms(&handle.snapshot()),
        [carte_api::Warm {
            app: "acme-billing".into(),
            hits: 3,
            opens: 3,
        }]
    );

    handle
        .request(key("acme-web", Stack::Main), None)
        .await
        .unwrap();
    wait_for(&handle, "billing warmed", |s| {
        status(s, "acme-billing", "main").state == InstanceState::Ready
    })
    .await;
    let (events, _) = handle.events(None, 100).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message == "warmed after acme-web opened"),
        "{events:#?}"
    );

    handle.shutdown().await;
    assert!(setup.paths.warming().is_file());
    let handle = setup.start();
    assert_eq!(warms(&handle.snapshot()).len(), 1);
    handle.shutdown().await;
}

#[tokio::test]
async fn apps_with_warm_off_are_never_warmed() {
    let setup = Setup::new();
    setup.app("acme-web", "web: FIXTURE_NAME=web exec {fixture}\n");
    setup.app("acme-billing", "web: FIXTURE_NAME=billing exec {fixture}\n");
    setup.project("[apps.acme-billing]\nwarm = false\n");
    let handle = setup.start();

    for _ in 0..3 {
        open_in_turn(&handle, "acme-web", "acme-billing").await;
    }
    assert!(
        status(&handle.snapshot(), "acme-web", "main")
            .warms
            .is_empty()
    );
    handle
        .request(key("acme-web", Stack::Main), None)
        .await
        .unwrap();
    let snapshot = handle.snapshot();
    assert_eq!(
        status(&snapshot, "acme-billing", "main").state,
        InstanceState::Stopped
    );
    handle.shutdown().await;
}
