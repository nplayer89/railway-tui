//! Bounded, time-ordered, de-duplicated log buffer.

use std::collections::{HashSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::model::LogLine;

pub const LOG_CAP: usize = 10_000;
/// Attribute keys that identify the replica a line came from. Task 12 confirms the real key.
pub const REPLICA_ATTR_KEYS: &[&str] = &["deploymentInstanceId", "replica"];

type Key = (DateTime<Utc>, String);

#[derive(Debug)]
pub struct LogBuffer {
    cap: usize,
    lines: VecDeque<LogLine>,
    seen: HashSet<Key>,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            lines: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// Inserts a line in timestamp order. Returns false for a duplicate, or for
    /// a line older than everything in a full buffer (it would be dropped at once).
    pub fn push(&mut self, line: LogLine) -> bool {
        if self.cap == 0 {
            return false;
        }
        let key = (line.timestamp, line.message.clone());
        if self.seen.contains(&key) {
            return false;
        }
        if self.lines.len() >= self.cap
            && self
                .lines
                .front()
                .is_some_and(|oldest| line.timestamp < oldest.timestamp)
        {
            return false;
        }
        match self.lines.back() {
            Some(last) if line.timestamp < last.timestamp => {
                let at = self
                    .lines
                    .partition_point(|l| l.timestamp <= line.timestamp);
                self.lines.insert(at, line);
            }
            _ => self.lines.push_back(line),
        }
        self.seen.insert(key);
        if self.lines.len() > self.cap {
            if let Some(old) = self.lines.pop_front() {
                self.seen.remove(&(old.timestamp, old.message));
            }
        }
        true
    }

    /// Pushes every line, returning how many were added.
    pub fn extend(&mut self, lines: impl IntoIterator<Item = LogLine>) -> usize {
        lines.into_iter().filter(|l| self.push(l.clone())).count()
    }

    pub fn last_timestamp(&self) -> Option<DateTime<Utc>> {
        self.lines.back().map(|l| l.timestamp)
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Lines matching the case-insensitive message `filter` and the `replica`, oldest first.
    pub fn visible(&self, filter: Option<&str>, replica: Option<&str>) -> Vec<&LogLine> {
        let needle = filter.filter(|f| !f.is_empty()).map(str::to_lowercase);
        self.lines
            .iter()
            .filter(|l| {
                needle
                    .as_deref()
                    .is_none_or(|n| l.message.to_lowercase().contains(n))
            })
            .filter(|l| {
                replica.is_none_or(|r| {
                    l.attributes
                        .iter()
                        .any(|(k, v)| REPLICA_ATTR_KEYS.contains(&k.as_str()) && v == r)
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn line(secs: i64, msg: &str) -> LogLine {
        LogLine {
            timestamp: Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            message: msg.into(),
            attributes: vec![],
        }
    }

    fn msgs(b: &LogBuffer) -> Vec<&str> {
        b.visible(None, None)
            .iter()
            .map(|l| l.message.as_str())
            .collect()
    }

    #[test]
    fn duplicates_dropped() {
        let mut b = LogBuffer::new(10);
        assert!(b.push(line(1, "a")));
        assert!(!b.push(line(1, "a")));
        assert!(b.push(line(1, "b")));
        assert_eq!(b.extend([line(1, "a"), line(2, "c")]), 1);
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn out_of_order_history_inserted_sorted() {
        let mut b = LogBuffer::new(10);
        b.extend([line(5, "e"), line(6, "f")]);
        b.extend([line(1, "a"), line(3, "c"), line(5, "e2")]);
        assert_eq!(msgs(&b), ["a", "c", "e", "e2", "f"]);
    }

    #[test]
    fn cap_drops_oldest_and_allows_its_key_again() {
        let mut b = LogBuffer::new(2);
        b.extend([line(1, "a"), line(2, "b"), line(3, "c")]);
        assert_eq!(msgs(&b), ["b", "c"]);
        // Still present: duplicate.
        assert!(!b.push(line(2, "b")));
        // "a" was evicted, so its key is free again (same timestamp as "c" sorts after it).
        assert!(b.push(line(3, "a")));
        assert_eq!(msgs(&b), ["c", "a"]);
        // Older than everything in a full buffer: rejected rather than stored.
        assert!(!b.push(line(0, "z")));
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn filter_is_case_insensitive_and_only_matching() {
        let mut b = LogBuffer::new(10);
        b.extend([line(1, "ERROR: x"), line(2, "ok"), line(3, "an error")]);
        assert_eq!(b.visible(Some("error"), None).len(), 2);
        assert_eq!(b.visible(Some(""), None).len(), 3);
    }

    #[test]
    fn replica_filter_uses_attributes() {
        let mut b = LogBuffer::new(10);
        let mut l1 = line(1, "a");
        l1.attributes = vec![("replica".into(), "r1".into())];
        let mut l2 = line(2, "b");
        l2.attributes = vec![("deploymentInstanceId".into(), "r2".into())];
        let mut l3 = line(3, "c");
        l3.attributes = vec![("other".into(), "r1".into())];
        b.extend([l1, l2, l3]);
        let v = b.visible(None, Some("r1"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message, "a");
        assert_eq!(b.visible(None, Some("r2"))[0].message, "b");
        assert_eq!(b.visible(Some("c"), Some("r1")).len(), 0);
    }

    #[test]
    fn last_timestamp_is_newest() {
        let mut b = LogBuffer::new(10);
        assert_eq!(b.last_timestamp(), None);
        b.extend([line(5, "e"), line(1, "a")]);
        assert_eq!(b.last_timestamp(), Some(line(5, "").timestamp));
    }
}
