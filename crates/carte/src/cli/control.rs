use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::bail;
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Hold, InstanceState, InstanceStatus, LogLine,
    LogsRequest, ProcessState, Snapshot, StopAllRequest, paths,
};
use carte_core::{address, discovery, naming};
use http::Method;

use super::{LogsArgs, Target};
use crate::client::Client;
use crate::paths::Paths;

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn address(target: &Target) -> Address {
    Address {
        address: target.address.clone().unwrap_or_default(),
        cwd: std::env::current_dir()
            .ok()
            .map(|dir| dir.canonicalize().unwrap_or(dir)),
    }
}

pub fn daemon(paths: &Paths) -> anyhow::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CARTE_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    runtime()?.block_on(crate::daemon::run(paths.clone()))?;
    Ok(ExitCode::SUCCESS)
}

pub fn ls(paths: &Paths, all: bool, json: bool) -> anyhow::Result<ExitCode> {
    let snapshot: Snapshot =
        runtime()?.block_on(Client::new(&paths.socket()).get(paths::SNAPSHOT))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", render_ls(&snapshot, all));
    Ok(ExitCode::SUCCESS)
}

pub fn render_ls(snapshot: &Snapshot, all: bool) -> String {
    let mut rows = Vec::new();
    let mut stopped = 0;
    for project in &snapshot.projects {
        for stack in &project.stacks {
            for inst in &stack.instances {
                let shown = all
                    || inst.state != InstanceState::Stopped
                    || inst.checkout == stack.name && stack.name != "main";
                if !shown {
                    stopped += 1;
                    continue;
                }
                rows.push(vec![
                    project.name.clone(),
                    stack.name.clone(),
                    inst.app.clone(),
                    state_word(inst),
                    checkout_word(inst),
                    inst.url.clone(),
                    detail(inst),
                ]);
            }
        }
    }
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("Nothing is running.\n");
    } else {
        let header = ["PROJECT", "STACK", "APP", "STATE", "CHECKOUT", "URL", ""];
        let mut table = vec![header.iter().map(|s| s.to_string()).collect::<Vec<_>>()];
        table.extend(rows);
        out.push_str(&columns(&table));
    }
    if stopped > 0 {
        out.push_str(&format!(
            "{} running, {stopped} stopped (carte ls --all lists them)\n",
            snapshot.running_instances
        ));
    }
    for problem in &snapshot.problems {
        let place = [problem.project.as_deref(), problem.app.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("/");
        out.push_str(&format!("problem: {place}: {}\n", problem.message));
    }
    out
}

fn state_word(inst: &InstanceStatus) -> String {
    match inst.state {
        InstanceState::Stopped => "stopped",
        InstanceState::Starting => "starting",
        InstanceState::Ready => "ready",
        InstanceState::Stopping => "stopping",
        InstanceState::Crashed => "crashed",
    }
    .to_string()
}

fn checkout_word(inst: &InstanceStatus) -> String {
    if inst.stack != "main" && inst.checkout == "main" {
        "main (fallback)".into()
    } else {
        inst.checkout.clone()
    }
}

fn detail(inst: &InstanceStatus) -> String {
    let mut parts = Vec::new();
    let down: Vec<&str> = inst
        .processes
        .iter()
        .filter(|p| !matches!(p.state, ProcessState::Running | ProcessState::Starting))
        .map(|p| p.name.as_str())
        .collect();
    if inst.state == InstanceState::Ready && !down.is_empty() {
        parts.push(format!("down: {}", down.join(", ")));
    }
    for hold in &inst.holds {
        parts.push(match hold {
            Hold::Pinned => "pinned".into(),
            Hold::Requests { count } => format!("{count} requests"),
            Hold::Connections { count } => format!("held by {count} connections"),
            Hold::Dependent { app } => format!("held by {app}"),
        });
    }
    if let Some(secs) = inst.idle_in_secs {
        parts.push(format!(
            "idle in {}",
            carte_core::duration::format(std::time::Duration::from_secs(secs))
        ));
    }
    if let Some(error) = &inst.last_error
        && inst.state == InstanceState::Crashed
    {
        parts.push(error.clone());
    }
    parts.join(", ")
}

fn columns(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

pub fn control(
    paths: &Paths,
    target: Target,
    action: Action,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let request = ControlRequest {
        target: address(&target),
        action,
    };
    let response: ControlResponse =
        runtime()?.block_on(Client::new(&paths.socket()).post(paths::CONTROL, &request))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(ExitCode::SUCCESS);
    }
    let verb = match action {
        Action::Up => "starting",
        Action::Down => "stopping",
        Action::Restart => "restarting",
        Action::Pin { .. } => "pinned",
        Action::Unpin { .. } => "unpinned",
        Action::Disable { .. } => "disabled",
        Action::Enable { .. } => "enabled",
    };
    if response.affected.is_empty() {
        println!("Nothing to do.");
    }
    for affected in &response.affected {
        println!("{verb} {affected}");
    }
    for message in &response.messages {
        eprintln!("{message}");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn logs(paths: &Paths, args: LogsArgs, json: bool) -> anyhow::Result<ExitCode> {
    let request = LogsRequest {
        target: address(&args.target),
        follow: args.follow,
        tail: Some(args.lines),
        search: args.search,
    };
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut stdout = std::io::stdout().lock();
    let mut labels: Vec<String> = Vec::new();
    runtime()?.block_on(Client::new(&paths.socket()).stream(
        Method::POST,
        paths::LOGS,
        Some(&request),
        |line: LogLine| {
            if json {
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&line).unwrap_or_default()
                );
                return;
            }
            let label = format!("{}@{}/{}", line.app, line.stack, line.process);
            let index = labels.iter().position(|l| *l == label).unwrap_or_else(|| {
                labels.push(label.clone());
                labels.len() - 1
            });
            let _ = if color {
                writeln!(
                    stdout,
                    "\x1b[{}m{label}\x1b[0m {}",
                    31 + index % 6,
                    line.text
                )
            } else {
                writeln!(stdout, "{label} {}", line.text)
            };
        },
    ))?;
    Ok(ExitCode::SUCCESS)
}

pub fn open(paths: &Paths, target: Target) -> anyhow::Result<ExitCode> {
    let world = discovery::load_world(&paths.config_dir);
    let address = address(&target);
    let parsed: address::Address = address.address.parse()?;
    let resolved = address::resolve(&world, &parsed, address.cwd.as_deref())?;
    let Some(key) = resolved.instance() else {
        bail!("name an app to open");
    };
    let state = world.project(&key.project).expect("resolved project");
    let app = state.project.app(&key.app).expect("resolved app");
    let url = naming::instance_url(&world.machine, &state.project, app, &key.stack);
    println!("{url}");
    let status = std::process::Command::new("open").arg(&url).status()?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub fn stop_all(paths: &Paths, idle: bool, json: bool) -> anyhow::Result<ExitCode> {
    let response: ControlResponse = runtime()?.block_on(
        Client::new(&paths.socket()).post(paths::STOP_ALL, &StopAllRequest { idle_only: idle }),
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.affected.is_empty() {
        println!("Nothing to stop.");
    } else {
        for affected in &response.affected {
            println!("stopping {affected}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
