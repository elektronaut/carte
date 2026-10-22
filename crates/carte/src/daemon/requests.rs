//! The latest requests the proxy answered, kept in memory for the TUI.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use carte_api::{Header, ProxiedRequest, RequestRoute};
use tokio::sync::broadcast;

use crate::proxy::{Handled, ListenerKind};

/// Requests kept for clients that ask for the past.
const KEEP: usize = 1000;

/// One request the proxy answered.
#[derive(Debug)]
pub struct Entry {
    pub seq: u64,
    pub handled: Handled,
}

impl Entry {
    pub fn to_api(&self) -> ProxiedRequest {
        let handled = &self.handled;
        let resolution = &handled.resolution;
        ProxiedRequest {
            seq: self.seq,
            at_ms: handled
                .at
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            subject: resolution.subject.clone(),
            method: handled.method.to_string(),
            path: handled.path.clone(),
            host: handled.host.clone(),
            status: handled.status.as_u16(),
            held_ms: handled.held.as_millis() as u64,
            upstream_ms: handled.upstream.map(|upstream| upstream.as_millis() as u64),
            shared: handled.kind == ListenerKind::Share,
            visitor: handled.visitor.clone(),
            websocket: handled.websocket,
            route: RequestRoute {
                checkout: resolution.checkout.clone(),
                via_alias: resolution.via_alias,
                served: handled.served.clone(),
                warmed: resolution.warmed.clone(),
            },
            request_headers: headers(&handled.request_headers),
            response_headers: headers(&handled.response_headers),
        }
    }
}

fn headers(map: &http::HeaderMap) -> Vec<Header> {
    map.iter()
        .map(|(name, value)| Header {
            name: name.to_string(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })
        .collect()
}

pub struct RequestLog {
    ring: Mutex<Ring>,
    tx: broadcast::Sender<Arc<Entry>>,
}

struct Ring {
    entries: VecDeque<Arc<Entry>>,
    next_seq: u64,
}

impl Default for RequestLog {
    fn default() -> Self {
        RequestLog {
            ring: Mutex::new(Ring {
                entries: VecDeque::with_capacity(KEEP),
                next_seq: 0,
            }),
            tx: broadcast::channel(256).0,
        }
    }
}

impl RequestLog {
    pub fn push(&self, handled: Handled) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.next_seq += 1;
        let entry = Arc::new(Entry {
            seq: ring.next_seq,
            handled,
        });
        if ring.entries.len() == KEEP {
            ring.entries.pop_front();
        }
        ring.entries.push_back(entry.clone());
        let _ = self.tx.send(entry);
    }

    /// The last `tail` requests `keep` lets through, oldest first.
    pub fn recent(&self, keep: impl Fn(&Entry) -> bool, tail: usize) -> Vec<Arc<Entry>> {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<Arc<Entry>> = ring
            .entries
            .iter()
            .rev()
            .filter(|entry| keep(entry))
            .take(tail)
            .cloned()
            .collect();
        entries.reverse();
        entries
    }

    pub fn follow(&self) -> broadcast::Receiver<Arc<Entry>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Resolution;

    fn handled(app: Option<&str>) -> Handled {
        Handled {
            at: std::time::SystemTime::now(),
            kind: ListenerKind::Local,
            visitor: None,
            method: http::Method::GET,
            host: "web.acme.test".into(),
            path: "/".into(),
            status: http::StatusCode::OK,
            held: std::time::Duration::ZERO,
            upstream: None,
            websocket: false,
            served: carte_api::Served::Forward,
            resolution: Resolution {
                subject: app.map(|app| carte_api::EventSubject {
                    project: "acme".into(),
                    stack: Some("main".into()),
                    app: Some(app.into()),
                    process: Some("web".into()),
                }),
                ..Resolution::default()
            },
            request_headers: http::HeaderMap::new(),
            response_headers: http::HeaderMap::new(),
        }
    }

    #[test]
    fn keeps_the_most_recent() {
        let log = RequestLog::default();
        for i in 0..KEEP + 5 {
            log.push(handled(Some(if i % 2 == 0 {
                "acme-web"
            } else {
                "acme-api"
            })));
        }
        log.push(handled(None));
        let all = log.recent(|_| true, usize::MAX);
        assert_eq!(all.len(), KEEP);
        assert_eq!(all.first().unwrap().seq, 7);
        assert_eq!(all.last().unwrap().seq, (KEEP + 6) as u64);

        let web = |entry: &Entry| {
            entry
                .handled
                .resolution
                .subject
                .as_ref()
                .is_some_and(|s| s.app.as_deref() == Some("acme-web"))
        };
        let recent = log.recent(web, 3);
        assert_eq!(
            recent.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [KEEP as u64 + 1, KEEP as u64 + 3, KEEP as u64 + 5]
        );
    }
}
