//! The provider pool: long-lived entries, one per configured provider.
//! Membership is fixed until restart; everything mutable on an entry is
//! atomic, so the hot path reads without locks. The Monitor is the only
//! writer of eligibility.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use crate::config::Provider;

pub struct Entry {
    pub id: String,
    pub url: String,
    /// In or out of rotation. Entries are born ineligible: nothing is
    /// served until the first probes pass.
    eligible: AtomicBool,
    /// The shared hysteresis streak: positive = consecutive successes,
    /// negative = consecutive failures, fed by probes and traffic alike.
    pub streak: AtomicI64,
    /// Last head height a probe returned.
    pub height: AtomicU64,
    /// Completed forwards, the billing basis.
    pub served: AtomicU64,
}

impl Entry {
    fn new(provider: &Provider) -> Self {
        Self {
            id: provider.id.clone(),
            url: provider.url.clone(),
            eligible: AtomicBool::new(false),
            streak: AtomicI64::new(0),
            height: AtomicU64::new(0),
            served: AtomicU64::new(0),
        }
    }

    pub fn eligible(&self) -> bool {
        self.eligible.load(Ordering::Relaxed)
    }

    pub fn set_eligible(&self, value: bool) {
        self.eligible.store(value, Ordering::Relaxed);
    }
}

pub struct Pool {
    entries: Box<[Entry]>,
    cursor: AtomicUsize,
}

impl Pool {
    pub fn new(providers: &[Provider]) -> Self {
        Self {
            entries: providers.iter().map(Entry::new).collect(),
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Round robin over eligible entries. The cursor is the next position
    /// to examine.
    pub fn next_eligible(&self) -> Option<&Entry> {
        let len = self.entries.len();
        if len == 0 {
            return None;
        }
        for _ in 0..len {
            let index = self.cursor.fetch_add(1, Ordering::Relaxed) % len;
            let entry = &self.entries[index];
            if entry.eligible() {
                return Some(entry);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(ids: &[&str]) -> Pool {
        let providers: Vec<Provider> = ids
            .iter()
            .map(|id| Provider {
                id: (*id).to_string(),
                url: format!("http://127.0.0.1:1/{id}"),
            })
            .collect();
        Pool::new(&providers)
    }

    #[test]
    fn entries_are_born_ineligible() {
        let pool = pool(&["a", "b"]);
        assert!(pool.entries().iter().all(|entry| !entry.eligible()));
        assert!(pool.next_eligible().is_none());
    }

    #[test]
    fn empty_pool_selects_nothing() {
        assert!(pool(&[]).next_eligible().is_none());
    }

    #[test]
    fn single_eligible_entry_is_always_picked() {
        let pool = pool(&["a", "b", "c"]);
        pool.entries()[1].set_eligible(true);
        for _ in 0..10 {
            assert_eq!(pool.next_eligible().expect("one eligible").id, "b");
        }
    }

    #[test]
    fn round_robin_is_even_over_the_eligible() {
        let pool = pool(&["a", "b", "c", "d"]);
        // Only the outer two are in rotation; the ineligible middle must
        // not skew the split.
        pool.entries()[0].set_eligible(true);
        pool.entries()[3].set_eligible(true);
        let mut picks = std::collections::HashMap::new();
        for _ in 0..100 {
            let id = pool.next_eligible().expect("eligible exist").id.clone();
            *picks.entry(id).or_insert(0) += 1;
        }
        assert_eq!(picks["a"], 50, "{picks:?}");
        assert_eq!(picks["d"], 50, "{picks:?}");
    }
}
