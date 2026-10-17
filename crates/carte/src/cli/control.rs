use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::bail;
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Hold, InstanceState, InstanceStatus, LogLine,
    LogsRequest, ProcessState, Snapshot, StopAllRequest, paths,
};
use carte_core::model::World;
use carte_core::{address, discovery, naming};
use http::Method;

use super::{LogsArgs, ShareArgs, Target};
use crate::client::Client;
use crate::exit::Failure;
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
        project: target.project.clone(),
    }
}

/// Resolves an address against the configuration on disk, as the daemon would.
fn resolve_locally(world: &World, address: &Address) -> Result<address::Target, Failure> {
    let parsed: address::Address = address.address.parse()?;
    let cwd = address.cwd.as_deref();
    Ok(match &address.project {
        Some(project) => address::resolve_in_project(world, project, &parsed, cwd)?,
        None => address::resolve(world, &parsed, cwd)?,
    })
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
                    || inst.public_url.is_some()
                    || inst.checkout == stack.name && stack.name != "main";
                if !shown {
                    stopped += 1;
                    continue;
                }
                let share = snapshot
                    .shares
                    .iter()
                    .find(|s| s.project == project.name && s.stack == stack.name);
                rows.push(vec![
                    instance_name(inst),
                    state_word(inst),
                    inst.url.clone(),
                    notes(inst, share),
                ]);
            }
        }
    }
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("Nothing is running.\n");
    } else {
        let header = ["NAME", "STATE", "URL", ""];
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
    if let Some(tunnel) = &snapshot.tunnel
        && tunnel.state == carte_api::TunnelState::Failed
    {
        out.push_str(&format!(
            "tunnel: {} isn't working: {}\n",
            tunnel.provider,
            tunnel.message.as_deref().unwrap_or("no details")
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

/// The address `carte up` takes for the instance.
fn instance_name(inst: &InstanceStatus) -> String {
    if inst.stack == "main" {
        inst.app.clone()
    } else {
        format!("{}@{}", inst.app, inst.stack)
    }
}

fn notes(inst: &InstanceStatus, share: Option<&carte_api::ShareStatus>) -> String {
    let mut parts = Vec::new();
    if inst.stack != "main" && inst.checkout == "main" {
        parts.push("main checkout".to_string());
    }
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
    if let Some(url) = &inst.public_url {
        parts.push(format!("public {url}"));
        if let Some(share) = share {
            if share.auth {
                parts.push("password".into());
            }
            if let Some(secs) = share.expires_in_secs {
                parts.push(format!(
                    "share ends in {}",
                    carte_core::duration::format(std::time::Duration::from_secs(secs))
                ));
            }
        }
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
    let resolved = resolve_locally(&world, &address)?;
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

pub fn share(paths: &Paths, args: ShareArgs, json: bool) -> anyhow::Result<ExitCode> {
    let ttl_secs = match &args.ttl {
        Some(ttl) => Some(carte_core::duration::parse(ttl)?.as_secs()),
        None => None,
    };
    let request = carte_api::ShareRequest {
        target: address(&args.target),
        apps: args.apps,
        ttl_secs,
        auth: args.auth,
        pin: args.pin,
        replace: args.replace,
    };
    let client = Client::new(&paths.socket());
    let runtime = runtime()?;
    let response: carte_api::ShareResponse =
        runtime.block_on(client.post(paths::SHARE, &request))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(ExitCode::SUCCESS);
    }
    let share = &response.share;
    println!("Shared from {}@{}:", share.project, share.stack);
    for app in &share.apps {
        println!("  {}  {}", app.app, app.url);
    }
    if let Some(credentials) = &response.credentials {
        println!(
            "User {}, password {}",
            credentials.user, credentials.password
        );
    }
    match share.expires_in_secs {
        Some(secs) => println!(
            "Ends in {}, or with `carte unshare`.",
            carte_core::duration::format(std::time::Duration::from_secs(secs))
        ),
        None => println!("Stays shared until `carte unshare` or the daemon restarts."),
    }
    for message in &response.messages {
        eprintln!("note: {message}");
    }
    let snapshot: Snapshot = runtime.block_on(client.get(paths::SNAPSHOT))?;
    if let Some(tunnel) = &snapshot.tunnel
        && tunnel.state == carte_api::TunnelState::Failed
    {
        eprintln!(
            "The {} tunnel isn't working: {}",
            tunnel.provider,
            tunnel.message.as_deref().unwrap_or("no details")
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn unshare(paths: &Paths, target: Target, json: bool) -> anyhow::Result<ExitCode> {
    let request = carte_api::UnshareRequest {
        target: address(&target),
    };
    let apps: Vec<String> =
        runtime()?.block_on(Client::new(&paths.socket()).post(paths::UNSHARE, &request))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&apps)?);
    } else {
        println!("No longer shared: {}", apps.join(", "));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures::{instance, shared_sample};

    #[test]
    fn lists_instances_by_address() {
        let mut snapshot = shared_sample();
        snapshot.running_instances = 2;
        let mut fallback = instance("nova-billing", "feature-x", "main", InstanceState::Ready);
        fallback.idle_in_secs = Some(300);
        snapshot.projects[0].stacks[1].instances[0] = fallback;
        assert_eq!(
            render_ls(&snapshot, false),
            "\
NAME                     STATE    URL
nova-billing             ready    https://nova-billing.test   public https://nova-billing--main--nova.demo.dev, password
nova-billing@feature-x   ready    https://nova-billing.test   main checkout, idle in 5m
nova-frontend@feature-x  stopped  https://nova-frontend.test
2 running, 1 stopped (carte ls --all lists them)
"
        );
    }
}
