use std::collections::VecDeque;

use chrono::{DateTime, Utc};

pub struct SelectionEntry {
    pub id: u64,
    pub text: String,
    pub timestamp: DateTime<Utc>,
}

pub struct SelectionBuffer {
    entries: VecDeque<SelectionEntry>,
    max_entries: usize,
    next_id: u64,
    query: String,
    filtered: Vec<usize>,
}

impl SelectionBuffer {
    pub fn new(max_entries: u32) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: max_entries as usize,
            next_id: 1,
            query: String::new(),
            filtered: Vec::new(),
        }
    }

    pub fn push(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        // Skip exact duplicates of the most recent entry.
        if let Some(latest) = self.entries.front() {
            if latest.text == text {
                return;
            }
        }
        let entry = SelectionEntry {
            id: self.next_id,
            text,
            timestamp: Utc::now(),
        };
        self.next_id += 1;
        self.entries.push_front(entry);
        if self.entries.len() > self.max_entries {
            self.entries.pop_back();
        }
        // Re-run search if active
        if !self.query.is_empty() {
            self.run_search();
        }
    }

    pub fn search(&mut self, query: String) {
        self.query = query;
        self.run_search();
    }

    fn run_search(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
    }

    pub fn get_query(&self) -> &str {
        &self.query
    }

    pub fn is_search_active(&self) -> bool {
        !self.query.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SelectionEntry> {
        self.entries.iter()
    }

    pub fn search_iter(&self) -> impl Iterator<Item = &SelectionEntry> {
        let entries = &self.entries;
        self.filtered.iter().filter_map(move |&i| entries.get(i))
    }

    pub fn get_by_id(&self, id: u64) -> Option<&SelectionEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.filtered.clear();
        self.query.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn set_max(&mut self, n: u32) {
        self.max_entries = n as usize;
        while self.entries.len() > self.max_entries {
            self.entries.pop_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_len() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("hello".into());
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn test_push_ignores_whitespace_only() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("   ".into());
        buf.push("".into());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_max_eviction() {
        let mut buf = SelectionBuffer::new(3);
        buf.push("a".into());
        buf.push("b".into());
        buf.push("c".into());
        buf.push("d".into()); // should evict oldest
        assert_eq!(buf.len(), 3);
        // most recent is first
        assert_eq!(buf.iter().next().unwrap().text, "d");
    }

    #[test]
    fn test_search() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("hello world".into());
        buf.push("goodbye".into());
        buf.push("hello again".into());
        buf.search("hello".into());
        let results: Vec<&str> = buf.search_iter().map(|e| e.text.as_str()).collect();
        assert_eq!(results.len(), 2);
        assert!(results.contains(&"hello world"));
        assert!(results.contains(&"hello again"));
    }

    #[test]
    fn test_search_case_insensitive() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("Hello World".into());
        buf.search("hello".into());
        assert_eq!(buf.search_iter().count(), 1);
    }

    #[test]
    fn test_clear() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("data".into());
        buf.push("more".into());
        buf.clear();
        assert_eq!(buf.len(), 0);
        assert!(!buf.is_search_active());
    }

    #[test]
    fn test_get_by_id() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("first".into());
        buf.push("second".into());
        let id = buf.iter().next().unwrap().id;
        let found = buf.get_by_id(id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().text, "second"); // most recent is first
    }

    #[test]
    fn test_get_by_id_missing() {
        let buf = SelectionBuffer::new(10);
        assert!(buf.get_by_id(999).is_none());
    }

    #[test]
    fn test_set_max_shrinks() {
        let mut buf = SelectionBuffer::new(10);
        buf.push("a".into());
        buf.push("b".into());
        buf.push("c".into());
        buf.push("d".into());
        buf.push("e".into());
        assert_eq!(buf.len(), 5);
        buf.set_max(2);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn test_is_search_active() {
        let mut buf = SelectionBuffer::new(10);
        assert!(!buf.is_search_active());
        buf.search("query".into());
        assert!(buf.is_search_active());
    }

    #[test]
    fn test_get_query() {
        let mut buf = SelectionBuffer::new(10);
        assert_eq!(buf.get_query(), "");
        buf.search("test".into());
        assert_eq!(buf.get_query(), "test");
    }
}
