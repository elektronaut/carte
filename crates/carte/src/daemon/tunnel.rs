//! Keeps the share provider's tunnel running while anything is shared.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use carte_api::{TunnelState, TunnelStatus};
use carte_core::env::{CommandRunner, SystemRunner};
use carte_core::model::ShareConfig;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::process::{IoMode, SpawnSpec, Spawner};
use crate::supervisor::Handle;

/// A tunnel provider: how to run it and how to tell it's connected.
pub trait Provider: Send + Sync + 'static {
    fn program(&self) -> &str;
    fn args(&self, config: &ShareConfig) -> Vec<String>;
    /// Whether a line of the provider's output means the tunnel is up.
    fn connected(&self, line: &str) -> bool;
    fn install_hint(&self) -> &str;
}

/// Cloudflare Tunnel: a named tunnel whose `*.<domain>` record points at it, with
/// every hostname sent to the share listener.
pub struct Cloudflare;

impl Provider for Cloudflare {
    fn program(&self) -> &str {
        "cloudflared"
    }

    fn args(&self, config: &ShareConfig) -> Vec<String> {
        vec![
            "tunnel".into(),
            "--no-autoupdate".into(),
            "--url".into(),
            format!("http://127.0.0.1:{}", config.port),
            "run".into(),
            config.tunnel.clone(),
        ]
    }

    fn connected(&self, line: &str) -> bool {
        line.contains("Registered tunnel connection")
    }

    fn install_hint(&self) -> &str {
        "install it with `brew install cloudflared`, then run `cloudflared tunnel login`"
    }
}

/// ngrok's agent on the account's dev domain. The free plan has one domain and no
/// names under it, so it carries one app at a time; visitors first see ngrok's warning
/// page.
pub struct Ngrok;

impl Provider for Ngrok {
    fn program(&self) -> &str {
        "ngrok"
    }

    fn args(&self, config: &ShareConfig) -> Vec<String> {
        vec![
            "http".into(),
            format!("http://127.0.0.1:{}", config.port),
            "--url".into(),
            format!("https://{}", config.domain),
            "--log".into(),
            "stdout".into(),
            "--log-format".into(),
            "logfmt".into(),
        ]
    }

    fn connected(&self, line: &str) -> bool {
        line.contains("msg=\"started tunnel\"")
    }

    fn install_hint(&self) -> &str {
        "install it with `brew install ngrok`, then run `ngrok config add-authtoken <token>`"
    }
}

pub fn provider(name: &str) -> Option<Arc<dyn Provider>> {
    match name {
        "cloudflare" => Some(Arc::new(Cloudflare)),
        "ngrok" => Some(Arc::new(Ngrok)),
        _ => None,
    }
}

fn shell_quote(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub async fn run(config: ShareConfig, supervisor: Handle, spawner: Arc<Spawner>, cwd: PathBuf) {
    let report = |state: TunnelState, message: Option<String>| {
        supervisor.tunnel_status(TunnelStatus {
            provider: config.provider.clone(),
            state,
            message,
        });
    };
    if config.provider == "none" {
        report(
            TunnelState::Idle,
            Some(format!(
                "carte runs no tunnel; point yours at http://127.0.0.1:{}",
                config.port
            )),
        );
        return;
    }
    let Some(provider) = provider(&config.provider) else {
        report(
            TunnelState::Failed,
            Some(format!(
                "unknown share provider {:?}; carte supports cloudflare and ngrok, or none to run your own tunnel",
                config.provider
            )),
        );
        return;
    };
    let mut shared = supervisor.watch_shared();
    let mut backoff = Duration::from_secs(2);
    loop {
        report(TunnelState::Idle, None);
        while shared.borrow_and_update().is_empty() {
            if shared.changed().await.is_err() {
                return;
            }
        }

        let Some(program) = SystemRunner::from_env().find_program(provider.program()) else {
            report(
                TunnelState::Failed,
                Some(format!(
                    "{} isn't installed: {}",
                    provider.program(),
                    provider.install_hint()
                )),
            );
            if shared.changed().await.is_err() {
                return;
            }
            continue;
        };
        report(TunnelState::Starting, None);
        let command = std::iter::once(program.display().to_string())
            .chain(provider.args(&config))
            .map(|arg| shell_quote(&arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut spec = SpawnSpec::new(command, cwd.clone()).io(IoMode::Pipes);
        spec.env = crate::supervisor::base_env()
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let mut handle = match spawner.spawn(&spec).await {
            Ok(handle) => handle,
            Err(err) => {
                report(TunnelState::Failed, Some(err.to_string()));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        let started = std::time::Instant::now();
        let recent = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(output) = handle.take_output() {
            let recent = recent.clone();
            let supervisor = supervisor.clone();
            let provider = provider.clone();
            let name = config.provider.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(output).lines();
                let mut announced = false;
                while let Ok(Some(line)) = lines.next_line().await {
                    if !announced && provider.connected(&line) {
                        announced = true;
                        supervisor.tunnel_status(TunnelStatus {
                            provider: name.clone(),
                            state: TunnelState::Connected,
                            message: None,
                        });
                    }
                    let mut recent = recent.lock().unwrap();
                    recent.push_back(line);
                    if recent.len() > 20 {
                        recent.pop_front();
                    }
                }
            });
        }

        let unshared = async {
            loop {
                if shared.changed().await.is_err() || shared.borrow().is_empty() {
                    return;
                }
            }
        };
        tokio::select! {
            status = handle.wait() => {
                let last: Vec<String> = recent.lock().unwrap().iter().rev().take(5).rev().cloned().collect();
                let how = match status {
                    Ok(status) => format!("{} exited ({status})", provider.program()),
                    Err(err) => err.to_string(),
                };
                report(TunnelState::Failed, Some(format!("{how}: {}", last.join(" / "))));
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(2);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            _ = unshared => {
                let _ = handle.stop(Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_sends_every_host_to_the_share_listener() {
        let config = ShareConfig {
            provider: "cloudflare".into(),
            domain: "inge.dev".into(),
            tunnel: "carte".into(),
            port: 9254,
        };
        assert_eq!(
            Cloudflare.args(&config),
            [
                "tunnel",
                "--no-autoupdate",
                "--url",
                "http://127.0.0.1:9254",
                "run",
                "carte"
            ]
        );
        assert!(Cloudflare.connected(
            "2026-10-17T10:00:00Z INF Registered tunnel connection connIndex=0 location=osl01"
        ));
        assert!(provider("tailscale").is_none());
    }

    #[test]
    fn ngrok_serves_the_dev_domain() {
        let config = ShareConfig {
            provider: "ngrok".into(),
            domain: "inge.ngrok-free.app".into(),
            tunnel: "carte".into(),
            port: 9254,
        };
        assert_eq!(
            Ngrok.args(&config).join(" "),
            "http http://127.0.0.1:9254 --url https://inge.ngrok-free.app --log stdout --log-format logfmt"
        );
        assert!(Ngrok.connected(
            "t=2026-10-17T10:00:00+0200 lvl=info msg=\"started tunnel\" obj=tunnels name=command_line addr=http://127.0.0.1:9254 url=https://inge.ngrok-free.app"
        ));
        assert!(
            !Ngrok.connected(
                "t=2026-10-17T10:00:00+0200 lvl=info msg=\"client session established\""
            )
        );
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(
            shell_quote("http://127.0.0.1:9254"),
            "http://127.0.0.1:9254"
        );
        assert_eq!(shell_quote("my tunnel"), "'my tunnel'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
