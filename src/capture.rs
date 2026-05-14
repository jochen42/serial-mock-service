// Bounded capture buffers for one port.
//
// Two views over the same incoming bytes:
//   * `raw`  — flat ring of bytes; oldest dropped when full
//   * `events` — structured per-line entries with monotonically-
//                increasing id, also bounded

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Capture {
    raw: VecDeque<u8>,
    max_raw: usize,
    events: VecDeque<Event>,
    max_events: usize,
    next_id: u64,
}

#[derive(Clone)]
pub struct Event {
    pub id: u64,
    pub ts_ms: u64,
    pub data: Vec<u8>,
    /// `"<scenario>:<rule_index>"` when an input rule fired, else None.
    pub matched_rule: Option<String>,
}

impl Capture {
    pub fn new(max_raw: usize, max_events: usize) -> Self {
        Self {
            raw: VecDeque::with_capacity(max_raw.min(8192)),
            max_raw,
            events: VecDeque::with_capacity(max_events.min(256)),
            max_events,
            next_id: 1,
        }
    }

    /// Append a chunk to the raw ring (no event emitted here).
    pub fn append_raw(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if self.raw.len() == self.max_raw {
                self.raw.pop_front();
            }
            self.raw.push_back(b);
        }
    }

    /// Record a structured event for a complete input line.
    pub fn push_event(&mut self, data: Vec<u8>, matched_rule: Option<String>) {
        let event = Event {
            id: self.next_id,
            ts_ms: now_ms(),
            data,
            matched_rule,
        };
        self.next_id += 1;
        if self.events.len() == self.max_events {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw.iter().copied().collect()
    }

    pub fn clear_raw(&mut self) {
        self.raw.clear();
    }

    pub fn events_since(&self, since: u64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| e.id > since)
            .cloned()
            .collect()
    }

    pub fn clear_events(&mut self) {
        self.events.clear();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ring_bounds_at_capacity() {
        let mut cap = Capture::new(4, 10);
        cap.append_raw(b"ABCDEF");
        assert_eq!(cap.raw_bytes(), b"CDEF");
    }

    #[test]
    fn raw_clear_empties_buffer() {
        let mut cap = Capture::new(16, 10);
        cap.append_raw(b"hi");
        cap.clear_raw();
        assert_eq!(cap.raw_bytes(), b"");
    }

    #[test]
    fn events_are_monotonically_ided() {
        let mut cap = Capture::new(64, 10);
        cap.push_event(b"a".to_vec(), None);
        cap.push_event(b"b".to_vec(), Some("s:0".into()));
        let events = cap.events_since(0);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, 1);
        assert_eq!(events[1].id, 2);
        assert_eq!(events[1].matched_rule.as_deref(), Some("s:0"));
    }

    #[test]
    fn events_since_filters_by_id() {
        let mut cap = Capture::new(64, 10);
        for i in 0..5 {
            cap.push_event(vec![b'a' + i as u8], None);
        }
        let tail = cap.events_since(3);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].id, 4);
    }

    #[test]
    fn event_log_drops_oldest_when_full_but_ids_keep_advancing() {
        let mut cap = Capture::new(64, 3);
        for i in 0..5 {
            cap.push_event(vec![b'a' + i as u8], None);
        }
        let events = cap.events_since(0);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].id, 3, "oldest should have been dropped");
        assert_eq!(events[2].id, 5);
    }
}
