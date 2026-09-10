//! The provider pool. Membership changes while the LB runs: the static
//! providers from the config file are there from the start, marketplace
//! providers come and go with their agreements. Readers take a snapshot
//! of the membership and never lock; a provider is shared by reference
//! count, so an entry a reader still holds stays valid after its
//! removal. Everything mutable on a provider is atomic, so the hot path
//! reads without locks. Health successes come only from probes; traffic
//! adds only failures — so traffic can take a provider out of rotation,
//! but never bring one in.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use reqwest::Url;

use crate::{
    chain::records::{Address, EntityKey},
    config,
};

/// How a provider entered the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Listed in the config file.
    Static,
    /// Accepted from the marketplace: its offer's address, the agreement
    /// record's key, and the tunnel port the LB reaches it through.
    Marketplace {
        address: Address,
        agreement_id: EntityKey,
        port: u16,
    },
}

#[derive(Debug)]
pub struct Provider {
    pub id: String,
    pub url: Url,
    pub source: Source,
    /// In or out of rotation. Providers are born ineligible: nothing is
    /// served until the first probes pass.
    eligible: AtomicBool,
    /// Positive = consecutive successes (probes only), negative =
    /// consecutive failures (probes and traffic alike).
    pub health_streak: AtomicI64,
    /// Last head height a probe returned. `u64::MAX` means no
    /// successful height probe yet.
    height: AtomicU64,
    /// Confirmed to be on the same chain as the reference. False until
    /// the first passing check; a mismatch clears it and quarantines.
    pub chain_verified: AtomicBool,
    /// When the next probe is due. Failing probes past the quarantine
    /// point push this out. A `Mutex` because `Instant` has no atomic;
    /// only the Monitor touches it, briefly.
    next_probe: Mutex<Instant>,
    /// Consecutive unanswered probes, the backoff input. Kept apart
    /// from the health streak so traffic failures cannot deepen the
    /// backoff.
    unanswered_probe_streak: AtomicU32,
    /// Completed forwards, the billing basis.
    pub served: AtomicU64,
    /// Client-traffic attempts that did not produce a provider answer.
    pub transport_failures: AtomicU64,
    /// Source of the most recent health signal. Starts as `Probe`:
    /// born-ineligible means no passing probe yet.
    last_health_source: AtomicU8,
    /// Round-trip time of the last block-height probe. `u64::MAX`
    /// means this provider has not been probed yet.
    last_probe_ms: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
#[error("provider {id:?}: url {url:?} does not parse")]
pub struct InvalidUrl {
    pub id: String,
    pub url: String,
    #[source]
    source: url::ParseError,
}

impl Provider {
    fn from_config(provider: &config::Provider) -> Result<Self, InvalidUrl> {
        let url = Url::parse(&provider.url).map_err(|source| InvalidUrl {
            id: provider.id.clone(),
            url: provider.url.clone(),
            source,
        })?;
        Ok(Self::new(provider.id.clone(), url, Source::Static))
    }

    /// A marketplace provider, named by its address and reached through
    /// its tunnel port on the loopback.
    pub fn marketplace(address: Address, agreement_id: EntityKey, port: u16) -> Self {
        let url = Url::parse(&format!("http://127.0.0.1:{port}")).expect("a loopback url parses");
        Self::new(
            format!("{address:#x}"),
            url,
            Source::Marketplace {
                address,
                agreement_id,
                port,
            },
        )
    }

    fn new(id: String, url: Url, source: Source) -> Self {
        Self {
            id,
            url,
            source,
            eligible: AtomicBool::new(false),
            health_streak: AtomicI64::new(0),
            height: AtomicU64::new(u64::MAX),
            chain_verified: AtomicBool::new(false),
            next_probe: Mutex::new(Instant::now()),
            unanswered_probe_streak: AtomicU32::new(0),
            served: AtomicU64::new(0),
            transport_failures: AtomicU64::new(0),
            last_health_source: AtomicU8::new(HealthSignal::Probe as u8),
            last_probe_ms: AtomicU64::new(u64::MAX),
        }
    }

    /// The next-probe time, locked. Poisoning is ignored.
    pub fn next_probe(&self) -> std::sync::MutexGuard<'_, Instant> {
        self.next_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One more probe gone unanswered.
    pub fn record_unanswered_probe(&self) {
        self.unanswered_probe_streak.fetch_add(1, Ordering::Relaxed);
    }

    /// An answered probe ends the unanswered streak.
    pub fn record_answered_probe(&self) {
        self.unanswered_probe_streak.store(0, Ordering::Relaxed);
    }

    /// Depth of the unanswered-probe streak.
    pub fn unanswered_probe_streak(&self) -> u32 {
        self.unanswered_probe_streak.load(Ordering::Relaxed)
    }

    /// One completed forward. Answers, not attempts: this is the
    /// billing basis.
    pub fn record_served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
    }

    /// One client-traffic attempt that produced no provider answer.
    pub fn record_transport_failure(&self) {
        self.transport_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one block-height probe's round-trip time.
    pub fn record_probe_duration(&self, duration: Duration) {
        // u64::MAX would read back as "never probed", so clamp to -1.
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX - 1);
        self.last_probe_ms.store(millis, Ordering::Relaxed);
    }

    pub fn last_probe_ms(&self) -> Option<u64> {
        match self.last_probe_ms.load(Ordering::Relaxed) {
            u64::MAX => None,
            millis => Some(millis),
        }
    }

    /// Records a successfully decoded head height.
    pub fn record_height(&self, height: u64) {
        // u64::MAX would read back as "never probed", so clamp to -1.
        self.height
            .store(height.min(u64::MAX - 1), Ordering::Relaxed);
    }

    pub fn last_height(&self) -> Option<u64> {
        match self.height.load(Ordering::Relaxed) {
            u64::MAX => None,
            height => Some(height),
        }
    }

    /// Why this provider is out of rotation, for the admin view: the
    /// source of its latest health signal — for a fresh provider,
    /// `probe`, meaning no passing probe yet. `None` while eligible.
    pub fn ineligibility_reason(&self) -> Option<&'static str> {
        if self.eligible() {
            return None;
        }
        HealthSignal::from_code(self.last_health_source.load(Ordering::Relaxed))
            .map(HealthSignal::as_str)
    }

    fn record_health_source(&self, source: HealthSignal) {
        self.last_health_source
            .store(source as u8, Ordering::Relaxed);
    }

    pub fn eligible(&self) -> bool {
        self.eligible.load(Ordering::Relaxed)
    }

    pub fn set_eligible(&self, value: bool) {
        self.eligible.store(value, Ordering::Relaxed);
    }

    /// Records one health signal and flips eligibility once `flip_after`
    /// results in a row agree. A provider that alternates between
    /// success and failure does not flap in and out.
    /// Every flip logs one event naming its source.
    pub fn record_health(&self, success: bool, flip_after: u32, source: HealthSignal) {
        self.record_health_source(source);

        let step = |streak: i64| {
            if success {
                streak.max(0).saturating_add(1)
            } else {
                streak.min(0).saturating_sub(1)
            }
        };
        let previous = self
            .health_streak
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |streak| {
                Some(step(streak))
            })
            .expect("the update never rejects");

        // Recompute our new value, we can't use health_streak directly,
        // because it could be already changed by concurrent call.
        let streak = step(previous);
        let flip = i64::from(flip_after);
        // Using outdated `streak` for eligibility decision is harmless,
        // because no single result can flip state (flip_after >= 2).
        if streak >= flip {
            self.set_eligible_and_log(true, source);
        } else if streak <= -flip {
            self.set_eligible_and_log(false, source);
        }
    }

    /// Quarantines immediately, without waiting for a failure streak —
    /// for verdicts that leave no room for doubt. The streak resets so
    /// recovery starts from zero once the cause is fixed.
    pub fn quarantine(&self, source: HealthSignal) {
        self.health_streak.store(0, Ordering::Relaxed);
        self.record_health_source(source);
        self.set_eligible_and_log(false, source);
    }

    /// Sets eligibility and logs the flip when the value actually
    /// changed. `swap` makes check-and-set one atomic step, so two
    /// racing callers cannot both log the same flip.
    fn set_eligible_and_log(&self, value: bool, source: HealthSignal) {
        if self.eligible.swap(value, Ordering::Relaxed) != value {
            tracing::info!(
                provider = %self.id,
                eligible = value,
                source = %source,
                "health flip"
            );
        }
    }
}

/// Where a health signal came from, for the flip log line.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum HealthSignal {
    Probe = 1,
    Traffic = 2,
    Lag = 3,
    Chain = 4,
}

impl HealthSignal {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Probe),
            2 => Some(Self::Traffic),
            3 => Some(Self::Lag),
            4 => Some(Self::Chain),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Traffic => "traffic",
            Self::Lag => "lag",
            Self::Chain => "chain",
        }
    }
}

impl std::fmt::Display for HealthSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The membership as one immutable list, replaced whole on every change.
/// Changes are rare (an agreement made or expired), reads are every
/// request, so copying the list on change is the right trade.
type Members = Vec<Arc<Provider>>;

#[derive(Debug)]
pub struct Pool {
    members: ArcSwap<Members>,
    cursor: AtomicUsize,
}

impl Pool {
    pub fn new(providers: &[config::Provider]) -> Result<Self, InvalidUrl> {
        let mut members = Members::with_capacity(providers.len());
        for provider in providers {
            members.push(Arc::new(Provider::from_config(provider)?));
        }
        Ok(Self {
            members: ArcSwap::from_pointee(members),
            cursor: AtomicUsize::new(0),
        })
    }

    /// The membership at this moment. Later changes do not show in it.
    pub fn snapshot(&self) -> Arc<Members> {
        self.members.load_full()
    }

    /// Adds a provider, born ineligible like every other.
    pub fn add(&self, provider: Provider) -> Arc<Provider> {
        let provider = Arc::new(provider);
        self.members.rcu(|members| {
            let mut members = Members::clone(members);
            members.push(provider.clone());
            members
        });
        provider
    }

    /// Removes the provider with this id. Once this returns, no new
    /// selection can pick it; a selection already holding it finishes
    /// with it.
    pub fn remove(&self, id: &str) -> Option<Arc<Provider>> {
        let mut removed = None;
        self.members.rcu(|members| {
            let mut members = Members::clone(members);
            if let Some(index) = members.iter().position(|provider| provider.id == id) {
                removed = Some(members.remove(index));
            }
            members
        });
        removed
    }

    /// Round robin over eligible providers. The cursor is the next position
    /// to examine.
    pub fn next_eligible(&self) -> Option<Arc<Provider>> {
        let members = self.members.load();
        let len = members.len();
        if len == 0 {
            return None;
        }
        for _ in 0..len {
            let index = self.cursor.fetch_add(1, Ordering::Relaxed) % len;
            let provider = &members[index];
            if provider.eligible() {
                return Some(provider.clone());
            }
        }
        // Concurrent selections advance the cursor too, so the lap
        // above may have sampled the same index twice and missed an
        // eligible provider.
        members.iter().find(|provider| provider.eligible()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(ids: &[&str]) -> Pool {
        let providers: Vec<config::Provider> = ids
            .iter()
            .map(|id| config::Provider {
                id: (*id).to_string(),
                url: format!("http://127.0.0.1:1/{id}"),
            })
            .collect();
        Pool::new(&providers).expect("urls parse")
    }

    #[test]
    fn providers_are_born_ineligible() {
        let pool = pool(&["a", "b"]);
        assert!(pool.snapshot().iter().all(|provider| !provider.eligible()));
        assert!(pool.next_eligible().is_none());
    }

    #[test]
    fn empty_pool_selects_nothing() {
        assert!(pool(&[]).next_eligible().is_none());
    }

    #[test]
    fn single_eligible_provider_is_always_picked() {
        let pool = pool(&["a", "b", "c"]);
        pool.snapshot()[1].set_eligible(true);
        for _ in 0..10 {
            assert_eq!(pool.next_eligible().expect("one eligible").id, "b");
        }
    }

    /// Selections racing on the cursor may sample the same index more
    /// than once within one lap — a lap of samples is not a lap of
    /// providers. The eligible provider must be found regardless.
    #[test]
    fn a_lone_eligible_provider_is_always_found_under_contention() {
        let pool = std::sync::Arc::new(pool(&["dead", "live"]));
        pool.snapshot()[1].set_eligible(true);

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    for _ in 0..100_000 {
                        assert!(
                            pool.next_eligible().is_some(),
                            "an eligible provider exists and must be found"
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("selection thread");
        }
    }

    #[test]
    fn an_added_provider_is_selected_once_eligible() {
        let pool = pool(&["a"]);
        pool.snapshot()[0].set_eligible(true);
        let added = pool.add(Provider::marketplace(
            Address::repeat_byte(0xbb),
            EntityKey::repeat_byte(0x01),
            20_000,
        ));
        assert_eq!(added.id, format!("{:#x}", Address::repeat_byte(0xbb)));
        assert_eq!(added.url.as_str(), "http://127.0.0.1:20000/");
        for _ in 0..4 {
            assert_eq!(pool.next_eligible().expect("a is eligible").id, "a");
        }

        added.set_eligible(true);
        let ids: Vec<String> = (0..4)
            .map(|_| pool.next_eligible().expect("both eligible").id.clone())
            .collect();
        assert!(ids.contains(&"a".to_string()), "{ids:?}");
        assert!(ids.contains(&added.id), "{ids:?}");
    }

    /// Other threads keep selecting while a provider is removed. A
    /// selection loads the list once, when it starts, so every selection
    /// that starts after `remove` returns works on the new list and
    /// cannot pick the removed provider.
    #[test]
    fn a_removed_provider_is_never_selected_again() {
        let pool = std::sync::Arc::new(pool(&["a", "b"]));
        for provider in pool.snapshot().iter() {
            provider.set_eligible(true);
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let pool = pool.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        pool.next_eligible();
                    }
                })
            })
            .collect();

        let removed = pool.remove("b").expect("b was a member");
        assert_eq!(removed.id, "b");
        assert!(removed.eligible(), "the entry itself is untouched");
        for _ in 0..1_000 {
            assert_eq!(pool.next_eligible().expect("a remains").id, "a");
        }
        assert!(pool.remove("b").is_none(), "removed twice");

        stop.store(true, Ordering::Relaxed);
        for thread in threads {
            thread.join().expect("selection thread");
        }
    }

    #[test]
    fn an_unparsable_url_names_its_provider() {
        let providers = vec![
            config::Provider {
                id: "good".into(),
                url: "http://127.0.0.1:1".into(),
            },
            config::Provider {
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
    fn a_streak_of_agreeing_results_flips_eligibility() {
        let pool = pool(&["a"]);
        let provider = pool.snapshot()[0].clone();

        provider.record_health(true, 3, HealthSignal::Probe);
        provider.record_health(true, 3, HealthSignal::Probe);
        assert!(!provider.eligible(), "two of three is not admission");
        provider.record_health(true, 3, HealthSignal::Probe);
        assert!(provider.eligible());

        provider.record_health(false, 3, HealthSignal::Probe);
        provider.record_health(false, 3, HealthSignal::Probe);
        assert!(provider.eligible(), "still in rotation until the third");
        provider.record_health(false, 3, HealthSignal::Probe);
        assert!(!provider.eligible());
    }

    #[test]
    fn one_disagreeing_result_restarts_the_streak() {
        let pool = pool(&["a"]);
        let provider = pool.snapshot()[0].clone();

        provider.record_health(false, 3, HealthSignal::Probe);
        provider.record_health(false, 3, HealthSignal::Probe);
        provider.record_health(true, 3, HealthSignal::Probe);
        assert_eq!(
            provider.health_streak.load(Ordering::Relaxed),
            1,
            "a success wipes the failures rather than counting against them"
        );
        provider.record_health(false, 3, HealthSignal::Probe);
        provider.record_health(false, 3, HealthSignal::Probe);
        assert_eq!(provider.health_streak.load(Ordering::Relaxed), -2);
    }

    #[test]
    fn quarantine_evicts_at_once_and_recovery_starts_from_zero() {
        let pool = pool(&["a"]);
        let provider = pool.snapshot()[0].clone();
        for _ in 0..5 {
            provider.record_health(true, 3, HealthSignal::Probe);
        }
        assert!(provider.eligible());

        provider.quarantine(HealthSignal::Chain);
        assert!(!provider.eligible(), "no failure streak needed");
        assert_eq!(
            provider.health_streak.load(Ordering::Relaxed),
            0,
            "the old success streak must not survive"
        );

        // Which is what makes readmission take a full flip_after again.
        provider.record_health(true, 3, HealthSignal::Probe);
        provider.record_health(true, 3, HealthSignal::Probe);
        assert!(!provider.eligible());
        provider.record_health(true, 3, HealthSignal::Probe);
        assert!(provider.eligible());
    }

    #[test]
    fn round_robin_is_even_over_the_eligible() {
        let pool = pool(&["a", "b", "c", "d"]);
        // Only the outer two are in rotation; the ineligible middle must
        // not skew the split.
        pool.snapshot()[0].set_eligible(true);
        pool.snapshot()[3].set_eligible(true);
        let mut picks = std::collections::HashMap::new();
        for _ in 0..100 {
            let id = pool.next_eligible().expect("eligible exist").id.clone();
            *picks.entry(id).or_insert(0) += 1;
        }
        assert_eq!(picks["a"], 50, "{picks:?}");
        assert_eq!(picks["d"], 50, "{picks:?}");
    }
}
