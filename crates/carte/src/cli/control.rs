use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::bail;
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Event, EventsRequest, Hold, InstanceState,
    InstanceStatus, LogLine, LogsRequest, MarkRequest, ProcessState, Snapshot, StartFailure,
    StatusResponse, StopAllRequest, paths,
};
use carte_core::kube::{self, KubeAddress};
use carte_core::model::{Stack, World};
use carte_core::{address, discovery, naming};
use http::Method;

use super::{EventsArgs, LogsArgs, ShareArgs, Target};
use crate::client::Client;
use crate::exit::{Exit, Failure};
use crate::paths::Paths;

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

pub(crate) fn address(target: &Target) -> Address {
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

pub fn restart_daemon(paths: &Paths) -> anyhow::Result<ExitCode> {
    use crate::setup::plist::{LABEL, runs_with_state};
    if !cfg!(target_os = "macos") {
        bail!("`carte daemon restart` restarts carte's LaunchAgent, which only exists on macOS");
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let service = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });
    if !crate::setup::check::launchd_loaded(&service) {
        bail!("carte's LaunchAgent isn't loaded; run `carte setup` to install it");
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let plist = std::fs::read_to_string(home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
        .unwrap_or_default();
    if !runs_with_state(&plist, &paths.state_dir) {
        bail!(
            "carte's LaunchAgent doesn't run the daemon for {}; restart that one yourself",
            paths.state_dir.display()
        );
    }
    let before = crate::setup::check::launchd_pid(&service);
    let status = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &service])
        .status()?;
    if !status.success() {
        bail!("`launchctl kickstart -k {service}` failed");
    }
    let client = Client::new(&paths.socket());
    let runtime = runtime()?;
    let started = std::time::Instant::now();
    loop {
        let pid = crate::setup::check::launchd_pid(&service);
        if pid.is_some()
            && pid != before
            && runtime
                .block_on(client.get::<Snapshot>(paths::SNAPSHOT))
                .is_ok()
        {
            break;
        }
        if started.elapsed() > std::time::Duration::from_secs(15) {
            bail!("carte restarted but doesn't answer yet; see `carte setup --check`");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!("Restarted carte");
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

pub fn status(paths: &Paths, target: Target, json: bool) -> anyhow::Result<ExitCode> {
    let (snapshot, resolved, response) = status_of(paths, &target)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.instances.is_empty() {
        println!("Nothing is running in {}.", describe(&resolved));
    } else {
        print!("{}", render_status(&snapshot, &response.instances));
    }
    Ok(ExitCode::SUCCESS)
}

/// The status of what `target` names, with the snapshot and resolution it came from.
fn status_of(
    paths: &Paths,
    target: &Target,
) -> anyhow::Result<(Snapshot, address::Target, StatusResponse)> {
    let world = discovery::load_world(&paths.config_dir);
    let address = address(target);
    if let Some(kube) = resolve_kube(&world, &address) {
        let kube = kube?;
        let snapshot: Snapshot =
            runtime()?.block_on(Client::new(&paths.socket()).get(paths::SNAPSHOT))?;
        let response = StatusResponse {
            instances: select_kube(&snapshot, &kube).into_iter().cloned().collect(),
        };
        let resolved = address::Target {
            project: kube.project,
            app: kube.service,
            stack: kube.namespace.map(Stack::Branch),
            process: None,
        };
        return Ok((snapshot, resolved, response));
    }
    let resolved = resolve_locally(&world, &address)?;
    let snapshot: Snapshot =
        runtime()?.block_on(Client::new(&paths.socket()).get(paths::SNAPSHOT))?;
    let response = StatusResponse {
        instances: select(&snapshot, &resolved).into_iter().cloned().collect(),
    };
    Ok((snapshot, resolved, response))
}

/// What `carte status --json` prints.
pub(crate) fn status_response(paths: &Paths, target: &Target) -> anyhow::Result<StatusResponse> {
    Ok(status_of(paths, target)?.2)
}

/// A Service's URL: with its namespace when the address or the project file names
/// one, otherwise the short form that leaves it to the cluster.
fn kube_url(world: &World, address: KubeAddress) -> anyhow::Result<String> {
    let Some(service) = &address.service else {
        bail!(
            "name a Service to open, like `carte open grafana.{}`",
            address.project
        );
    };
    let project = world
        .kube_project(&address.project)
        .expect("resolved project");
    let namespace = address
        .namespace
        .clone()
        .or_else(|| project.service(service).and_then(|s| s.namespace.clone()));
    if let Some(namespace) = namespace.as_ref().filter(|n| !project.allows_namespace(n)) {
        bail!(
            "{namespace} isn't in `namespaces` of the {} project file",
            project.name
        );
    }
    Ok(match namespace {
        Some(namespace) => kube::url(&world.machine, &project.name, &namespace, service),
        None => {
            let host = format!("{service}.{}.{}", project.name, world.machine.tld);
            match world.machine.https_port {
                443 => format!("https://{host}"),
                port => format!("https://{host}:{port}"),
            }
        }
    })
}

fn resolve_kube(world: &World, address: &Address) -> Option<Result<KubeAddress, Failure>> {
    let parsed: address::Address = match address.address.parse() {
        Ok(parsed) => parsed,
        Err(err) => return Some(Err(Failure::from(err))),
    };
    kube::resolve_address(world, address.project.as_deref(), &parsed)
        .map(|resolved| resolved.map_err(Failure::from))
}

/// The kube instances an address names: a Service's in every namespace carte knows
/// it in, otherwise the running or failed ones.
fn select_kube<'a>(snapshot: &'a Snapshot, address: &KubeAddress) -> Vec<&'a InstanceStatus> {
    snapshot
        .projects
        .iter()
        .filter(|p| p.name == address.project)
        .flat_map(|p| &p.stacks)
        .filter(|s| address.namespace.as_ref().is_none_or(|n| s.name == *n))
        .flat_map(|s| &s.instances)
        .filter(|i| match &address.service {
            Some(service) => i.app == *service,
            None => i.state != InstanceState::Stopped,
        })
        .collect()
}

/// The instances a resolved address names: an app's instance in its stack (main when
/// none is given) even when stopped, otherwise the running or shared ones.
fn select<'a>(snapshot: &'a Snapshot, target: &address::Target) -> Vec<&'a InstanceStatus> {
    let stack = target
        .stack
        .as_ref()
        .map(|s| s.slug().to_string())
        .or_else(|| target.app.as_ref().map(|_| "main".to_string()));
    snapshot
        .projects
        .iter()
        .filter(|p| p.name == target.project)
        .flat_map(|p| &p.stacks)
        .filter(|s| stack.as_ref().is_none_or(|name| s.name == *name))
        .flat_map(|s| &s.instances)
        .filter(|i| match &target.app {
            Some(app) => i.app == *app,
            None => i.state != InstanceState::Stopped || i.public_url.is_some(),
        })
        .collect()
}

fn describe(target: &address::Target) -> String {
    match &target.stack {
        Some(stack) => format!("{}@{}", target.project, stack.slug()),
        None => target.project.clone(),
    }
}

pub fn render_status(snapshot: &Snapshot, instances: &[InstanceStatus]) -> String {
    let mut out = String::new();
    for (i, inst) in instances.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{}  {}\n", instance_name(inst), state_word(inst)));
        let mut rows: Vec<(&str, String)> = vec![("url", inst.url.clone())];
        if let Some(url) = &inst.public_url {
            let share = snapshot
                .shares
                .iter()
                .find(|s| s.stack == inst.stack && s.apps.iter().any(|a| a.app == inst.app));
            let mut public = url.clone();
            if share.is_some_and(|s| s.auth) {
                public.push_str(", password");
            }
            if let Some(secs) = share.and_then(|s| s.expires_in_secs) {
                public.push_str(&format!(
                    ", ends in {}",
                    carte_core::duration::format(std::time::Duration::from_secs(secs))
                ));
            }
            rows.push(("public", public));
        }
        match &inst.kube {
            Some(kube) => {
                if let (Some(pod), Some(port)) = (&kube.pod, kube.port) {
                    rows.push(("pod", format!("{pod} :{port}")));
                }
            }
            None => {
                let fallback = if inst.stack != "main" && inst.checkout == "main" {
                    " (main checkout)"
                } else {
                    ""
                };
                rows.push((
                    "checkout",
                    format!("{}{fallback}", inst.checkout_path.display()),
                ));
            }
        }
        if !inst.processes.is_empty() {
            let processes: Vec<String> = inst.processes.iter().map(process_word).collect();
            rows.push(("processes", processes.join(", ")));
        }
        let holds: Vec<String> = inst.holds.iter().map(hold_word).collect();
        if !holds.is_empty() {
            rows.push(("held by", holds.join(", ")));
        }
        if !inst.warms.is_empty() {
            let warms: Vec<String> = inst
                .warms
                .iter()
                .map(|w| format!("{} ({}/{})", w.app, w.hits, w.opens))
                .collect();
            rows.push(("warms", warms.join(", ")));
        }
        if let Some(secs) = inst.idle_in_secs {
            rows.push((
                "idle in",
                carte_core::duration::format(std::time::Duration::from_secs(secs)),
            ));
        }
        if let Some(error) = &inst.last_error {
            rows.push(("last error", error.clone()));
        }
        if inst.env_changed {
            rows.push(("env", "changed since it started; restart to use it".into()));
        }
        let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (key, value) in rows {
            out.push_str(&format!("  {key:width$}  {value}\n"));
        }
    }
    out
}

fn process_word(process: &carte_api::ProcessStatus) -> String {
    let state = match process.state {
        ProcessState::Stopped => "stopped".to_string(),
        ProcessState::Starting => "starting".to_string(),
        ProcessState::Running => "running".to_string(),
        ProcessState::Stopping => "stopping".to_string(),
        ProcessState::Exited {
            code: Some(code), ..
        } => format!("exited {code}"),
        ProcessState::Exited {
            signal: Some(signal),
            ..
        } => format!("killed by signal {signal}"),
        ProcessState::Exited { .. } => "exited".to_string(),
        ProcessState::Disabled => "disabled".to_string(),
        ProcessState::Done => "done".to_string(),
    };
    let state = if process.gave_up {
        format!("{state}, gave up restarting")
    } else if let Some(secs) = process.restart_in_secs {
        format!(
            "{state}, restarting in {}",
            carte_core::duration::format(std::time::Duration::from_secs(secs))
        )
    } else {
        state
    };
    match process.port {
        Some(port) if process.pid.is_some() => format!("{} {state} :{port}", process.name),
        _ => format!("{} {state}", process.name),
    }
}

fn hold_word(hold: &Hold) -> String {
    match hold {
        Hold::Pinned => "pinned".into(),
        Hold::Requests { count } => format!("{count} requests"),
        Hold::Connections { count } => format!("{count} connections"),
        Hold::Dependent { app } => app.clone(),
        Hold::Exec { count } => execs(*count),
    }
}

fn execs(count: u32) -> String {
    if count == 1 {
        "1 exec".into()
    } else {
        format!("{count} execs")
    }
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
                let name = match inst.kube {
                    Some(_) => format!("{}.{}@{}", inst.app, project.name, inst.stack),
                    None => instance_name(inst),
                };
                rows.push(vec![
                    name,
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
    for project in &snapshot.projects {
        if let Some(error) = project.kube.as_ref().and_then(|k| k.error.as_ref()) {
            out.push_str(&format!("cluster: {}: {error}\n", project.name));
        }
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
    if inst.kube.is_some() && inst.state == InstanceState::Crashed {
        return "failed".into();
    }
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
    if let Some(pod) = inst.kube.as_ref().and_then(|k| k.pod.as_ref()) {
        parts.push(format!("pod {pod}"));
    }
    let down: Vec<String> = inst
        .processes
        .iter()
        .filter(|p| {
            !matches!(
                p.state,
                ProcessState::Running | ProcessState::Starting | ProcessState::Done
            )
        })
        .map(|p| {
            if p.gave_up {
                format!("{} (gave up)", p.name)
            } else if let Some(secs) = p.restart_in_secs {
                format!(
                    "{} (restarting in {})",
                    p.name,
                    carte_core::duration::format(std::time::Duration::from_secs(secs))
                )
            } else {
                p.name.clone()
            }
        })
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
            Hold::Exec { count } => format!("held by {}", execs(*count)),
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
    wait: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let response = send_control(paths, &target, action, wait)?;
    let code = if response.failures.is_empty() {
        Exit::Ok
    } else {
        Exit::NotReady
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(code.into());
    }
    print!("{}", render_control(&response, action, wait));
    for message in &response.messages {
        eprintln!("{message}");
    }
    eprint!("{}", render_failures(&response.failures));
    Ok(code.into())
}

pub(crate) fn send_control(
    paths: &Paths,
    target: &Target,
    action: Action,
    wait: bool,
) -> anyhow::Result<ControlResponse> {
    let request = ControlRequest {
        target: address(target),
        action,
        wait,
    };
    runtime()?.block_on(Client::new(&paths.socket()).post(paths::CONTROL, &request))
}

fn render_control(response: &ControlResponse, action: Action, waited: bool) -> String {
    let verb = match action {
        Action::Up if waited => "ready",
        Action::Restart if waited => "restarted",
        Action::Up => "starting",
        Action::Down => "stopping",
        Action::Restart => "restarting",
        Action::Pin { .. } => "pinned",
        Action::Unpin { .. } => "unpinned",
        Action::Disable { .. } => "disabled",
        Action::Enable { .. } => "enabled",
    };
    if response.affected.is_empty() {
        return "Nothing to do.\n".into();
    }
    let failed = |affected: &str| {
        response.failures.iter().any(|f| {
            affected == f.instance
                || affected
                    .strip_prefix(f.instance.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    response
        .affected
        .iter()
        .filter(|affected| !failed(affected))
        .map(|affected| format!("{verb} {affected}\n"))
        .collect()
}

fn render_failures(failures: &[StartFailure]) -> String {
    let mut out = String::new();
    for failure in failures {
        out.push_str(&format!(
            "{} didn't get ready: {}: {}\n",
            failure.instance, failure.title, failure.detail
        ));
        let skip = failure.log.len().saturating_sub(20);
        for line in &failure.log[skip..] {
            out.push_str(&format!("  | {line}\n"));
        }
    }
    out
}

/// Columns of the rule a marker prints as.
const MARKER_WIDTH: usize = 60;

pub(crate) fn logs_request(args: LogsArgs) -> LogsRequest {
    let since_ms = args
        .since
        .map(|since| crate::supervisor::runner::now_ms().saturating_sub(since.as_millis() as u64));
    LogsRequest {
        target: address(&args.target),
        follow: args.follow,
        tail: args.lines,
        search: args.search,
        since_ms,
        since_start: args.since_start,
        top: args.rank.is_some().then_some(args.top),
        rank: args.rank,
    }
}

/// The lines a request without `follow` returns.
pub(crate) fn collect_logs(paths: &Paths, request: &LogsRequest) -> anyhow::Result<Vec<LogLine>> {
    let mut lines = Vec::new();
    runtime()?.block_on(Client::new(&paths.socket()).stream(
        Method::POST,
        paths::LOGS,
        Some(request),
        |line: LogLine| lines.push(line),
    ))?;
    Ok(lines)
}

pub fn logs(paths: &Paths, args: LogsArgs, json: bool) -> anyhow::Result<ExitCode> {
    if args.mark {
        let request = MarkRequest {
            target: address(&args.target),
        };
        let marker: LogLine =
            runtime()?.block_on(Client::new(&paths.socket()).post(paths::MARK, &request))?;
        if json {
            println!("{}", serde_json::to_string(&marker)?);
        } else {
            println!("{}", crate::logs::marker_rule(marker.at_ms, MARKER_WIDTH));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let request = logs_request(args);
    let timed = request.rank.is_some();
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
            if line.marker {
                let _ = writeln!(
                    stdout,
                    "{}",
                    crate::logs::marker_rule(line.at_ms, MARKER_WIDTH)
                );
                return;
            }
            let label = format!("{}@{}/{}", line.app, line.stack, line.process);
            let index = labels.iter().position(|l| *l == label).unwrap_or_else(|| {
                labels.push(label.clone());
                labels.len() - 1
            });
            let time = if timed {
                format!("{} ", crate::logs::clock(line.at_ms))
            } else {
                String::new()
            };
            let _ = if color {
                writeln!(
                    stdout,
                    "{time}\x1b[{}m{label}\x1b[0m {}",
                    31 + index % 6,
                    line.text
                )
            } else {
                writeln!(stdout, "{time}{label} {}", line.text)
            };
        },
    ))?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn events_request(args: EventsArgs) -> EventsRequest {
    let target = Target {
        address: args.address,
        project: args.project,
    };
    let scoped = target.address.is_some() || target.project.is_some();
    EventsRequest {
        target: scoped.then(|| address(&target)),
        follow: args.follow,
        tail: Some(args.lines),
    }
}

/// The events a request without `follow` returns.
pub(crate) fn collect_events(paths: &Paths, request: &EventsRequest) -> anyhow::Result<Vec<Event>> {
    let mut events = Vec::new();
    runtime()?.block_on(Client::new(&paths.socket()).stream(
        Method::POST,
        paths::EVENTS,
        Some(request),
        |event: Event| events.push(event),
    ))?;
    Ok(events)
}

pub fn events(paths: &Paths, args: EventsArgs, json: bool) -> anyhow::Result<ExitCode> {
    let request = events_request(args);
    let mut stdout = std::io::stdout().lock();
    runtime()?.block_on(Client::new(&paths.socket()).stream(
        Method::POST,
        paths::EVENTS,
        Some(&request),
        |event: Event| {
            let line = if json {
                serde_json::to_string(&event).unwrap_or_default()
            } else {
                crate::clock::event_line(&event)
            };
            let _ = writeln!(stdout, "{line}");
        },
    ))?;
    Ok(ExitCode::SUCCESS)
}

pub fn open(paths: &Paths, target: Target) -> anyhow::Result<ExitCode> {
    let world = discovery::load_world(&paths.config_dir);
    let address = address(&target);
    let url = match resolve_kube(&world, &address) {
        Some(kube) => kube_url(&world, kube?)?,
        None => {
            let resolved = resolve_locally(&world, &address)?;
            let Some(key) = resolved.instance() else {
                bail!("name an app to open");
            };
            let state = world.project(&key.project).expect("resolved project");
            let app = state.project.app(&key.app).expect("resolved app");
            naming::instance_url(&world.machine, &state.project, app, &key.stack)
        }
    };
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
    use carte_core::model::Stack;

    fn target(app: Option<&str>, stack: Option<&str>) -> address::Target {
        address::Target {
            project: "nova".into(),
            app: app.map(Into::into),
            stack: stack.map(|s| match s {
                "main" => Stack::Main,
                branch => Stack::Branch(branch.into()),
            }),
            process: None,
        }
    }

    #[test]
    fn status_shows_an_app_whether_running_or_not() {
        let snapshot = shared_sample();
        let names = |t| {
            select(&snapshot, &t)
                .iter()
                .map(|i| instance_name(i))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(target(Some("nova-billing"), None)), ["nova-billing"]);
        assert_eq!(
            names(target(Some("nova-frontend"), Some("feature-x"))),
            ["nova-frontend@feature-x"]
        );
        assert_eq!(names(target(None, None)), ["nova-billing"]);
        assert!(names(target(None, Some("feature-x"))).is_empty());
    }

    #[test]
    fn waited_starts_report_ready_and_failures_with_their_log() {
        let response = ControlResponse {
            affected: vec!["nova-billing@main".into(), "nova-frontend@main/web".into()],
            messages: Vec::new(),
            failures: vec![StartFailure {
                instance: "nova-frontend@main".into(),
                title: "nova-frontend@main crashed".into(),
                detail: "web exited with code 1".into(),
                log: (1..=25).map(|i| format!("line {i}")).collect(),
            }],
        };
        assert_eq!(
            render_control(&response, Action::Up, true),
            "ready nova-billing@main\n"
        );
        assert_eq!(
            render_control(&response, Action::Up, false),
            "starting nova-billing@main\n"
        );
        let failures = render_failures(&response.failures);
        assert!(failures.starts_with(
            "nova-frontend@main didn't get ready: nova-frontend@main crashed: web exited with code 1\n  | line 6\n"
        ), "{failures}");
        assert!(failures.ends_with("  | line 25\n"), "{failures}");
    }

    #[test]
    fn renders_status() {
        let mut snapshot = shared_sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[0].pid = Some(10);
        billing.processes[0].port = Some(54321);
        billing.processes[1].state = ProcessState::Exited {
            code: Some(1),
            signal: None,
        };
        billing.holds = vec![Hold::Connections { count: 2 }];
        billing.idle_in_secs = Some(840);
        billing.last_error = Some("worker exited with code 1".into());
        billing.warms = vec![
            carte_api::Warm {
                app: "nova-frontend".into(),
                hits: 8,
                opens: 10,
            },
            carte_api::Warm {
                app: "nova-auth".into(),
                hits: 5,
                opens: 10,
            },
        ];
        let instances = vec![snapshot.projects[0].stacks[0].instances[0].clone()];
        assert_eq!(
            render_status(&snapshot, &instances),
            "\
nova-billing  ready
  url         https://nova-billing.test
  public      https://nova-billing--main--nova.demo.dev, password
  checkout    /work/nova-billing
  processes   web running :54321, worker exited 1
  held by     2 connections
  warms       nova-frontend (8/10), nova-auth (5/10)
  idle in     14m
  last error  worker exited with code 1
"
        );
    }

    #[test]
    fn shows_processes_carte_gave_up_on() {
        let mut snapshot = shared_sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Exited {
            code: None,
            signal: Some(9),
        };
        billing.processes[1].gave_up = true;
        let instances = vec![billing.clone()];
        let status = render_status(&snapshot, &instances);
        assert!(
            status.contains(
                "processes  web running, worker killed by signal 9, gave up restarting\n"
            ),
            "{status}"
        );
        let ls = render_ls(&snapshot, false);
        assert!(ls.contains("down: worker (gave up)"), "{ls}");
    }

    #[test]
    fn shows_when_a_crashed_process_restarts() {
        let mut snapshot = shared_sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.processes[1].state = ProcessState::Exited {
            code: Some(1),
            signal: None,
        };
        billing.processes[1].restart_in_secs = Some(4);
        let instances = vec![billing.clone()];
        let status = render_status(&snapshot, &instances);
        assert!(
            status.contains("processes  web running, worker exited 1, restarting in 4s\n"),
            "{status}"
        );
        let ls = render_ls(&snapshot, false);
        assert!(ls.contains("down: worker (restarting in 4s)"), "{ls}");
    }

    #[test]
    fn shows_commands_that_hold_an_instance() {
        let mut snapshot = shared_sample();
        let billing = &mut snapshot.projects[0].stacks[0].instances[0];
        billing.holds = vec![Hold::Exec { count: 1 }];
        let instances = vec![billing.clone()];
        let status = render_status(&snapshot, &instances);
        assert!(status.contains("  held by    1 exec\n"), "{status}");
        let ls = render_ls(&snapshot, false);
        assert!(ls.contains("held by 1 exec"), "{ls}");

        snapshot.projects[0].stacks[0].instances[0].holds =
            vec![Hold::Connections { count: 2 }, Hold::Exec { count: 3 }];
        let ls = render_ls(&snapshot, false);
        assert!(
            ls.contains("held by 2 connections, held by 3 execs"),
            "{ls}"
        );
    }

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
