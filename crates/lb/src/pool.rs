//! The provider pool: long-lived entries, one per configured provider.
//! Membership is fixed until restart; everything mutable on an entry is
//! atomic, so the hot path reads without locks. The Monitor is the only
//! writer of eligibility.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use reqwest::Url;

use crate::config::Provider;

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub url: Url,
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

#[derive(Debug, thiserror::Error)]
#[error("provider {id:?}: url {url:?} does not parse")]
pub struct InvalidUrl {
    pub id: String,
    pub url: String,
    #[source]
    source: url::ParseError,
}

impl Entry {
    fn new(provider: &Provider) -> Result<Self, InvalidUrl> {
        let url = Url::parse(&provider.url).map_err(|source| InvalidUrl {
            id: provider.id.clone(),
            url: provider.url.clone(),
            source,
        })?;
        Ok(Self {
            id: provider.id.clone(),
            url,
            eligible: AtomicBool::new(false),
            streak: AtomicI64::new(0),
            height: AtomicU64::new(0),
            served: AtomicU64::new(0),
        })
    }

    pub fn eligible(&self) -> bool {
        self.eligible.load(Ordering::Relaxed)
    }

    pub fn set_eligible(&self, value: bool) {
        self.eligible.store(value, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct Pool {
    entries: Box<[Entry]>,
    cursor: AtomicUsize,
}

impl Pool {
    pub fn new(providers: &[Provider]) -> Result<Self, InvalidUrl> {
        Ok(Self {
            entries: providers.iter().map(Entry::new).collect::<Result<_, _>>()?,
            cursor: AtomicUsize::new(0),
        })
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
        Pool::new(&providers).expect("urls parse")
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
    fn an_unparsable_url_names_its_provider() {
        let providers = vec![
            Provider {
                id: "good".into(),
                url: "http://127.0.0.1:1".into(),
            },
            Provider {
                id: "broken".into(),
                url: "http://".into(),
            },
        ];
        let error = Pool::new(&providers).expect_err("must refuse");
        assert_eq!(error.id, "broken");
        assert!(error.to_string().contains("broken"), "{error}");
        assert!(error.to_string().contains("http://"), "{error}");
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
