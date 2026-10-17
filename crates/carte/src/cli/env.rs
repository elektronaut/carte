use std::path::Path;
use std::process::ExitCode;

use anyhow::bail;
use carte_api::EnvResponse;
use carte_core::address::{self, Address};
use carte_core::discovery;
use carte_core::env::{Source, TemplateOrigin};

use super::prompt::tilde;
use super::{EnvArgs, Target};
use crate::exit::{Exit, Failure};
use crate::paths::Paths;
use crate::process_env::{self, DaemonState, Rendered, Subject};

/// What masked values show instead.
const MASK: &str = "••••";

pub fn run(paths: &Paths, mut args: EnvArgs, json: bool) -> anyhow::Result<ExitCode> {
    if args.key.is_none()
        && let Some(typed) = args.target.address.take_if(|typed| looks_like_key(typed))
    {
        args.key = Some(typed);
    }
    let (rendered, daemon) = render(paths, &args.target, args.process.clone())?;

    if let Some(wanted) = &args.key
        && rendered.env.get(wanted).is_none()
    {
        return Err(Failure::new(
            Exit::NotFound,
            format!("{wanted} isn't in the env of {}", rendered.instance),
        )
        .into());
    }
    let mask = !args.values && args.key.is_none();
    let listed = |key: &str| args.key.as_deref().is_none_or(|wanted| wanted == key);

    if json {
        let response = EnvResponse {
            instance: rendered.instance.clone(),
            vars: rendered
                .env
                .iter()
                .filter(|(key, _)| listed(key))
                .filter_map(|(key, _)| rendered.var(key, mask))
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(ExitCode::SUCCESS);
    }

    let checkout = &rendered.checkout;
    println!(
        "{}  {}  (env from {}{})",
        rendered.instance,
        tilde(checkout),
        rendered.provider,
        if daemon {
            ", as the daemon builds it"
        } else {
            "; the daemon isn't running, so this shell's environment stands in for it"
        }
    );
    println!();
    let env = &rendered.env;
    let label = |source: &Source| source_label(source, checkout);
    let shown = |key: &str, value: &str, source: &Source| {
        (!mask || process_env::revealed(source)).then(|| rendered.shown(key, value, source))
    };
    let key_width = env.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let source_width = env
        .iter()
        .map(|(_, v)| label(&v.source).len())
        .max()
        .unwrap_or(0);
    let mut masked = false;
    for (key, v) in env.iter().filter(|(key, _)| listed(key)) {
        let value = shown(key, &v.value, &v.source);
        masked |= value.is_none();
        println!(
            "{key:key_width$}  {:source_width$}  {}",
            label(&v.source),
            value.as_deref().unwrap_or(MASK)
        );
        for old in v.overrode.iter().rev() {
            let value = shown(key, &old.value, &old.source);
            masked |= value.is_none();
            println!(
                "{:key_width$}  ↳ overrode {}: {}",
                "",
                label(&old.source),
                value.as_deref().unwrap_or(MASK)
            );
        }
    }
    if masked {
        println!();
        println!(
            "Values from the environment and env files are hidden; `carte env <KEY>` or `--values` shows them."
        );
    }
    if !rendered.notices.is_empty() {
        println!();
    }
    for notice in &rendered.notices {
        println!("note: {notice}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The env of the process an address names, and whether the daemon's view was used.
pub(crate) fn render(
    paths: &Paths,
    target: &Target,
    process: Option<String>,
) -> anyhow::Result<(Rendered, bool)> {
    render_for(paths, target, process, false)
}

/// Like [`render`]; with `command`, the env `carte exec` runs a command with.
pub(crate) fn render_for(
    paths: &Paths,
    target: &Target,
    process: Option<String>,
    command: bool,
) -> anyhow::Result<(Rendered, bool)> {
    let world = discovery::load_world(&paths.config_dir);
    let typed = target.address.as_deref().unwrap_or("");
    let mut address: Address = typed.parse().map_err(Failure::from)?;
    if process.is_some() {
        address.process = process;
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let resolved = match &target.project {
        Some(project) => address::resolve_in_project(&world, project, &address, Some(&cwd)),
        None => address::resolve(&world, &address, Some(&cwd)),
    }
    .map_err(Failure::from)?;
    let Some(key) = resolved.instance() else {
        bail!("{typed:?} names a whole project or stack; give an app, as in `carte env <app>`");
    };
    let daemon = daemon_state(paths, &key);
    let process = resolved.process.as_deref();
    let rendered = process_env::render_subject(
        paths,
        &world,
        &key,
        if command {
            Subject::Command(process)
        } else {
            Subject::Process(process)
        },
        daemon.as_ref().unwrap_or(&DaemonState {
            env: crate::supervisor::base_env(),
            ports: Default::default(),
            shared: Default::default(),
            running: Default::default(),
        }),
    )?;
    Ok((rendered, daemon.is_some()))
}

/// The daemon's base env, ports and shares, when the daemon runs.
fn daemon_state(paths: &Paths, key: &carte_core::model::InstanceKey) -> Option<DaemonState> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let client = crate::client::Client::new(&paths.socket());
    runtime.block_on(async {
        let env: carte_api::DaemonEnv = client.get(carte_api::paths::ENV).await.ok()?;
        let snapshot: carte_api::Snapshot = client.get(carte_api::paths::SNAPSHOT).await.ok()?;
        Some(DaemonState::new(env.vars, &snapshot, key))
    })
}

/// An environment variable name rather than an address: addresses are lowercase.
fn looks_like_key(typed: &str) -> bool {
    typed.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && typed
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A short name for a source; the JSON output has the full one.
fn source_label(source: &Source, checkout: &Path) -> String {
    match source {
        Source::EnvFile { path, line } => {
            let path = path.strip_prefix(checkout).unwrap_or(path);
            format!("{}:{line}", path.display())
        }
        Source::Template { origin, .. } => match origin {
            TemplateOrigin::Project { .. } => "project template".into(),
            TemplateOrigin::App { .. } => "app template".into(),
            TemplateOrigin::Process { .. } => "process template".into(),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_keys_from_addresses() {
        for key in ["DATABASE_URL", "PORT", "_X", "S3_BUCKET"] {
            assert!(looks_like_key(key), "{key}");
        }
        for address in [
            "web",
            "web@feature-x",
            "@feature-x",
            "web/worker",
            "Web",
            "",
        ] {
            assert!(!looks_like_key(address), "{address}");
        }
    }
}
