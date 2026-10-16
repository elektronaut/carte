//! `carte setup --share <domain>`: a Cloudflare tunnel for this machine and the
//! `[share]` section that points carte at it.

use std::path::PathBuf;

use anyhow::bail;
use carte_core::env::{CommandRunner, SystemRunner};
use toml_edit::{DocumentMut, Item, Table, value};

use super::plist::LABEL;
use super::{Action, Plan, SetupEnv, Step};

/// What the plan depends on, gathered read-only.
pub struct ShareFacts {
    pub cloudflared: Option<PathBuf>,
    /// `cloudflared tunnel login` has left its certificate.
    pub logged_in: bool,
    pub tunnel_exists: bool,
    pub daemon_loaded: bool,
}

impl ShareFacts {
    pub fn gather(env: &SetupEnv, tunnel: &str) -> Self {
        let cloudflared = SystemRunner::from_env().find_program("cloudflared");
        let logged_in = env.home.join(".cloudflared/cert.pem").is_file();
        let tunnel_exists = match (&cloudflared, logged_in) {
            (Some(program), true) => std::process::Command::new(program)
                .args(["tunnel", "list", "--name", tunnel, "--output", "json"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("\"id\"")),
            _ => false,
        };
        let daemon_loaded = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{}/{LABEL}", env.uid)])
            .output()
            .is_ok_and(|o| o.status.success());
        ShareFacts {
            cloudflared,
            logged_in,
            tunnel_exists,
            daemon_loaded,
        }
    }
}

pub fn plan(
    env: &SetupEnv,
    facts: &ShareFacts,
    domain: &str,
    tunnel: &str,
) -> anyhow::Result<Plan> {
    let domain = domain.trim().trim_start_matches("*.").trim_end_matches('.');
    if domain.is_empty() || !domain.contains('.') {
        bail!(
            "{domain:?} isn't a domain; give the domain whose Cloudflare zone shares live under, like example.dev"
        );
    }
    let Some(program) = &facts.cloudflared else {
        bail!("cloudflared isn't installed: run `brew install cloudflared` first");
    };
    let program = program.display().to_string();
    let mut steps = Vec::new();
    if !facts.logged_in {
        steps.push(Step::run(
            "Log in to Cloudflare and pick the domain's zone (opens your browser)",
            &program,
            &["tunnel", "login"],
        ));
    }
    if !facts.tunnel_exists {
        steps.push(Step::run(
            format!("Create the tunnel {tunnel:?} for this machine"),
            &program,
            &["tunnel", "create", tunnel],
        ));
    }
    let wildcard = format!("*.{domain}");
    steps.push(Step::run(
        format!("Send every {wildcard} name to the tunnel"),
        &program,
        &[
            "tunnel",
            "route",
            "dns",
            "--overwrite-dns",
            tunnel,
            &wildcard,
        ],
    ));
    let config_path = env.paths.machine_config();
    let current = std::fs::read_to_string(&config_path).unwrap_or_default();
    let contents = share_section(&current, domain, tunnel)?;
    if contents != current {
        steps.push(Step {
            description: format!("Tell carte to share under {domain}"),
            action: Action::WriteFile {
                path: config_path,
                contents,
            },
        });
    }
    let mut notes = Vec::new();
    if facts.daemon_loaded {
        steps.push(Step::run(
            "Restart the daemon so it starts the share listener",
            "launchctl",
            &["kickstart", "-k", &format!("gui/{}/{LABEL}", env.uid)],
        ));
    } else {
        notes.push(
            "the daemon isn't running from its LaunchAgent; restart it yourself to start sharing"
                .into(),
        );
    }
    notes.push(format!(
        "after this, `carte share <app>` makes an app public at https://<app>.{domain}"
    ));
    Ok(Plan { steps, notes })
}

/// `config.toml` with its `[share]` section set, everything else kept as written.
pub fn share_section(text: &str, domain: &str, tunnel: &str) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    if !doc.contains_key("share") {
        doc.insert("share", Item::Table(Table::new()));
    }
    let Some(share) = doc["share"].as_table_mut() else {
        bail!("[share] in config.toml isn't a table");
    };
    share["provider"] = value("cloudflare");
    share["domain"] = value(domain);
    share["tunnel"] = value(tunnel);
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::paths::Paths;

    fn env(root: &Path) -> SetupEnv {
        SetupEnv {
            paths: Paths::under(root),
            machine: Default::default(),
            home: root.join("home"),
            uid: 501,
            exe: root.join("carte"),
            resolver_dir: root.join("resolver"),
            launch_agents_dir: root.join("agents"),
            puma_dev_dir: root.join("puma-dev"),
        }
    }

    fn facts(logged_in: bool, tunnel_exists: bool) -> ShareFacts {
        ShareFacts {
            cloudflared: Some(PathBuf::from("/opt/homebrew/bin/cloudflared")),
            logged_in,
            tunnel_exists,
            daemon_loaded: true,
        }
    }

    fn commands(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::Run { command, .. } => Some(command.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_fresh_machine_logs_in_creates_routes_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let plan = plan(&env, &facts(false, false), "*.demo.dev.", "carte").unwrap();
        assert_eq!(
            commands(&plan),
            [
                "/opt/homebrew/bin/cloudflared tunnel login",
                "/opt/homebrew/bin/cloudflared tunnel create carte",
                "/opt/homebrew/bin/cloudflared tunnel route dns --overwrite-dns carte '*.demo.dev'",
                "launchctl kickstart -k gui/501/carte.daemon",
            ]
        );
        let written = plan.steps.iter().find_map(|s| match &s.action {
            Action::WriteFile { contents, .. } => Some(contents.clone()),
            _ => None,
        });
        assert_eq!(
            written.as_deref(),
            Some("[share]\nprovider = \"cloudflare\"\ndomain = \"demo.dev\"\ntunnel = \"carte\"\n")
        );
    }

    #[test]
    fn an_existing_tunnel_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&env(dir.path()), &facts(true, true), "demo.dev", "carte").unwrap();
        assert!(commands(&plan)[0].contains("route dns"));
    }

    #[test]
    fn the_config_keeps_its_comments() {
        let text = "# mine\ntld = \"test\" # keep\n";
        let out = share_section(text, "demo.dev", "carte").unwrap();
        assert!(out.starts_with("# mine\ntld = \"test\" # keep\n"), "{out}");
        assert!(
            out.ends_with(
                "[share]\nprovider = \"cloudflare\"\ndomain = \"demo.dev\"\ntunnel = \"carte\"\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn needs_cloudflared_and_a_domain() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let missing = ShareFacts {
            cloudflared: None,
            ..facts(true, true)
        };
        assert!(
            plan(&env, &missing, "demo.dev", "carte")
                .unwrap_err()
                .to_string()
                .contains("brew install cloudflared")
        );
        assert!(plan(&env, &facts(true, true), "localhost", "carte").is_err());
    }
}
