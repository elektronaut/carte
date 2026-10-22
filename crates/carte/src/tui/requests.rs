//! The requests tab's buffer: the requests followed for the selected node, which one
//! is selected, and which one is open.

use std::collections::VecDeque;
use std::ops::Range;

use carte_api::ProxiedRequest;
use ratatui::layout::Rect;

const CAPACITY: usize = 1000;

#[derive(Debug, Default)]
pub struct Requests {
    pub generation: u64,
    /// The scope the stream follows; a new one means a new stream.
    pub key: Option<String>,
    rows: VecDeque<ProxiedRequest>,
    /// The selected request, by sequence number; `None` follows the newest.
    pub selected: Option<u64>,
    /// The request shown in full, by sequence number.
    pub open: Option<u64>,
    /// Rows above the top of the open request.
    pub detail_scroll: usize,
    pub ended: Option<String>,
    /// Rows the tab showed last time it was drawn.
    pub height: usize,
    /// The first row shown last time the list was drawn.
    pub offset: usize,
    /// Where the selected request was drawn beside the list while zoomed.
    pub detail_area: Rect,
}

impl Requests {
    /// Starts over for a new stream and returns its generation.
    pub fn restart(&mut self, key: Option<String>) -> u64 {
        self.generation += 1;
        self.key = key;
        self.rows.clear();
        self.selected = None;
        self.open = None;
        self.detail_scroll = 0;
        self.ended = None;
        self.offset = 0;
        self.generation
    }

    pub fn push(&mut self, generation: u64, request: ProxiedRequest) {
        if generation != self.generation {
            return;
        }
        self.rows.push_back(request);
        if self.rows.len() > CAPACITY {
            self.rows.pop_front();
            let first = self.rows.front().map_or(0, |r| r.seq);
            if let Some(seq) = &mut self.selected {
                *seq = (*seq).max(first);
            }
        }
    }

    pub fn end(&mut self, generation: u64, error: Option<String>) {
        if generation == self.generation {
            self.ended = Some(error.unwrap_or_else(|| "the stream ended".into()));
        }
    }

    pub fn rows(&self) -> &VecDeque<ProxiedRequest> {
        &self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn index_of(&self, seq: u64) -> Option<usize> {
        self.rows.iter().position(|r| r.seq >= seq)
    }

    /// The selected row's index; `None` while following.
    pub fn selected_index(&self) -> Option<usize> {
        self.index_of(self.selected?)
    }

    /// Moves the selection `delta` rows down; up from following starts at the newest.
    pub fn select_by(&mut self, delta: isize) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        let from = match self.selected_index() {
            Some(index) => index,
            None if delta < 0 => last + 1,
            None => return,
        };
        let index = from.saturating_add_signed(delta).min(last);
        self.selected = Some(self.rows[index].seq);
        self.detail_scroll = 0;
    }

    pub fn select_index(&mut self, index: usize) {
        if let Some(request) = self.rows.get(index) {
            self.selected = Some(request.seq);
            self.detail_scroll = 0;
        }
    }

    pub fn follow(&mut self) {
        self.selected = None;
        self.detail_scroll = 0;
    }

    /// The selected request, or the newest while following.
    pub fn shown(&self) -> Option<&ProxiedRequest> {
        match self.selected_index() {
            Some(index) => self.rows.get(index),
            None => self.rows.back(),
        }
    }

    /// Opens the selected request, or the newest while following.
    pub fn open_selected(&mut self) {
        if let Some(seq) = self.shown().map(|r| r.seq) {
            self.selected = Some(seq);
            self.open = Some(seq);
            self.detail_scroll = 0;
        }
    }

    pub fn request(&self, seq: u64) -> Option<&ProxiedRequest> {
        self.rows.iter().find(|r| r.seq == seq)
    }

    #[cfg(test)]
    pub fn opened(&self) -> Option<&ProxiedRequest> {
        self.request(self.open?)
    }

    /// The rows that fill `height` rows: the newest while following, otherwise a
    /// window that keeps the selection in view and moves only past its edges.
    pub fn window(&mut self, height: usize) -> Range<usize> {
        let len = self.rows.len();
        let most = len.saturating_sub(height);
        self.offset = match self.selected_index() {
            None => most,
            Some(index) if index < self.offset => index,
            Some(index) if index >= self.offset + height => index + 1 - height,
            Some(_) => self.offset.min(most),
        };
        self.offset..(self.offset + height).min(len)
    }
}

#[cfg(test)]
pub mod tests {
    use carte_api::{Header, ProxiedRequest, RequestRoute};

    use super::*;

    pub fn request(seq: u64, path: &str, status: Option<u16>) -> ProxiedRequest {
        ProxiedRequest {
            seq,
            at_ms: 0,
            subject: None,
            method: "GET".into(),
            path: path.into(),
            host: "nova-billing.test".into(),
            status,
            cancelled: status.is_none(),
            held_ms: 0,
            upstream_ms: Some(12),
            shared: false,
            visitor: None,
            websocket: false,
            route: RequestRoute {
                checkout: None,
                via_alias: false,
                served: None,
                warmed: Vec::new(),
            },
            request_headers: vec![Header {
                name: "accept".into(),
                value: "*/*".into(),
            }],
            response_headers: Vec::new(),
            headers_truncated: false,
        }
    }

    fn filled(count: u64) -> Requests {
        let mut requests = Requests::default();
        let generation = requests.restart(Some("nova".into()));
        for seq in 0..count {
            requests.push(generation, request(seq, &format!("/{seq}"), Some(200)));
        }
        requests
    }

    #[test]
    fn follows_the_newest_until_a_row_is_selected() {
        let mut requests = filled(10);
        assert_eq!(requests.window(4), 6..10);
        requests.select_by(-1);
        assert_eq!(
            requests.selected,
            Some(9),
            "up from following picks the newest"
        );
        requests.select_by(-5);
        assert_eq!(requests.window(4), 4..8);
        let generation = requests.generation;
        requests.push(generation, request(10, "/10", Some(200)));
        assert_eq!(
            requests.window(4),
            4..8,
            "a new request leaves the view still"
        );
        requests.select_by(100);
        assert_eq!(requests.selected, Some(10));
        requests.follow();
        assert_eq!(requests.window(4), 7..11);
    }

    #[test]
    fn opens_the_selected_request_or_the_newest() {
        let mut requests = filled(3);
        requests.open_selected();
        assert_eq!(requests.opened().map(|r| r.seq), Some(2));
        requests.select_by(-2);
        requests.open_selected();
        assert_eq!(requests.opened().map(|r| r.path.as_str()), Some("/0"));
        requests.push(0, request(3, "/stale", Some(200)));
        assert_eq!(
            requests.rows().len(),
            3,
            "an earlier stream's request is dropped"
        );
    }
}
