//! Shares: apps of a stack exposed through the tunnel.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use carte_api::{Credentials, ExternalRequest, ShareStatus, SharedApp};
use carte_core::model::{InstanceKey, ShareConfig, Stack, World};
use carte_core::naming;

pub(super) struct Share {
    pub apps: BTreeSet<String>,
    pub credentials: Option<Credentials>,
    pub pin: bool,
    pub started: Instant,
    pub expires: Option<Instant>,
    pub requests: VecDeque<ExternalRequest>,
}

/// Shares by project and stack.
pub(super) type Shares = BTreeMap<(String, Stack), Share>;

/// What the share listener may serve: each shared instance and the credentials it
/// requires, if any.
pub type SharedRoutes = BTreeMap<InstanceKey, Option<Credentials>>;

pub(super) fn routes(shares: &Shares) -> SharedRoutes {
    shares
        .iter()
        .flat_map(|((project, stack), share)| {
            share.apps.iter().map(move |app| {
                (
                    InstanceKey {
                        project: project.clone(),
                        app: app.clone(),
                        stack: stack.clone(),
                    },
                    share.credentials.clone(),
                )
            })
        })
        .collect()
}

/// The shared app names of one stack, for `public_url:` and `CARTE_PUBLIC_URL`.
pub(super) fn shared_apps(shares: &Shares, project: &str, stack: &Stack) -> BTreeSet<String> {
    shares
        .get(&(project.to_string(), stack.clone()))
        .map(|share| share.apps.clone())
        .unwrap_or_default()
}

pub(super) fn status(
    world: &World,
    config: &ShareConfig,
    (project, stack): &(String, Stack),
    share: &Share,
    now: Instant,
) -> ShareStatus {
    let state = world.project(project);
    let apps = share
        .apps
        .iter()
        .filter_map(|name| {
            let state = state?;
            let app = state.project.app(name)?;
            let url = naming::public_url(config, state, app, stack).ok()?;
            Some(SharedApp {
                app: name.clone(),
                url,
            })
        })
        .collect();
    ShareStatus {
        project: project.clone(),
        stack: stack.slug().to_string(),
        apps,
        auth: share.credentials.is_some(),
        pinned: share.pin,
        shared_secs: now.saturating_duration_since(share.started).as_secs(),
        expires_in_secs: share
            .expires
            .map(|at| at.saturating_duration_since(now).as_secs()),
        requests: share.requests.iter().cloned().collect(),
    }
}

/// Credentials for a share: a fixed user and a random password from the system's
/// random source.
pub(super) fn new_credentials() -> Result<Credentials, String> {
    credentials_from(getrandom::fill)
}

fn credentials_from<E: std::fmt::Display>(
    fill: impl FnOnce(&mut [u8]) -> Result<(), E>,
) -> Result<Credentials, String> {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 16];
    fill(&mut bytes).map_err(|e| format!("couldn't make a password: {e}"))?;
    let password = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    Ok(Credentials {
        user: "carte".into(),
        password,
    })
}

/// Checks an `Authorization: Basic …` header against a share's credentials, in
/// time that doesn't depend on where they differ.
pub fn authorized(header: Option<&str>, credentials: &Credentials) -> bool {
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Basic ")) else {
        return false;
    };
    use base64::Engine;
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let expected = format!("{}:{}", credentials.user, credentials.password);
    let expected = expected.as_bytes();
    decoded.len() == expected.len()
        && decoded
            .iter()
            .zip(expected)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_random_and_readable() {
        let a = new_credentials().unwrap();
        let b = new_credentials().unwrap();
        assert_ne!(a.password, b.password);
        assert_eq!(a.password.len(), 16);
        assert!(a.password.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn credentials_fail_without_randomness() {
        let failed = credentials_from(|_| Err("no entropy")).unwrap_err();
        assert_eq!(failed, "couldn't make a password: no entropy");
    }

    #[test]
    fn checks_basic_auth() {
        let credentials = Credentials {
            user: "carte".into(),
            password: "detail".into(),
        };
        assert!(authorized(Some("Basic Y2FydGU6ZGV0YWls"), &credentials));
        assert!(!authorized(Some("Basic Y2FydGU6d3Jvbmc="), &credentials));
        assert!(!authorized(
            Some("Basic Y2FydGU6ZGV0YWlseA=="),
            &credentials
        ));
        assert!(!authorized(Some("Bearer x"), &credentials));
        assert!(!authorized(None, &credentials));
    }
}
