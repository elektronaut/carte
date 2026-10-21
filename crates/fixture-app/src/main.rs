//! Test fixture for carte: a small HTTP server whose behaviour is set by env vars.
//!
//! | Variable | Effect |
//! | --- | --- |
//! | `PORT` | Port to listen on; without it the app only runs its other knobs and sleeps |
//! | `FIXTURE_NAME` | Name reported by `GET /` (default `fixture`) |
//! | `FIXTURE_BIND` | Address to bind, `127.0.0.1` (default) or `::1`; `--ipv6` also selects `::1` |
//! | `FIXTURE_ENV` | Comma-separated env var names echoed by `GET /` |
//! | `FIXTURE_BOOT_DELAY_MS` | Sleep before listening |
//! | `FIXTURE_CRASH` | Exit with this code during boot |
//! | `FIXTURE_IGNORE_TERM` | `1` ignores SIGTERM |
//! | `FIXTURE_LINES` | Print this many numbered lines on start |
//! | `FIXTURE_PARTIAL` | Print this text without a trailing newline on start |
//! | `FIXTURE_CHILD_PIDFILE` | Spawn a child that stays in the process group, write its pid here |
//! | `FIXTURE_GROUP_CHILD_PIDFILE` | Spawn a child in a process group of its own, write its pid here |
//! | `FIXTURE_DAEMON_PIDFILE` | Spawn a self-daemonising grandchild (double fork + setsid), write its pid here |
//! | `FIXTURE_ECHO_STDIN` | `1` echoes each stdin line to stdout as `echo: <line>` |
//! | `FIXTURE_TAKE_TERMINAL` | `1` moves to a process group of its own, makes it the terminal's foreground group and exits |
//! | `FIXTURE_LIFELINE_PID` | Exit when this process exits; passed on to spawned children |
//! | `FIXTURE_GUARD_PID` | With `FIXTURE_LIFELINE_PID`, only watch: SIGKILL this pid when the lifeline exits, and exit once either is gone |
//!
//! `GET /headers` answers with the peer address and the request headers as received.
//!
//! Every response names the app in an `X-Fixture-Name` header.
//!
//! On start it prints `tty: true` or `tty: false` for stdout, then `ready` once listening.

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{self, Command};
use std::thread;
use std::time::Duration;

const LIFELINE_POLL: Duration = Duration::from_millis(200);

fn main() {
    if let Some(lifeline) = env_parse::<libc::pid_t>("FIXTURE_LIFELINE_PID") {
        if let Some(guarded) = env_parse::<libc::pid_t>("FIXTURE_GUARD_PID") {
            guard(lifeline, guarded);
        }
        thread::spawn(move || {
            while alive(lifeline) {
                thread::sleep(LIFELINE_POLL);
            }
            process::exit(0);
        });
    }

    if env::var("FIXTURE_TAKE_TERMINAL").as_deref() == Ok("1") {
        take_terminal();
    }

    if env::var_os("FIXTURE_SLEEPER").is_some() {
        sleep_forever();
    }

    let name = env::var("FIXTURE_NAME").unwrap_or_else(|_| "fixture".into());
    let mut stdout = io::stdout();
    writeln!(stdout, "tty: {}", stdout.is_terminal()).ok();

    if let Some(n) = env_parse::<usize>("FIXTURE_LINES") {
        for i in 1..=n {
            writeln!(stdout, "line {i}").ok();
        }
    }
    if let Ok(partial) = env::var("FIXTURE_PARTIAL") {
        write!(stdout, "{partial}").ok();
        stdout.flush().ok();
    }

    if env::var("FIXTURE_IGNORE_TERM").as_deref() == Ok("1") {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }

    if let Ok(path) = env::var("FIXTURE_CHILD_PIDFILE") {
        spawn_child(&path, false);
    }
    if let Ok(path) = env::var("FIXTURE_GROUP_CHILD_PIDFILE") {
        spawn_child(&path, true);
    }
    if let Ok(path) = env::var("FIXTURE_DAEMON_PIDFILE") {
        spawn_daemon(&path);
    }

    if env::var("FIXTURE_ECHO_STDIN").as_deref() == Ok("1") {
        thread::spawn(echo_stdin);
    }

    if let Some(ms) = env_parse::<u64>("FIXTURE_BOOT_DELAY_MS") {
        thread::sleep(Duration::from_millis(ms));
    }

    if let Some(code) = env_parse::<i32>("FIXTURE_CRASH") {
        eprintln!("crashing with {code}");
        process::exit(code);
    }

    let Some(port) = env_parse::<u16>("PORT") else {
        println!("ready");
        sleep_forever();
    };

    let ipv6 =
        env::args().any(|a| a == "--ipv6") || env::var("FIXTURE_BIND").as_deref() == Ok("::1");
    let addr = if ipv6 {
        format!("[::1]:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("bind {addr}: {e}");
        process::exit(1);
    });
    println!("ready");

    for stream in listener.incoming().flatten() {
        let name = name.clone();
        thread::spawn(move || {
            let _ = serve(stream, &name);
        });
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok()?.trim().parse().ok()
}

fn alive(pid: libc::pid_t) -> bool {
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn guard(lifeline: libc::pid_t, guarded: libc::pid_t) -> ! {
    while alive(guarded) {
        if !alive(lifeline) {
            unsafe { libc::kill(guarded, libc::SIGKILL) };
            break;
        }
        thread::sleep(LIFELINE_POLL);
    }
    process::exit(0);
}

fn take_terminal() -> ! {
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::setpgid(0, 0);
        libc::tcsetpgrp(0, libc::getpid());
    }
    process::exit(0);
}

fn sleep_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn echo_stdin() {
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        println!("echo: {line}");
    }
}

fn spawn_child(pidfile: &str, own_group: bool) {
    let exe = env::current_exe().expect("current_exe");
    let mut command = Command::new(exe);
    command.env_clear().env("FIXTURE_SLEEPER", "1");
    if own_group {
        command.process_group(0);
    }
    if let Some(lifeline) = env::var_os("FIXTURE_LIFELINE_PID") {
        command.env("FIXTURE_LIFELINE_PID", lifeline);
    }
    match command.spawn() {
        Ok(child) => fs::write(pidfile, child.id().to_string()).expect("write child pidfile"),
        Err(e) => eprintln!("spawn child: {e}"),
    }
}

fn spawn_daemon(pidfile: &str) {
    let exe = env::current_exe().expect("current_exe");
    let exe = std::ffi::CString::new(exe.into_os_string().into_encoded_bytes()).expect("exe path");
    let pidfile = pidfile.to_owned();
    unsafe {
        match libc::fork() {
            -1 => eprintln!("fork: {}", io::Error::last_os_error()),
            0 => {
                libc::setsid();
                if libc::fork() == 0 {
                    let _ = fs::write(&pidfile, libc::getpid().to_string());
                    libc::setenv(c"FIXTURE_SLEEPER".as_ptr(), c"1".as_ptr(), 1);
                    let argv = [exe.as_ptr(), std::ptr::null()];
                    libc::execv(exe.as_ptr(), argv.as_ptr());
                }
                libc::_exit(0);
            }
            pid => {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
    }
}

fn serve(stream: TcpStream, name: &str) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut content_length = 0usize;
    let mut headers = format!("peer: {}\n", stream.peer_addr()?.ip());
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        headers.push_str(header.trim_end());
        headers.push('\n');
        if let Some((k, v)) = header.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (status, content_type, body) = match path {
        "/up" => ("200 OK", "text/plain", "ok".to_owned()),
        "/" => ("200 OK", "application/json", index_json(name)),
        "/headers" => ("200 OK", "text/plain", headers),
        _ => ("404 Not Found", "text/plain", "not found".to_owned()),
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Fixture-Name: {name}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn index_json(name: &str) -> String {
    let vars: Vec<String> = env::var("FIXTURE_ENV")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| {
            let value = env::var(k)
                .map(|v| json_string(&v))
                .unwrap_or_else(|_| "null".into());
            format!("{}:{value}", json_string(k))
        })
        .collect();
    format!(
        "{{\"name\":{},\"pid\":{},\"env\":{{{}}}}}",
        json_string(name),
        process::id(),
        vars.join(",")
    )
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
