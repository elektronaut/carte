//! Desktop notifications when a process crashes, carte gives up on it, or it stops
//! at a debugger prompt.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use carte_api::{Event, EventKind, EventSubject};
use carte_core::model::{InstanceKey, Stack, World};
use carte_core::naming;
use tokio::process::Command;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::clock::now_ms;
use crate::supervisor::Handle;

/// After a notification, how long further crashes of the instance only count
/// towards one summary.
const QUIET_MS: u64 = 5 * 60 * 1000;

/// Something to tell about one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: InstanceKey,
    pub subtitle: String,
    pub message: String,
}

struct Quiet {
    until_ms: u64,
    crashes: u32,
    latest: String,
}

/// After a pause notification, how long further pauses of the instance stay quiet.
const PAUSE_QUIET_MS: u64 = 60 * 1000;

/// Decides which crashes to tell about: the first one right away, later ones within
/// the quiet period as one summary when it ends, and every give-up. A pause is told
/// about unless the same process paused within the last minute.
#[derive(Default)]
pub struct Throttle {
    quiet: BTreeMap<InstanceKey, Quiet>,
    paused: BTreeMap<(InstanceKey, String), u64>,
}

impl Throttle {
    /// What to tell after `event`, or after a quiet period ends when there is none.
    /// While notifications are disabled it drops everything it was holding, so it
    /// starts fresh when they're enabled again.
    pub fn step(&mut self, event: Option<&Event>, enabled: bool, now_ms: u64) -> Vec<Notice> {
        if !enabled {
            self.quiet.clear();
            return Vec::new();
        }
        match event {
            Some(event) => self.event(event, now_ms),
            None => self.due(now_ms),
        }
    }

    pub fn event(&mut self, event: &Event, now_ms: u64) -> Vec<Notice> {
        let mut notices = self.due(now_ms);
        let Some(subject) = event.subjects.first() else {
            return notices;
        };
        let (Some(key), Some(process)) = (instance_key(subject), subject.process.as_deref()) else {
            return notices;
        };
        let label = instance_label(subject);
        match event.kind {
            EventKind::Crashed => {
                if let Some(quiet) = self.quiet.get_mut(&key) {
                    quiet.crashes += 1;
                    quiet.latest = event.message.clone();
                } else {
                    self.quiet.insert(
                        key.clone(),
                        Quiet {
                            until_ms: now_ms + QUIET_MS,
                            crashes: 0,
                            latest: String::new(),
                        },
                    );
                    notices.push(Notice {
                        key,
                        subtitle: format!("{label} crashed"),
                        message: event.message.clone(),
                    });
                }
            }
            EventKind::Paused => {
                self.paused
                    .retain(|_, at| now_ms.saturating_sub(*at) < PAUSE_QUIET_MS);
                if let Entry::Vacant(entry) = self.paused.entry((key.clone(), process.to_string()))
                {
                    entry.insert(now_ms);
                    notices.push(Notice {
                        subtitle: format!("{label} paused in {process}"),
                        message: format!("carte attach {label}/{process}"),
                        key,
                    });
                }
            }
            EventKind::GaveUp => {
                self.quiet.remove(&key);
                notices.push(Notice {
                    key,
                    subtitle: format!("{label}: gave up restarting {process}"),
                    message: event.message.clone(),
                });
            }
            _ => {}
        }
        notices
    }

    /// Summaries of the quiet periods that have ended.
    pub fn due(&mut self, now_ms: u64) -> Vec<Notice> {
        let ended: Vec<InstanceKey> = self
            .quiet
            .iter()
            .filter(|(_, quiet)| quiet.until_ms <= now_ms)
            .map(|(key, _)| key.clone())
            .collect();
        ended
            .into_iter()
            .filter_map(|key| {
                let quiet = self.quiet.remove(&key)?;
                (quiet.crashes > 0).then(|| {
                    let label = instance_label(&subject(&key));
                    let times = match quiet.crashes {
                        1 => "once more".to_string(),
                        n => format!("{n} more times"),
                    };
                    Notice {
                        key,
                        subtitle: format!("{label} crashed {times}"),
                        message: quiet.latest,
                    }
                })
            })
            .collect()
    }

    /// When the next quiet period ends.
    pub fn next_due(&self) -> Option<u64> {
        self.quiet.values().map(|quiet| quiet.until_ms).min()
    }
}

fn instance_key(subject: &EventSubject) -> Option<InstanceKey> {
    let stack = match subject.stack.as_deref()? {
        "main" => Stack::Main,
        slug => Stack::Branch(slug.to_string()),
    };
    Some(InstanceKey {
        project: subject.project.clone(),
        app: subject.app.clone()?,
        stack,
    })
}

fn subject(key: &InstanceKey) -> EventSubject {
    EventSubject {
        project: key.project.clone(),
        stack: Some(key.stack.slug().to_string()),
        app: Some(key.app.clone()),
        process: None,
    }
}

fn instance_label(subject: &EventSubject) -> String {
    EventSubject {
        process: None,
        ..subject.clone()
    }
    .label()
}

/// A notification as the desktop shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub subtitle: String,
    pub message: String,
    /// Notification Center keeps one entry per group.
    pub group: String,
    /// What a click opens.
    pub url: Option<String>,
}

impl Notification {
    pub fn new(notice: Notice, world: &World) -> Self {
        let key = &notice.key;
        Notification {
            title: "carte".into(),
            subtitle: notice.subtitle,
            message: notice.message,
            group: format!("carte.{}.{}.{}", key.project, key.stack.slug(), key.app),
            url: url(world, key),
        }
    }
}

fn url(world: &World, key: &InstanceKey) -> Option<String> {
    let state = world.project(&key.project)?;
    let app = state.project.app(&key.app)?;
    Some(naming::instance_url(
        &world.machine,
        &state.project,
        app,
        &key.stack,
    ))
}

pub fn terminal_notifier_args(n: &Notification) -> Vec<String> {
    let mut args = vec![
        "-title".into(),
        n.title.clone(),
        "-subtitle".into(),
        n.subtitle.clone(),
        "-message".into(),
        n.message.clone(),
        "-group".into(),
        n.group.clone(),
    ];
    if let Some(url) = &n.url {
        args.extend(["-open".into(), url.clone()]);
    }
    args
}

pub fn osascript_args(n: &Notification) -> Vec<String> {
    vec![
        "-e".into(),
        "on run argv".into(),
        "-e".into(),
        "display notification (item 2 of argv) with title (item 1 of argv) subtitle (item 3 of argv)"
            .into(),
        "-e".into(),
        "end run".into(),
        n.title.clone(),
        n.message.clone(),
        n.subtitle.clone(),
    ]
}

pub fn notify_send_args(n: &Notification) -> Vec<String> {
    vec![
        "--app-name".into(),
        "carte".into(),
        "--".into(),
        format!("{}: {}", n.title, n.subtitle),
        n.message.clone(),
    ]
}

/// The program and arguments that show `n` on macOS: terminal-notifier when it's
/// installed, for the click action, otherwise AppleScript.
pub fn macos_command(
    n: &Notification,
    terminal_notifier: Option<PathBuf>,
) -> (PathBuf, Vec<String>) {
    match terminal_notifier {
        Some(program) => (program, terminal_notifier_args(n)),
        None => ("/usr/bin/osascript".into(), osascript_args(n)),
    }
}

/// The program and arguments that show `n` on Linux; none without a display.
pub fn linux_command(n: &Notification, display: bool) -> Option<(PathBuf, Vec<String>)> {
    display.then(|| ("notify-send".into(), notify_send_args(n)))
}

/// The first executable `name` on `PATH` or in Homebrew's directories.
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn command(n: &Notification) -> Option<(PathBuf, Vec<String>)> {
    if cfg!(target_os = "macos") {
        Some(macos_command(n, find_program("terminal-notifier")))
    } else {
        let display = ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()));
        linux_command(n, display)
    }
}

async fn deliver(n: Notification) {
    let Some((program, args)) = command(&n) else {
        debug!("no display for a notification: {}", n.subtitle);
        return;
    };
    let child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            debug!("couldn't run {}: {err}", program.display());
            return;
        }
    };
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => warn!("{} failed with {status}", program.display()),
        Ok(Err(err)) => warn!("{} failed: {err}", program.display()),
        Err(_) => warn!("{} didn't finish in time", program.display()),
    }
}

/// Follows the supervisor's events and notifies, while the machine config allows it.
pub async fn run(supervisor: Handle) {
    let mut events = supervisor.follow_events();
    let mut throttle = Throttle::default();
    loop {
        let wait = throttle.next_due().map(|at| at.saturating_sub(now_ms()));
        let event = tokio::select! {
            received = events.recv() => match received {
                Ok(event) => Some(event),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            () = sleep_ms(wait) => None,
        };
        let world = supervisor.world();
        let notices = throttle.step(event.as_ref(), world.machine.notifications, now_ms());
        for notice in notices {
            tokio::spawn(deliver(Notification::new(notice, &world)));
        }
    }
}

async fn sleep_ms(ms: Option<u64>) {
    match ms {
        Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, stack: &str, process: Option<&str>, message: &str) -> Event {
        Event {
            seq: 0,
            at_ms: 0,
            kind,
            message: message.into(),
            subjects: vec![EventSubject {
                project: "acme".into(),
                stack: Some(stack.into()),
                app: Some("acme-web".into()),
                process: process.map(str::to_string),
            }],
            boot_ms: None,
            exit_code: Some(1),
        }
    }

    fn crash(stack: &str, message: &str) -> Event {
        event(EventKind::Crashed, stack, Some("worker"), message)
    }

    fn subtitles(notices: &[Notice]) -> Vec<&str> {
        notices.iter().map(|n| n.subtitle.as_str()).collect()
    }

    #[test]
    fn tells_about_the_first_crash_and_sums_up_the_rest() {
        let mut throttle = Throttle::default();
        let first = throttle.event(&crash("feature-x", "worker exited with code 1"), 1_000);
        assert_eq!(subtitles(&first), ["acme-web@feature-x crashed"]);
        assert_eq!(first[0].message, "worker exited with code 1");

        assert!(
            throttle
                .event(&crash("feature-x", "worker exited with code 2"), 60_000)
                .is_empty()
        );
        assert!(
            throttle
                .event(&crash("feature-x", "worker exited with code 3"), 120_000)
                .is_empty()
        );
        assert_eq!(throttle.next_due(), Some(1_000 + QUIET_MS));
        assert!(throttle.due(QUIET_MS).is_empty());

        let summary = throttle.due(1_000 + QUIET_MS);
        assert_eq!(
            subtitles(&summary),
            ["acme-web@feature-x crashed 2 more times"]
        );
        assert_eq!(summary[0].message, "worker exited with code 3");
        assert_eq!(throttle.next_due(), None);

        let again = throttle.event(
            &crash("feature-x", "worker exited with code 1"),
            2 * QUIET_MS,
        );
        assert_eq!(subtitles(&again), ["acme-web@feature-x crashed"]);
    }

    #[test]
    fn a_quiet_period_without_crashes_ends_silently() {
        let mut throttle = Throttle::default();
        throttle.event(&crash("main", "worker was killed by signal 9"), 0);
        assert!(throttle.due(QUIET_MS).is_empty());
        assert_eq!(throttle.next_due(), None);
        assert_eq!(
            subtitles(&throttle.event(&crash("main", "again"), QUIET_MS + 1)),
            ["acme-web crashed"]
        );
    }

    #[test]
    fn a_crash_after_the_quiet_period_sums_up_first() {
        let mut throttle = Throttle::default();
        throttle.event(&crash("main", "one"), 0);
        throttle.event(&crash("main", "two"), 1_000);
        let notices = throttle.event(&crash("main", "three"), QUIET_MS + 1);
        assert_eq!(
            subtitles(&notices),
            ["acme-web crashed once more", "acme-web crashed"]
        );
        assert_eq!(notices[0].message, "two");
    }

    #[test]
    fn throttles_each_instance_on_its_own() {
        let mut throttle = Throttle::default();
        throttle.event(&crash("main", "one"), 0);
        assert_eq!(
            subtitles(&throttle.event(&crash("feature-x", "two"), 1_000)),
            ["acme-web@feature-x crashed"]
        );
    }

    #[test]
    fn a_give_up_always_tells_and_drops_the_summary() {
        let mut throttle = Throttle::default();
        throttle.event(&crash("feature-x", "one"), 0);
        throttle.event(&crash("feature-x", "two"), 1_000);
        let message = "worker crashed 5 times within a minute; it stays down";
        let notices = throttle.event(
            &event(EventKind::GaveUp, "feature-x", Some("worker"), message),
            2_000,
        );
        assert_eq!(
            subtitles(&notices),
            ["acme-web@feature-x: gave up restarting worker"]
        );
        assert_eq!(notices[0].message, message);
        assert_eq!(throttle.next_due(), None);
        assert!(throttle.due(QUIET_MS * 2).is_empty());
    }

    #[test]
    fn tells_about_a_pause_with_the_command_that_answers_it() {
        let mut throttle = Throttle::default();
        let pause =
            |stack: &str, process| event(EventKind::Paused, stack, Some(process), "at (rdbg)");
        let notices = throttle.event(&pause("feature-x", "worker"), 0);
        assert_eq!(subtitles(&notices), ["acme-web@feature-x paused in worker"]);
        assert_eq!(notices[0].message, "carte attach acme-web@feature-x/worker");
        assert!(
            throttle
                .event(&pause("feature-x", "worker"), 20_000)
                .is_empty(),
            "stepping through code doesn't notify at each step"
        );
        assert_eq!(
            subtitles(&throttle.event(&pause("feature-x", "web"), 30_000)),
            ["acme-web@feature-x paused in web"],
            "another process does"
        );
        assert_eq!(
            subtitles(&throttle.event(&pause("main", "web"), 30_000)),
            ["acme-web paused in web"]
        );
        assert_eq!(
            subtitles(&throttle.event(&pause("feature-x", "worker"), 60_000)),
            ["acme-web@feature-x paused in worker"]
        );
        assert_eq!(throttle.next_due(), None);
    }

    #[test]
    fn ignores_failed_starts_clean_exits_and_stops() {
        let mut throttle = Throttle::default();
        for event in [
            event(EventKind::Crashed, "main", None, "web exited with code 1"),
            event(EventKind::Exited, "main", Some("worker"), "worker exited"),
            event(EventKind::Stopped, "main", None, "idle for 15m"),
        ] {
            assert!(throttle.event(&event, 0).is_empty());
        }
        assert_eq!(throttle.next_due(), None);
    }

    #[test]
    fn keeps_nothing_while_disabled_and_starts_fresh() {
        let mut throttle = Throttle::default();
        throttle.step(Some(&crash("main", "one")), true, 0);
        assert!(
            throttle
                .step(Some(&crash("main", "two")), false, 1_000)
                .is_empty()
        );
        assert_eq!(throttle.next_due(), None);
        assert!(
            throttle
                .step(Some(&crash("main", "three")), false, 2_000)
                .is_empty()
        );
        assert_eq!(throttle.next_due(), None);

        let notices = throttle.step(Some(&crash("main", "four")), true, 3_000);
        assert_eq!(subtitles(&notices), ["acme-web crashed"]);
        assert_eq!(notices[0].message, "four");
        assert!(throttle.step(None, true, 3_000 + QUIET_MS).is_empty());
    }

    #[test]
    fn a_quiet_period_ending_while_disabled_tells_nothing() {
        let mut throttle = Throttle::default();
        throttle.step(Some(&crash("main", "one")), true, 0);
        throttle.step(Some(&crash("main", "two")), true, 1_000);
        assert!(throttle.step(None, false, QUIET_MS).is_empty());
        assert_eq!(throttle.next_due(), None);
    }

    fn notification(url: Option<&str>) -> Notification {
        Notification {
            title: "carte".into(),
            subtitle: "acme-web@feature-x crashed".into(),
            message: "worker exited with code 1".into(),
            group: "carte.acme.feature-x.acme-web".into(),
            url: url.map(str::to_string),
        }
    }

    #[test]
    fn uses_terminal_notifier_with_a_click_to_open() {
        let (program, args) = macos_command(
            &notification(Some("https://web.acme.test")),
            Some("/opt/homebrew/bin/terminal-notifier".into()),
        );
        assert_eq!(program, Path::new("/opt/homebrew/bin/terminal-notifier"));
        assert_eq!(
            args,
            [
                "-title",
                "carte",
                "-subtitle",
                "acme-web@feature-x crashed",
                "-message",
                "worker exited with code 1",
                "-group",
                "carte.acme.feature-x.acme-web",
                "-open",
                "https://web.acme.test",
            ]
        );
        let (_, args) = macos_command(&notification(None), Some("terminal-notifier".into()));
        assert!(!args.contains(&"-open".to_string()));
    }

    #[test]
    fn falls_back_to_applescript_with_the_text_as_arguments() {
        let mut n = notification(None);
        n.message = "worker said \"bye\" & left".into();
        let (program, args) = macos_command(&n, None);
        assert_eq!(program, Path::new("/usr/bin/osascript"));
        assert_eq!(
            args,
            [
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv) subtitle (item 3 of argv)",
                "-e",
                "end run",
                "carte",
                "worker said \"bye\" & left",
                "acme-web@feature-x crashed",
            ]
        );
    }

    #[test]
    fn uses_notify_send_only_with_a_display() {
        let n = notification(Some("https://web.acme.test"));
        assert_eq!(linux_command(&n, false), None);
        let (program, args) = linux_command(&n, true).unwrap();
        assert_eq!(program, Path::new("notify-send"));
        assert_eq!(
            args,
            [
                "--app-name",
                "carte",
                "--",
                "carte: acme-web@feature-x crashed",
                "worker exited with code 1",
            ]
        );
    }

    #[test]
    fn has_no_click_action_for_an_unknown_app() {
        let notice = Notice {
            key: InstanceKey {
                project: "acme".into(),
                app: "acme-web".into(),
                stack: Stack::Branch("feature-x".into()),
            },
            subtitle: "acme-web@feature-x crashed".into(),
            message: "worker exited with code 1".into(),
        };
        let n = Notification::new(notice, &World::default());
        assert_eq!(n, notification(None));
    }
}
