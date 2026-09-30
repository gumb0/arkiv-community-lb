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
    integrity::Verdict,
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
    /// The two gates on eligibility as one byte, a bit per gate, set
    /// while that gate lets the provider through: health, by the probes,
    /// and integrity, by the absence of a confirmed divergence. Eligible
    /// is both bits set. One word, so a change to either gate sees the
    /// other exactly, and a flip of what `eligible()` answers is logged
    /// once. Read through `eligible`, `healthy` and
    /// `serving_wrong_data`, written through `set_eligibility`; nothing
    /// else touches the bits. Providers are born with the health bit
    /// clear: nothing is served until the first probes pass.
    eligibility: AtomicU8,
    /// The last integrity verdict, encoded, `0` before any; and the
    /// height it was given at.
    verdict: AtomicU8,
    verdict_height: AtomicU64,
    /// Positive = consecutive successes (probes only), negative =
    /// consecutive failures (probes and traffic alike).
    pub health_streak: AtomicI64,
    /// Last head height a probe returned. `u64::MAX` means no
    /// successful height probe yet.
    height: AtomicU64,
    /// Confirmed to be on the same chain as the reference. False until
    /// the first passing check; a mismatch clears it and quarantines.
    pub chain_verified: AtomicBool,
    /// Asks the Monitor for a chain check at its next sweep, ahead of
    /// the chain round. Set for a provider that just joined the pool,
    /// and again when its tunnel is admitted: without it a newcomer
    /// waits up to a chain-check interval before its first probe.
    chain_check_due: AtomicBool,
    /// When the next probe is due. Failing probes past the quarantine
    /// point push this out. A `Mutex` because `Instant` has no atomic;
    /// the Monitor touches it, briefly, and `schedule_probe_now` once
    /// per admitted tunnel.
    next_probe: Mutex<Instant>,
    /// Consecutive unanswered probes, the backoff input. Kept apart
    /// from the health streak so traffic failures cannot deepen the
    /// backoff.
    unanswered_probe_streak: AtomicU32,
    /// Completed forwards in the current settlement period, the
    /// billing basis: what the agreement's open counter record on the
    /// chain should say.
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

/// A marketplace provider's id in the pool: its address, lowercase.
pub fn marketplace_id(address: Address) -> String {
    format!("{address:#x}")
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
    pub fn from_marketplace(address: Address, agreement_id: EntityKey, port: u16) -> Self {
        let url = Url::parse(&format!("http://127.0.0.1:{port}")).expect("a loopback url parses");
        Self::new(
            marketplace_id(address),
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
            eligibility: AtomicU8::new(Gate::Integrity.bit()),
            verdict: AtomicU8::new(0),
            verdict_height: AtomicU64::new(0),
            health_streak: AtomicI64::new(0),
            height: AtomicU64::new(u64::MAX),
            chain_verified: AtomicBool::new(false),
            chain_check_due: AtomicBool::new(true),
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

    /// Makes the next probe due at once. A marketplace provider enters
    /// the pool before its tunnel exists, so its probes fail and back
    /// off; when the tunnel is admitted, waiting out that backoff would
    /// keep a working node out of rotation for minutes.
    pub fn schedule_probe_now(&self) {
        *self.next_probe() = Instant::now();
        // The probe is gated by the chain check; ask for that too.
        self.chain_check_due.store(true, Ordering::Relaxed);
    }

    /// Whether a chain check was asked for since the last one, clearing
    /// the request. The Monitor's to call.
    pub fn take_chain_check_due(&self) -> bool {
        self.chain_check_due.swap(false, Ordering::Relaxed)
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

    /// The settlement period just written and closed: its count is no
    /// longer the entry's to carry. Subtracted rather than cleared, so
    /// a request served while the closing write was in flight stays
    /// and belongs to the next period.
    pub fn subtract_served(&self, count: u64) {
        let _ = self
            .served
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |served| {
                Some(served.saturating_sub(count))
            });
    }

    /// What this settlement period already counted on the chain, from
    /// the agreement's open counter record. Added, not stored, so a
    /// request served between the entry joining the pool and the record
    /// being read is not lost.
    pub fn seed_served(&self, count: u64) {
        self.served.fetch_add(count, Ordering::Relaxed);
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
    /// source of the signal that keeps it out. While the provider is
    /// unhealthy, its latest health signal's — for a fresh provider,
    /// `probe`, meaning no passing probe yet; while it is healthy but
    /// was found serving wrong data, `integrity`, and the last verdict
    /// beside it says when. `None` while eligible.
    pub fn ineligibility_reason(&self) -> Option<&'static str> {
        if self.eligible() {
            return None;
        }
        if !self.healthy() {
            return HealthSignal::from_code(self.last_health_source.load(Ordering::Relaxed))
                .map(HealthSignal::as_str);
        }
        Some(HealthSignal::Integrity.as_str())
    }

    fn record_health_source(&self, source: HealthSignal) {
        self.last_health_source
            .store(source as u8, Ordering::Relaxed);
    }

    /// The agreement this provider serves under, if it came from the
    /// marketplace.
    pub fn agreement_id(&self) -> Option<EntityKey> {
        match &self.source {
            Source::Static => None,
            Source::Marketplace { agreement_id, .. } => Some(*agreement_id),
        }
    }

    /// In rotation: both gates let it through.
    pub fn eligible(&self) -> bool {
        self.eligibility.load(Ordering::Relaxed) == ALL_GATES
    }

    /// The probes pass.
    pub fn healthy(&self) -> bool {
        self.eligibility.load(Ordering::Relaxed) & Gate::Health.bit() != 0
    }

    /// Found serving data that is not the chain's, and no passing round
    /// since.
    pub fn serving_wrong_data(&self) -> bool {
        self.eligibility.load(Ordering::Relaxed) & Gate::Integrity.bit() == 0
    }

    /// Opens or closes the health gate by hand, for tests that place
    /// providers without probing them. The integrity gate is untouched.
    pub fn set_health(&self, value: bool) {
        self.set_eligibility(Gate::Health, value);
    }

    /// Records one integrity verdict and the height it was given at.
    pub fn record_integrity(&self, verdict: Verdict, height: u64) {
        self.verdict
            .store(encode_verdict(verdict), Ordering::Relaxed);
        self.verdict_height.store(height, Ordering::Relaxed);
        match verdict {
            Verdict::Divergence => {
                self.set_eligibility_and_log(Gate::Integrity, false, HealthSignal::Integrity)
            }
            // Only a passing round lifts an integrity quarantine, never
            // the probes.
            Verdict::Match => {
                self.set_eligibility_and_log(Gate::Integrity, true, HealthSignal::Integrity)
            }
            // Stale ticks no health either: the probes see the same lag,
            // with the same tolerance, every few seconds, so a tick from
            // a round would count it twice.
            Verdict::Stale | Verdict::Unknown => {}
        }
    }

    /// The last integrity verdict and the height it was given at, if
    /// any round has judged this provider yet.
    pub fn last_verdict(&self) -> Option<(Verdict, u64)> {
        decode_verdict(self.verdict.load(Ordering::Relaxed))
            .map(|verdict| (verdict, self.verdict_height.load(Ordering::Relaxed)))
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
            self.set_eligibility_and_log(Gate::Health, true, source);
        } else if streak <= -flip {
            self.set_eligibility_and_log(Gate::Health, false, source);
        }
    }

    /// Quarantines immediately, without waiting for a failure streak —
    /// for verdicts that leave no room for doubt. The streak resets so
    /// recovery starts from zero once the cause is fixed.
    pub fn quarantine(&self, source: HealthSignal) {
        self.health_streak.store(0, Ordering::Relaxed);
        self.record_health_source(source);
        self.set_eligibility_and_log(Gate::Health, false, source);
    }

    /// Sets one gate's bit to `value` and answers what `eligible()` said
    /// before and after, from the one atomic update, so two racing
    /// callers cannot both see the same flip.
    fn set_eligibility(&self, gate: Gate, value: bool) -> (bool, bool) {
        let bit = gate.bit();
        // Either atomic returns the byte as it was just before.
        let before = if value {
            self.eligibility.fetch_or(bit, Ordering::Relaxed)
        } else {
            self.eligibility.fetch_and(!bit, Ordering::Relaxed)
        };
        let after = if value { before | bit } else { before & !bit };
        (before == ALL_GATES, after == ALL_GATES)
    }

    /// Sets a gate and logs the flip when what `eligible()` answers
    /// actually changed. A health readmission of a provider found
    /// serving wrong data changes nothing visible and logs nothing; the
    /// match that clears that afterwards is the flip.
    fn set_eligibility_and_log(&self, gate: Gate, value: bool, source: HealthSignal) {
        let (was, now) = self.set_eligibility(gate, value);
        if was != now {
            tracing::info!(
                provider = %self.id,
                eligible = now,
                source = %source,
                "eligibility flip"
            );
        }
    }
}

/// The two gates on eligibility. Each has a bit in the entry's
/// eligibility byte, set while the gate lets the provider through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Set by passing probes; cleared by failing probes and by traffic
    /// failures.
    Health,
    /// Cleared by a confirmed divergence; set by a passing round.
    Integrity,
}

impl Gate {
    const fn bit(self) -> u8 {
        match self {
            Self::Health => 1,
            Self::Integrity => 2,
        }
    }
}

/// Both bits set: eligible.
const ALL_GATES: u8 = Gate::Health.bit() | Gate::Integrity.bit();

fn encode_verdict(verdict: Verdict) -> u8 {
    match verdict {
        Verdict::Match => 1,
        Verdict::Stale => 2,
        Verdict::Divergence => 3,
        Verdict::Unknown => 4,
    }
}

fn decode_verdict(code: u8) -> Option<Verdict> {
    match code {
        1 => Some(Verdict::Match),
        2 => Some(Verdict::Stale),
        3 => Some(Verdict::Divergence),
        4 => Some(Verdict::Unknown),
        _ => None,
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
    /// A verdict from an integrity round: the second gate, never a
    /// tick on the streak.
    Integrity = 5,
}

impl HealthSignal {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Probe),
            2 => Some(Self::Traffic),
            3 => Some(Self::Lag),
            4 => Some(Self::Chain),
            5 => Some(Self::Integrity),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Traffic => "traffic",
            Self::Lag => "lag",
            Self::Chain => "chain",
            Self::Integrity => "integrity",
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

    /// The member with this id, while it is one.
    pub fn get(&self, id: &str) -> Option<Arc<Provider>> {
        self.snapshot()
            .iter()
            .find(|provider| provider.id == id)
            .cloned()
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
    fn a_seeded_count_adds_to_what_the_entry_already_served() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        // A request answered before the agreement's record was read
        // still counts: the period's count on the chain is added to it,
        // not written over it.
        provider.record_served();
        provider.seed_served(48213);
        assert_eq!(provider.served.load(Ordering::Relaxed), 48214);
    }

    #[test]
    fn a_closed_period_leaves_what_was_served_after_its_count_was_taken() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.seed_served(10);
        // The closing write carries ten; three more are answered while
        // it is in flight, and they belong to the next period.
        for _ in 0..3 {
            provider.record_served();
        }
        provider.subtract_served(10);
        assert_eq!(provider.served.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_confirmed_divergence_takes_a_healthy_provider_out() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.set_health(true);
        provider.record_integrity(Verdict::Divergence, 1_204_000);
        assert!(!provider.eligible());
        assert_eq!(provider.ineligibility_reason(), Some("integrity"));
    }

    #[test]
    fn probe_successes_do_not_readmit_a_diverged_provider_but_a_match_does() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.set_health(true);
        provider.record_integrity(Verdict::Divergence, 10);
        // The health gate is open the whole time; the probes cannot
        // touch the other one.
        for _ in 0..3 {
            provider.record_health(true, 3, HealthSignal::Probe);
        }
        assert!(!provider.eligible());
        provider.record_integrity(Verdict::Match, 11);
        assert!(provider.eligible());
    }

    #[test]
    fn stale_and_unknown_leave_the_integrity_gate_as_it_is() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.set_health(true);
        provider.record_integrity(Verdict::Unknown, 10);
        assert!(provider.eligible(), "nothing judged, nothing changed");
        provider.record_integrity(Verdict::Divergence, 11);
        provider.record_integrity(Verdict::Stale, 12);
        provider.record_integrity(Verdict::Unknown, 13);
        assert!(!provider.eligible(), "only a match lifts it");
        assert_eq!(provider.last_verdict(), Some((Verdict::Unknown, 13)));
    }

    #[test]
    fn a_diverged_provider_that_is_also_unhealthy_names_its_health_reason() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.set_health(true);
        provider.record_integrity(Verdict::Divergence, 10);
        provider.quarantine(HealthSignal::Chain);
        assert_eq!(provider.ineligibility_reason(), Some("chain"));
        // Health back, still diverged: the reason moves to integrity.
        for _ in 0..3 {
            provider.record_health(true, 3, HealthSignal::Probe);
        }
        assert_eq!(provider.ineligibility_reason(), Some("integrity"));
    }

    #[test]
    fn the_last_verdict_is_the_divergence_and_its_height() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        provider.record_integrity(Verdict::Divergence, 1_204_000);
        assert_eq!(
            provider.last_verdict(),
            Some((Verdict::Divergence, 1_204_000))
        );
    }

    #[test]
    fn the_two_gates_are_read_apart() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        assert!(!provider.healthy(), "born unprobed");
        assert!(!provider.serving_wrong_data(), "born unjudged");
        provider.set_health(true);
        provider.record_integrity(Verdict::Divergence, 10);
        assert!(provider.healthy());
        assert!(provider.serving_wrong_data());
        provider.quarantine(HealthSignal::Probe);
        provider.record_integrity(Verdict::Match, 11);
        assert!(!provider.healthy());
        assert!(!provider.serving_wrong_data());
    }

    #[test]
    fn every_verdict_survives_the_slot() {
        let pool = pool(&["a"]);
        let provider = &pool.snapshot()[0];
        for verdict in [
            Verdict::Match,
            Verdict::Stale,
            Verdict::Divergence,
            Verdict::Unknown,
        ] {
            provider.record_integrity(verdict, 7);
            assert_eq!(provider.last_verdict(), Some((verdict, 7)));
        }
    }

    #[test]
    fn a_provider_never_judged_has_no_verdict() {
        let pool = pool(&["a"]);
        assert_eq!(pool.snapshot()[0].last_verdict(), None);
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
        pool.snapshot()[1].set_health(true);
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
        pool.snapshot()[1].set_health(true);

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
        pool.snapshot()[0].set_health(true);
        let added = pool.add(Provider::from_marketplace(
            Address::repeat_byte(0xbb),
            EntityKey::repeat_byte(0x01),
            20_000,
        ));
        assert_eq!(added.id, format!("{:#x}", Address::repeat_byte(0xbb)));
        assert_eq!(added.url.as_str(), "http://127.0.0.1:20000/");
        for _ in 0..4 {
            assert_eq!(pool.next_eligible().expect("a is eligible").id, "a");
        }

        added.set_health(true);
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
            provider.set_health(true);
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
        pool.snapshot()[0].set_health(true);
        pool.snapshot()[3].set_health(true);
        let mut picks = std::collections::HashMap::new();
        for _ in 0..100 {
            let id = pool.next_eligible().expect("eligible exist").id.clone();
            *picks.entry(id).or_insert(0) += 1;
        }
        assert_eq!(picks["a"], 50, "{picks:?}");
        assert_eq!(picks["d"], 50, "{picks:?}");
    }
}
