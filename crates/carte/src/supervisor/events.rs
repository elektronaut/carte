//! The event log: what the daemon did and why, kept in memory for a while.

use std::collections::{BTreeMap, VecDeque};

use carte_api::{Event, EventKind, EventSubject};
use carte_core::model::{InstanceKey, Stack};
use tokio::sync::broadcast;

/// Events kept for clients that ask for the past.
const KEEP: usize = 1000;

pub(super) struct EventLog {
    events: VecDeque<Event>,
    next_seq: u64,
    tx: broadcast::Sender<Event>,
    /// The latest event about each instance or one of its processes.
    latest: BTreeMap<InstanceKey, Event>,
}

impl EventLog {
    pub fn new(tx: broadcast::Sender<Event>) -> Self {
        EventLog {
            events: VecDeque::new(),
            next_seq: 0,
            tx,
            latest: BTreeMap::new(),
        }
    }

    pub fn push(
        &mut self,
        kind: EventKind,
        message: impl Into<String>,
        subjects: Vec<EventSubject>,
    ) {
        self.push_event(kind, message.into(), subjects, |_| {});
    }

    pub fn push_event(
        &mut self,
        kind: EventKind,
        message: String,
        subjects: Vec<EventSubject>,
        extra: impl FnOnce(&mut Event),
    ) {
        self.next_seq += 1;
        let mut event = Event {
            seq: self.next_seq,
            at_ms: super::runner::now_ms(),
            kind,
            message,
            subjects,
            boot_ms: None,
            exit_code: None,
        };
        extra(&mut event);
        for subject in &event.subjects {
            if let Some(key) = instance_key(subject) {
                self.latest.insert(key, event.clone());
            }
        }
        if self.events.len() == KEEP {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        let _ = self.tx.send(event);
    }

    /// The last `tail` events the filter matches, oldest first.
    pub fn recent(&self, filter: &EventFilter, tail: usize) -> Vec<Event> {
        let mut events: Vec<Event> = self
            .events
            .iter()
            .rev()
            .filter(|e| filter.matches(e))
            .take(tail)
            .cloned()
            .collect();
        events.reverse();
        events
    }

    pub fn latest(&self) -> &BTreeMap<InstanceKey, Event> {
        &self.latest
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

pub(super) fn instance(key: &InstanceKey) -> EventSubject {
    EventSubject {
        project: key.project.clone(),
        stack: Some(key.stack.slug().to_string()),
        app: Some(key.app.clone()),
        process: None,
    }
}

pub(super) fn process(key: &InstanceKey, name: &str) -> EventSubject {
    EventSubject {
        process: Some(name.to_string()),
        ..instance(key)
    }
}

pub(super) fn project(name: &str) -> EventSubject {
    EventSubject {
        project: name.to_string(),
        stack: None,
        app: None,
        process: None,
    }
}

pub(super) fn stack(project: &str, stack: &str) -> EventSubject {
    EventSubject {
        stack: Some(stack.to_string()),
        ..self::project(project)
    }
}

/// Which events a client wants: those about a project, stack, app or process and
/// what's under it. An empty filter wants everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub project: Option<String>,
    /// The stack's slug.
    pub stack: Option<String>,
    pub app: Option<String>,
    pub process: Option<String>,
}

impl EventFilter {
    pub fn matches(&self, event: &Event) -> bool {
        let Some(project) = &self.project else {
            return true;
        };
        event.subjects.iter().any(|s| {
            s.project == *project
                && self
                    .stack
                    .as_ref()
                    .is_none_or(|stack| s.stack.as_ref() == Some(stack))
                && self
                    .app
                    .as_ref()
                    .is_none_or(|app| s.app.as_ref() == Some(app))
                && self
                    .process
                    .as_ref()
                    .is_none_or(|p| s.process.as_ref().is_none_or(|sp| sp == p))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app: &str, stack: Stack) -> InstanceKey {
        InstanceKey {
            project: "acme".into(),
            app: app.into(),
            stack,
        }
    }

    fn filter(stack: Option<&str>, app: Option<&str>, process: Option<&str>) -> EventFilter {
        EventFilter {
            project: Some("acme".into()),
            stack: stack.map(Into::into),
            app: app.map(Into::into),
            process: process.map(Into::into),
        }
    }

    fn kinds(log: &EventLog, filter: &EventFilter) -> Vec<String> {
        log.recent(filter, 100)
            .iter()
            .map(|e| format!("{} {}", e.subject_label(), e.kind.words()))
            .collect()
    }

    #[test]
    fn filters_by_scope() {
        let (tx, _) = broadcast::channel(16);
        let mut log = EventLog::new(tx);
        let web = key("acme-web", Stack::Main);
        let api = key("acme-api", Stack::Branch("x".into()));
        log.push(EventKind::ConfigReloaded, "", vec![]);
        log.push(EventKind::ConfigError, "bad", vec![project("acme")]);
        log.push(EventKind::Started, "", vec![instance(&web)]);
        log.push(EventKind::Crashed, "", vec![process(&web, "worker")]);
        log.push(EventKind::Stopped, "", vec![instance(&api), instance(&web)]);

        assert_eq!(kinds(&log, &EventFilter::default()).len(), 5);
        assert_eq!(
            kinds(&log, &filter(None, None, None)),
            [
                "acme config error",
                "acme-web started",
                "acme-web/worker crashed",
                "acme-api@x stopped"
            ]
        );
        assert_eq!(
            kinds(&log, &filter(Some("x"), None, None)),
            ["acme-api@x stopped"]
        );
        assert_eq!(
            kinds(&log, &filter(Some("main"), Some("acme-web"), Some("web"))),
            ["acme-web started", "acme-api@x stopped"],
            "an instance's events count for its processes; another process's don't"
        );
        assert_eq!(
            log.latest()[&web].kind,
            EventKind::Stopped,
            "an event about two instances is the latest for both"
        );
    }

    #[test]
    fn keeps_the_most_recent() {
        let (tx, _) = broadcast::channel(16);
        let mut log = EventLog::new(tx);
        for _ in 0..KEEP + 5 {
            log.push(EventKind::ConfigReloaded, "", vec![]);
        }
        let all = log.recent(&EventFilter::default(), usize::MAX);
        assert_eq!(all.len(), KEEP);
        assert_eq!(all.last().unwrap().seq, (KEEP + 5) as u64);
        assert_eq!(log.recent(&EventFilter::default(), 2).len(), 2);
    }
}
