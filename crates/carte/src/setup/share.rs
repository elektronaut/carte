//! `carte setup --share <domain>`: a Cloudflare tunnel for this machine, or the
//! account's ngrok dev domain, and the `[share]` section that points carte at it.

use std::path::PathBuf;

use anyhow::bail;
use carte_core::env::{CommandRunner, SystemRunner};
use toml_edit::{DocumentMut, Item, Table, value};

use super::plist::LABEL;
use super::{Action, Plan, SetupEnv, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Cloudflare,
    Ngrok,
}

impl Provider {
    /// ngrok's own domains mean ngrok; any other domain is a Cloudflare zone.
    pub fn for_domain(domain: &str) -> Self {
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let ngrok = [
            "ngrok-free.app",
            "ngrok-free.dev",
            "ngrok.app",
            "ngrok.dev",
            "ngrok.io",
        ];
        if ngrok
            .iter()
            .any(|suffix| domain.ends_with(&format!(".{suffix}")))
        {
            Provider::Ngrok
        } else {
            Provider::Cloudflare
        }
    }

    fn program(self) -> &'static str {
        match self {
            Provider::Cloudflare => "cloudflared",
            Provider::Ngrok => "ngrok",
        }
    }
}

/// What the plan depends on, gathered read-only.
pub struct ShareFacts {
    pub provider: Provider,
    pub program: Option<PathBuf>,
    /// Cloudflare: `cloudflared tunnel login` has left its certificate. ngrok: the
    /// agent's config has an authtoken.
    pub logged_in: bool,
    pub tunnel_exists: bool,
    pub daemon_loaded: bool,
}

impl ShareFacts {
    pub fn gather(env: &SetupEnv, provider: Provider, tunnel: &str) -> Self {
        let program = SystemRunner::from_env().find_program(provider.program());
        let logged_in = match provider {
            Provider::Cloudflare => env.home.join(".cloudflared/cert.pem").is_file(),
            Provider::Ngrok => [
                "Library/Application Support/ngrok/ngrok.yml",
                ".config/ngrok/ngrok.yml",
                ".ngrok2/ngrok.yml",
            ]
            .iter()
            .filter_map(|path| std::fs::read_to_string(env.home.join(path)).ok())
            .any(|text| {
                text.lines()
                    .any(|l| l.trim_start().starts_with("authtoken:"))
            }),
        };
        let tunnel_exists = match (provider, &program, logged_in) {
            (Provider::Cloudflare, Some(program), true) => std::process::Command::new(program)
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
            provider,
            program,
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
            "{domain:?} isn't a domain; give the domain whose Cloudflare zone shares live under, like example.dev, or your ngrok dev domain"
        );
    }
    let Some(program) = &facts.program else {
        let name = facts.provider.program();
        bail!("{name} isn't installed: run `brew install {name}` first");
    };
    let program = program.display().to_string();
    let mut steps = Vec::new();
    if facts.provider == Provider::Ngrok {
        if !facts.logged_in {
            bail!(
                "ngrok needs your authtoken first: run `ngrok config add-authtoken <token>` with the token from https://dashboard.ngrok.com/get-started/your-authtoken"
            );
        }
    } else {
        cloudflare_steps(&mut steps, facts, &program, domain, tunnel);
    }
    let config_path = env.paths.machine_config();
    let current = std::fs::read_to_string(&config_path).unwrap_or_default();
    let contents = share_section(&current, facts.provider, domain, tunnel)?;
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
    notes.push(match facts.provider {
        Provider::Cloudflare => format!(
            "after this, `carte share <app>` makes an app public at https://<app>.{domain}"
        ),
        Provider::Ngrok => format!(
            "after this, `carte share <app>` makes an app public at https://{domain}: ngrok's free plan carries one app at a time, and visitors first see ngrok's warning page"
        ),
    });
    Ok(Plan { steps, notes })
}

fn cloudflare_steps(
    steps: &mut Vec<Step>,
    facts: &ShareFacts,
    program: &str,
    domain: &str,
    tunnel: &str,
) {
    if !facts.logged_in {
        steps.push(Step::run(
            "Log in to Cloudflare and pick the domain's zone (opens your browser)",
            program,
            &["tunnel", "login"],
        ));
    }
    if !facts.tunnel_exists {
        steps.push(Step::run(
            format!("Create the tunnel {tunnel:?} for this machine"),
            program,
            &["tunnel", "create", tunnel],
        ));
    }
    let wildcard = format!("*.{domain}");
    steps.push(Step::run(
        format!("Send every {wildcard} name to the tunnel"),
        program,
        &[
            "tunnel",
            "route",
            "dns",
            "--overwrite-dns",
            tunnel,
            &wildcard,
        ],
    ));
}

/// `config.toml` with its `[share]` section set, everything else kept as written.
pub fn share_section(
    text: &str,
    provider: Provider,
    domain: &str,
    tunnel: &str,
) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    if !doc.contains_key("share") {
        doc.insert("share", Item::Table(Table::new()));
    }
    let Some(share) = doc["share"].as_table_mut() else {
        bail!("[share] in config.toml isn't a table");
    };
    share["provider"] = value(match provider {
        Provider::Cloudflare => "cloudflare",
        Provider::Ngrok => "ngrok",
    });
    share["domain"] = value(domain);
    match provider {
        Provider::Cloudflare => share["tunnel"] = value(tunnel),
        Provider::Ngrok => {
            share.remove("tunnel");
        }
    }
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
            provider: Provider::Cloudflare,
            program: Some(PathBuf::from("/opt/homebrew/bin/cloudflared")),
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
                "launchctl kickstart -k gui/501/no.elektronaut.carte",
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
        let out = share_section(text, Provider::Cloudflare, "demo.dev", "carte").unwrap();
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
            program: None,
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

    #[test]
    fn ngrok_domains_mean_ngrok() {
        assert_eq!(
            Provider::for_domain("alex-dev.ngrok-free.app"),
            Provider::Ngrok
        );
        assert_eq!(Provider::for_domain("x.NGROK.dev."), Provider::Ngrok);
        assert_eq!(Provider::for_domain("ngrok.app"), Provider::Cloudflare);
        assert_eq!(Provider::for_domain("demo.dev"), Provider::Cloudflare);
    }

    #[test]
    fn ngrok_needs_only_its_authtoken_and_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        std::fs::create_dir_all(env.paths.machine_config().parent().unwrap()).unwrap();
        std::fs::write(
            env.paths.machine_config(),
            "[share]\nprovider = \"cloudflare\"\ndomain = \"demo.dev\"\ntunnel = \"carte\"\n",
        )
        .unwrap();
        let ngrok = |logged_in| ShareFacts {
            provider: Provider::Ngrok,
            program: Some(PathBuf::from("/opt/homebrew/bin/ngrok")),
            logged_in,
            tunnel_exists: false,
            daemon_loaded: true,
        };
        assert!(
            plan(&env, &ngrok(false), "alex.ngrok-free.app", "carte")
                .unwrap_err()
                .to_string()
                .contains("ngrok config add-authtoken")
        );
        let plan = plan(&env, &ngrok(true), "alex.ngrok-free.app", "carte").unwrap();
        assert_eq!(
            commands(&plan),
            ["launchctl kickstart -k gui/501/no.elektronaut.carte"]
        );
        let written = plan.steps.iter().find_map(|s| match &s.action {
            Action::WriteFile { contents, .. } => Some(contents.clone()),
            _ => None,
        });
        assert_eq!(
            written.as_deref(),
            Some("[share]\nprovider = \"ngrok\"\ndomain = \"alex.ngrok-free.app\"\n")
        );
    }
}
