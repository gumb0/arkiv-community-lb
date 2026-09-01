//! The provider pool. Membership is fixed until restart, and each
//! provider is long-lived; everything mutable on it is atomic, so the
//! hot path reads without locks. Health successes come only from
//! probes; traffic adds only failures — so traffic can take a provider
//! out of rotation, but never bring one in.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use reqwest::Url;

use crate::config;

#[derive(Debug)]
pub struct Provider {
    pub id: String,
    pub url: Url,
    /// In or out of rotation. Providers are born ineligible: nothing is
    /// served until the first probes pass.
    eligible: AtomicBool,
    /// Positive = consecutive successes (probes only), negative =
    /// consecutive failures (probes and traffic alike).
    pub health_streak: AtomicI64,
    /// Last head height a probe returned.
    pub height: AtomicU64,
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
    fn new(provider: &config::Provider) -> Result<Self, InvalidUrl> {
        let url = Url::parse(&provider.url).map_err(|source| InvalidUrl {
            id: provider.id.clone(),
            url: provider.url.clone(),
            source,
        })?;
        Ok(Self {
            id: provider.id.clone(),
            url,
            eligible: AtomicBool::new(false),
            health_streak: AtomicI64::new(0),
            height: AtomicU64::new(0),
            chain_verified: AtomicBool::new(false),
            next_probe: Mutex::new(Instant::now()),
            unanswered_probe_streak: AtomicU32::new(0),
            served: AtomicU64::new(0),
        })
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
pub enum HealthSignal {
    Probe,
    Traffic,
    Lag,
    Chain,
}

impl std::fmt::Display for HealthSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Probe => "probe",
            Self::Traffic => "traffic",
            Self::Lag => "lag",
            Self::Chain => "chain",
        })
    }
}

#[derive(Debug)]
pub struct Pool {
    providers: Box<[Provider]>,
    cursor: AtomicUsize,
}

impl Pool {
    pub fn new(providers: &[config::Provider]) -> Result<Self, InvalidUrl> {
        Ok(Self {
            providers: providers
                .iter()
                .map(Provider::new)
                .collect::<Result<_, _>>()?,
            cursor: AtomicUsize::new(0),
        })
    }

    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Round robin over eligible providers. The cursor is the next position
    /// to examine.
    pub fn next_eligible(&self) -> Option<&Provider> {
        let len = self.providers.len();
        if len == 0 {
            return None;
        }
        for _ in 0..len {
            let index = self.cursor.fetch_add(1, Ordering::Relaxed) % len;
            let provider = &self.providers[index];
            if provider.eligible() {
                return Some(provider);
            }
        }
        None
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
        assert!(pool.providers().iter().all(|provider| !provider.eligible()));
        assert!(pool.next_eligible().is_none());
    }

    #[test]
    fn empty_pool_selects_nothing() {
        assert!(pool(&[]).next_eligible().is_none());
    }

    #[test]
    fn single_eligible_provider_is_always_picked() {
        let pool = pool(&["a", "b", "c"]);
        pool.providers()[1].set_eligible(true);
        for _ in 0..10 {
            assert_eq!(pool.next_eligible().expect("one eligible").id, "b");
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
        let provider = &pool.providers()[0];

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
        let provider = &pool.providers()[0];

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
        let provider = &pool.providers()[0];
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
        pool.providers()[0].set_eligible(true);
        pool.providers()[3].set_eligible(true);
        let mut picks = std::collections::HashMap::new();
        for _ in 0..100 {
            let id = pool.next_eligible().expect("eligible exist").id.clone();
            *picks.entry(id).or_insert(0) += 1;
        }
        assert_eq!(picks["a"], 50, "{picks:?}");
        assert_eq!(picks["d"], 50, "{picks:?}");
    }
}
