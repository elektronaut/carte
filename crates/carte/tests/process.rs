mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use carte::process::{IoMode, Output, ProcessHandle, SpawnSpec, Spawner};
use common::{fixture, guard, lifeline};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;

fn spawner() -> Spawner {
    Spawner::new(env!("CARGO_BIN_EXE_carte"))
}

fn spec() -> SpawnSpec {
    SpawnSpec::new(fixture().display().to_string(), std::env::temp_dir())
        .env("FIXTURE_LIFELINE_PID", lifeline())
}

async fn read_until(output: &mut Output, seen: &mut Vec<u8>, needle: &str) {
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(seen).contains(needle) {
            let n = output.read(&mut buf).await.unwrap();
            assert!(n > 0, "EOF before {needle:?}");
            seen.extend_from_slice(&buf[..n]);
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "timed out waiting for {needle:?}, got {:?}",
        String::from_utf8_lossy(seen)
    );
}

async fn read_to_end(output: &mut Output) -> String {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), output.read_to_end(&mut seen))
        .await
        .expect("output EOF")
        .unwrap();
    String::from_utf8_lossy(&seen).into_owned()
}

async fn run_to_end(handle: &mut ProcessHandle) -> (ExitStatus, String) {
    let mut output = handle.take_output().unwrap();
    let (status, text) = tokio::join!(handle.wait(), read_to_end(&mut output));
    (status.unwrap(), text)
}

fn drain(mut output: Output) {
    tokio::spawn(async move { while output.read(&mut [0u8; 4096]).await.is_ok_and(|n| n > 0) {} });
}

fn alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

async fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} still alive");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn read_pidfile(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(pid) = s.trim().parse()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no pidfile at {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(mut handle: ProcessHandle) {
    handle.stop(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn pty_output_is_line_buffered_and_a_tty() {
    let mut handle = spawner()
        .spawn(
            &spec()
                .env("FIXTURE_LINES", "3")
                .env("FIXTURE_PARTIAL", "partial>"),
        )
        .await
        .unwrap();
    assert_ne!(handle.pid(), handle.shim_pid());
    let mut output = handle.take_output().unwrap();
    let mut seen = Vec::new();
    read_until(&mut output, &mut seen, "partial>").await;
    drain(output);
    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.starts_with("tty: true\r\nline 1\r\nline 2\r\nline 3\r\n"),
        "{text:?}"
    );
    assert!(alive(handle.pid()));
    stop(handle).await;
}

#[tokio::test]
async fn pipe_mode_merges_output_without_a_tty() {
    let mut handle = spawner()
        .spawn(
            &spec()
                .io(IoMode::Pipes)
                .env("FIXTURE_LINES", "2")
                .env("FIXTURE_CRASH", "4"),
        )
        .await
        .unwrap();
    let (status, text) = run_to_end(&mut handle).await;
    assert!(text.contains("tty: false\nline 1\nline 2\n"), "{text:?}");
    assert!(text.contains("crashing with 4\n"), "{text:?}");
    assert_eq!(status.code(), Some(4));
}

#[tokio::test]
async fn crash_output_survives_exit_on_a_pty() {
    let mut handle = spawner()
        .spawn(&spec().env("FIXTURE_LINES", "50").env("FIXTURE_CRASH", "3"))
        .await
        .unwrap();
    let (status, text) = run_to_end(&mut handle).await;
    assert_eq!(status.code(), Some(3));
    assert!(text.contains("line 50\r\n"), "{text:?}");
    assert!(text.contains("crashing with 3"), "{text:?}");
}

#[tokio::test]
async fn pty_input_reaches_the_foreground_group() {
    let mut handle = spawner()
        .spawn(&spec().env("FIXTURE_ECHO_STDIN", "1"))
        .await
        .unwrap();
    let mut output = handle.take_output().unwrap();
    let mut seen = Vec::new();
    read_until(&mut output, &mut seen, "ready").await;
    handle.write_input(b"hello\n").await.unwrap();
    read_until(&mut output, &mut seen, "echo: hello").await;
    drain(output);
    stop(handle).await;
}

#[tokio::test]
async fn command_runs_with_given_env_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mut handle = spawner()
        .spawn(
            &SpawnSpec::new("echo \"$GREETING from $(pwd -P)\"", dir.path()).env("GREETING", "hi"),
        )
        .await
        .unwrap();
    let (status, text) = run_to_end(&mut handle).await;
    let cwd = dir.path().canonicalize().unwrap();
    assert_eq!(text, format!("hi from {}\r\n", cwd.display()));
    assert!(status.success());
}

#[tokio::test]
async fn serves_http_on_port() {
    for ipv6 in [false, true] {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut spec = spec()
            .env("PORT", port.to_string())
            .env("FIXTURE_NAME", "web")
            .env("FIXTURE_ENV", "PORT,MISSING");
        if ipv6 {
            spec = spec.env("FIXTURE_BIND", "::1");
        }
        let mut handle = spawner().spawn(&spec).await.unwrap();
        let mut output = handle.take_output().unwrap();
        read_until(&mut output, &mut Vec::new(), "ready").await;
        drain(output);

        let host = if ipv6 {
            format!("[::1]:{port}")
        } else {
            format!("127.0.0.1:{port}")
        };
        assert!(get(&host, "/up").ends_with("\r\n\r\nok"));
        let body = get(&host, "/");
        let expected = format!(
            r#"{{"name":"web","pid":{},"env":{{"PORT":"{port}","MISSING":null}}}}"#,
            handle.pid()
        );
        assert!(body.ends_with(&expected), "{body}");
        stop(handle).await;
    }
}

fn get(host: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(host).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[tokio::test]
async fn stop_terminates_the_group() {
    let mut handle = spawner().spawn(&spec()).await.unwrap();
    drain(handle.take_output().unwrap());
    let pid = handle.pid();
    let started = Instant::now();
    let status = handle.stop(Duration::from_secs(5)).await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!alive(pid));
    assert!(handle.signal_group(Signal::SIGTERM).is_err());
}

#[tokio::test]
async fn stop_escalates_to_kill() {
    let mut handle = spawner()
        .spawn(&spec().env("FIXTURE_IGNORE_TERM", "1"))
        .await
        .unwrap();
    let mut output = handle.take_output().unwrap();
    read_until(&mut output, &mut Vec::new(), "ready").await;
    drain(output);
    let started = Instant::now();
    let status = handle.stop(Duration::from_millis(300)).await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn suspend_character_on_the_pty_does_not_stop_the_app() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut handle = spawner()
        .spawn(&spec().env("PORT", port.to_string()))
        .await
        .unwrap();
    let mut output = handle.take_output().unwrap();
    read_until(&mut output, &mut Vec::new(), "ready").await;
    drain(output);
    handle.write_input(b"\x1a").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let host = format!("127.0.0.1:{port}");
    let response = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(host).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(stream, "GET /up HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).map(|_| response)
    })
    .await
    .unwrap();
    assert!(
        response.as_ref().is_ok_and(|r| r.ends_with("\r\n\r\nok")),
        "{response:?}"
    );
    stop(handle).await;
}

/// Waits until `pid` is stopped.
async fn wait_stopped(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ps = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&ps.stdout)
            .trim_start()
            .starts_with('T')
        {
            return;
        }
        assert!(Instant::now() < deadline, "pid {pid} not stopped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A command that leaves `marker` behind when SIGTERM reaches its handler.
fn trapping_term(marker: &Path) -> SpawnSpec {
    SpawnSpec::new(
        format!(
            "trap 'touch {}; exit 3' TERM; echo ready; while :; do sleep 0.1; done",
            marker.display()
        ),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn stop_resumes_a_stopped_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("terminated");
    let mut handle = spawner().spawn(&trapping_term(&marker)).await.unwrap();
    let mut output = handle.take_output().unwrap();
    read_until(&mut output, &mut Vec::new(), "ready").await;
    drain(output);
    handle.signal_group(Signal::SIGSTOP).unwrap();
    wait_stopped(handle.pid()).await;
    let started = Instant::now();
    let status = handle.stop(Duration::from_secs(5)).await.unwrap();
    assert_eq!(status.code(), Some(3));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(marker.exists());
}

#[tokio::test]
async fn closing_the_lifeline_resumes_a_stopped_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("terminated");
    let mut handle = spawner().spawn(&trapping_term(&marker)).await.unwrap();
    let mut output = handle.take_output().unwrap();
    read_until(&mut output, &mut Vec::new(), "ready").await;
    drain(output);
    handle.signal_group(Signal::SIGSTOP).unwrap();
    wait_stopped(handle.pid()).await;
    handle.close_lifeline();
    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(marker.exists());
}

#[tokio::test]
async fn sigint_is_delivered_to_the_group() {
    let mut handle = spawner().spawn(&spec()).await.unwrap();
    let mut output = handle.take_output().unwrap();
    read_until(&mut output, &mut Vec::new(), "ready").await;
    drain(output);
    handle.signal_group(Signal::SIGINT).unwrap();
    assert_eq!(handle.wait().await.unwrap().signal(), Some(libc::SIGINT));
}

#[tokio::test]
async fn shim_forwards_signals_to_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let mut handle = spawner()
        .spawn(&spec().env("FIXTURE_CHILD_PIDFILE", &pidfile))
        .await
        .unwrap();
    drain(handle.take_output().unwrap());
    let child = read_pidfile(&pidfile).await;
    kill(Pid::from_raw(handle.shim_pid() as i32), Signal::SIGTERM).unwrap();
    assert_eq!(handle.wait().await.unwrap().signal(), Some(libc::SIGTERM));
    wait_dead(child).await;
}

#[tokio::test]
async fn dropping_the_handle_kills_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let handle = spawner()
        .spawn(
            &spec()
                .env("FIXTURE_CHILD_PIDFILE", &pidfile)
                .env("FIXTURE_IGNORE_TERM", "1"),
        )
        .await
        .unwrap();
    let (pid, shim) = (handle.pid(), handle.shim_pid());
    let child = read_pidfile(&pidfile).await;
    assert!(alive(child));
    drop(handle);
    wait_dead(child).await;
    wait_dead(pid).await;
    wait_dead(shim).await;
}

#[tokio::test]
async fn closing_the_lifeline_alone_kills_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let mut handle = spawner()
        .spawn(&spec().env("FIXTURE_CHILD_PIDFILE", &pidfile))
        .await
        .unwrap();
    drain(handle.take_output().unwrap());
    let child = read_pidfile(&pidfile).await;
    handle.close_lifeline();
    let status = tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    wait_dead(handle.pid()).await;
    wait_dead(child).await;
}

#[tokio::test]
async fn self_daemonising_child_escapes_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("daemon.pid");
    let handle = spawner()
        .spawn(&spec().env("FIXTURE_DAEMON_PIDFILE", &pidfile))
        .await
        .unwrap();
    let pid = handle.pid();
    let daemon = read_pidfile(&pidfile).await;
    drop(handle);
    wait_dead(pid).await;
    let escaped = alive(daemon);
    let _ = kill(Pid::from_raw(daemon as i32), Signal::SIGKILL);
    assert!(
        escaped,
        "expected the setsid grandchild to survive the group kill"
    );
}

#[tokio::test]
async fn failing_command_reports_its_exit_code() {
    let mut handle = spawner()
        .spawn(&SpawnSpec::new("exit 7", "/"))
        .await
        .unwrap();
    assert_eq!(run_to_end(&mut handle).await.0.code(), Some(7));
    let mut handle = spawner()
        .spawn(&SpawnSpec::new("no-such-command-xyz", "/"))
        .await
        .unwrap();
    let (status, text) = run_to_end(&mut handle).await;
    assert_eq!(status.code(), Some(127));
    assert!(text.contains("no-such-command-xyz"), "{text:?}");
}

/// Plays the daemon for `sigkilled_daemon_takes_its_groups_down`; a no-op otherwise.
#[tokio::test]
async fn fake_daemon() {
    let Some(dir) = std::env::var_os("CARTE_TEST_FAKE_DAEMON") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let mut handles = Vec::new();
    for (i, io) in [IoMode::default(), IoMode::Pipes].into_iter().enumerate() {
        let pidfile = dir.join(format!("child{i}.pid"));
        let mut handle = spawner()
            .spawn(
                &spec()
                    .io(io)
                    .env("FIXTURE_CHILD_PIDFILE", &pidfile)
                    .env("FIXTURE_IGNORE_TERM", "1"),
            )
            .await
            .unwrap();
        drain(handle.take_output().unwrap());
        std::fs::write(dir.join(format!("leader{i}.pid")), handle.pid().to_string()).unwrap();
        handles.push(handle);
    }
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn sigkilled_daemon_takes_its_groups_down() {
    let dir = tempfile::tempdir().unwrap();
    fixture();
    let mut daemon = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fake_daemon", "--nocapture"])
        .env("CARTE_TEST_FAKE_DAEMON", dir.path())
        .env("FIXTURE_LIFELINE_PID", lifeline())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    guard(daemon.id());
    let mut pids = Vec::new();
    for i in 0..2 {
        pids.push(read_pidfile(&dir.path().join(format!("leader{i}.pid"))).await);
        pids.push(read_pidfile(&dir.path().join(format!("child{i}.pid"))).await);
    }
    assert!(pids.iter().all(|&pid| alive(pid)));
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    for pid in pids {
        wait_dead(pid).await;
    }
}

/// The descriptors above stderr that `pid` has open.
fn open_fds(pid: u32) -> Vec<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|dir| {
                dir.flatten()
                    .filter(|e| e.file_name().to_string_lossy().parse::<u32>().unwrap_or(0) > 2)
                    .map(|e| {
                        std::fs::read_link(e.path())
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    let lsof = std::process::Command::new("lsof")
        .args(["-p", &pid.to_string(), "-F", "fn"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&lsof.stdout).into_owned();
    let mut fds = Vec::new();
    let mut open = false;
    for line in text.lines() {
        if let Some(fd) = line.strip_prefix('f') {
            open = fd.parse::<u32>().is_ok_and(|fd| fd > 2);
        } else if let Some(name) = line.strip_prefix('n')
            && open
        {
            fds.push(name.to_string());
        }
    }
    fds
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_descriptors_dont_leak_into_provider_commands() {
    use carte_core::env::{CommandRunner, CommandSpec, SystemRunner};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let done = Arc::new(AtomicBool::new(false));
    let provider = std::thread::spawn({
        let done = done.clone();
        move || {
            let runner = SystemRunner {
                search_path: Vec::new(),
                own_group: true,
            };
            let spec = CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "sleep 5 </dev/null >/dev/null 2>&1 & echo $!".into(),
                ],
                cwd: "/".into(),
                env: None,
                timeout: Duration::from_secs(10),
            };
            let mut pids = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let output = runner.run(&spec).unwrap();
                pids.extend(
                    String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .parse::<u32>(),
                );
            }
            pids
        }
    });
    for io in [IoMode::default(), IoMode::Pipes].repeat(25) {
        let mut handle = spawner()
            .spawn(&SpawnSpec::new("true", "/").io(io))
            .await
            .unwrap();
        run_to_end(&mut handle).await;
    }
    done.store(true, Ordering::Relaxed);
    let pids = provider.join().unwrap();
    let leaked: Vec<(u32, Vec<String>)> = pids
        .iter()
        .map(|&pid| (pid, open_fds(pid)))
        .filter(|(_, fds)| !fds.is_empty())
        .collect();
    for &pid in &pids {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    assert!(!pids.is_empty());
    assert!(
        leaked.is_empty(),
        "{} of {}: {leaked:?}",
        leaked.len(),
        pids.len()
    );
}
