//! What an integrity round says about a provider. The checker itself
//! comes with the round; these are the words the pool entry records.

/// One provider's verdict from one round. Only a match lifts an
/// integrity quarantine; only a divergence, once confirmed, sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both reads answered and both agreed with the reference.
    Match,
    /// Behind the chain: no block at the height, or the entity answered
    /// too far back. The lag path's business, not integrity's.
    Stale,
    /// Different from the reference in either read. From one look it is
    /// provisional; a round records it only once a second look agrees
    /// with the first. The evidence event says which read.
    Divergence,
    /// Nothing could be judged: the reference or the provider did not
    /// answer, or answered at a block the other side did not have.
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Stale => "stale",
            Self::Divergence => "divergence",
            Self::Unknown => "unknown",
        }
    }
}

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use tokio::sync::watch;

use crate::{
    chain::{
        ChainReader,
        reader::{BlockAt, BlockFields, Condition, Query, Reader},
        records::{ArkivEntity, EntityKey},
    },
    config,
    pool::{Pool, Provider},
};

/// Provider reads in flight at once, the probe sweep's bound.
const CONCURRENT_READS: usize = 16;

/// The integrity checker: one round every interval over the providers
/// whose health is fine, each round two reads per provider compared
/// against the reference, and one verdict per provider recorded on its
/// entry (`docs/INTEGRITY.md`).
pub struct IntegrityChecker<R> {
    pool: Arc<Pool>,
    /// The client providers are read with, the probe client; the
    /// reference has its own reader.
    client: reqwest::Client,
    reference: R,
    config: config::Integrity,
    /// A provider answering the sample this far behind the reference's
    /// head is stale, not judged: `health.lag_tolerance_blocks`.
    lag_tolerance: u64,
    /// Provider reads are client-shaped, a block and an entity, so they
    /// get the client's timeout, not the probe's.
    attempt_timeout: Duration,
    /// The Monitor's: the first round waits for the boot window.
    ready: Arc<AtomicBool>,
    /// Keys to sample from: a page of live entities read from the
    /// reference. Written by the round only, and rounds run one after
    /// another, so a check and a later store never race; the mutex is
    /// there because the checker is shared behind `&self`.
    keys: Mutex<Vec<EntityKey>>,
}

/// One provider's verdict from one look, and the read that disagreed
/// when it is a divergence.
struct Finding {
    provider: Arc<Provider>,
    verdict: Verdict,
    disagreed: Option<Read>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    Block,
    Entity,
}

impl Read {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Entity => "entity",
        }
    }
}

/// What a provider answered, before the reference is asked.
struct Answers {
    provider: Arc<Provider>,
    block: Result<Option<BlockFields>, ()>,
    /// The rows a query by key answered, one or none from an honest
    /// node, kept as answered so the comparison is exactly what the two
    /// nodes said; and the block they were answered at.
    rows: Result<(Vec<ArkivEntity>, u64), ()>,
}

impl<R: ChainReader> IntegrityChecker<R> {
    pub fn new(
        pool: Arc<Pool>,
        client: reqwest::Client,
        reference: R,
        config: config::Integrity,
        lag_tolerance: u64,
        attempt_timeout: Duration,
        ready: Arc<AtomicBool>,
    ) -> Self {
        Self {
            pool,
            client,
            reference,
            config,
            lag_tolerance,
            attempt_timeout,
            ready,
            keys: Mutex::new(Vec::new()),
        }
    }

    /// Rounds until shutdown: the first as soon as the boot window
    /// closes, so a restart, which forgets the verdicts, does not serve
    /// a known liar for a whole interval; then every interval.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut waiting = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = waiting.tick() => {
                    if self.ready.load(Ordering::Relaxed) {
                        break;
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
        let mut tick = tokio::time::interval(self.config.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.round().await,
                _ = shutdown.changed() => return,
            }
        }
    }

    /// One round: the finalized block and the reference's head, a key,
    /// every healthy provider read at once, the reference pinned at each
    /// block the providers answered at, the comparisons, a second look
    /// at any mismatch, and a verdict per provider.
    pub async fn round(&self) {
        let providers: Vec<Arc<Provider>> = self
            .pool
            .snapshot()
            .iter()
            .filter(|provider| provider.healthy())
            .cloned()
            .collect();
        if providers.is_empty() {
            tracing::info!("integrity round: no healthy provider to check");
            return;
        }
        let Some((finalized, head)) = self.reference_state().await else {
            tracing::info!(
                providers = providers.len(),
                "integrity round: the reference could not be read, nobody is judged"
            );
            return;
        };
        self.load_keys_if_empty(head).await;
        let Some(key) = self.pick_key() else {
            tracing::info!(
                providers = providers.len(),
                "integrity round: no keys to sample yet, nobody is judged"
            );
            return;
        };

        let mut findings = self.check(&providers, &finalized, head, key).await;
        self.confirm(&mut findings, &finalized, head, key).await;
        self.record(&findings, finalized.number);
    }

    /// A divergence from one look is looked at again after a wait: a
    /// reorganisation of the tip has resolved by then, a liar has not.
    /// Twice is the divergence, named by its first read; anything else
    /// is what the second look said.
    async fn confirm(
        &self,
        findings: &mut [Finding],
        finalized: &BlockFields,
        head: u64,
        key: EntityKey,
    ) {
        let doubtful: Vec<Arc<Provider>> = findings
            .iter()
            .filter(|finding| finding.verdict == Verdict::Divergence)
            .map(|finding| finding.provider.clone())
            .collect();
        if doubtful.is_empty() {
            return;
        }
        tokio::time::sleep(self.config.confirm_after).await;
        for second in self.check(&doubtful, finalized, head, key).await {
            let first = findings
                .iter_mut()
                .find(|finding| Arc::ptr_eq(&finding.provider, &second.provider))
                .expect("the same providers were asked");
            if second.verdict != Verdict::Divergence {
                *first = second;
            }
        }
    }

    /// Every verdict onto its provider's entry, one line per provider
    /// and one per round.
    fn record(&self, findings: &[Finding], height: u64) {
        let mut counts = [0usize; 4];
        for finding in findings {
            let provider = &finding.provider;
            let verdict = finding.verdict;
            match verdict {
                Verdict::Match => {
                    tracing::debug!(provider = %provider.id, height, "integrity: match");
                }
                Verdict::Stale | Verdict::Unknown => {
                    tracing::info!(provider = %provider.id, height, verdict = verdict.as_str(), "integrity");
                }
                Verdict::Divergence => {
                    let check = finding
                        .disagreed
                        .expect("a divergence names the read that disagreed")
                        .as_str();
                    tracing::warn!(provider = %provider.id, height, check, "integrity: divergence confirmed");
                }
            }
            counts[verdict as usize] += 1;
            provider.record_integrity(verdict, height);
        }
        tracing::info!(
            height,
            matched = counts[Verdict::Match as usize],
            stale = counts[Verdict::Stale as usize],
            diverged = counts[Verdict::Divergence as usize],
            unknown = counts[Verdict::Unknown as usize],
            "integrity round"
        );
    }

    /// The reference's finalized block and its head.
    async fn reference_state(&self) -> Option<(BlockFields, u64)> {
        let head = match self.reference.block_number().await {
            Ok(head) => head,
            Err(error) => {
                tracing::warn!(%error, "integrity: the reference's head could not be read");
                return None;
            }
        };
        let finalized = match self.reference.block(BlockAt::Finalized).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                tracing::warn!("integrity: the reference has no finalized block");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "integrity: the finalized block could not be read");
                return None;
            }
        };
        Some((finalized, head))
    }

    /// Reads one page of live entities from the reference and keeps
    /// their keys, when there are none yet. The lock is never held
    /// across the read.
    async fn load_keys_if_empty(&self, head: u64) {
        if !self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            return;
        }
        let query = Query {
            conditions: vec![Condition::ExpiresAfter(head)],
            at_block: None,
        };
        match self.reference.query(&query).await {
            Ok(page) => {
                *self
                    .keys
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    page.entities.iter().map(|entity| entity.key).collect();
            }
            Err(error) => {
                tracing::warn!(%error, "integrity: the page of live entities could not be read");
            }
        }
    }

    /// One key to read this round, at random from the page: the pick
    /// only has to be one a provider cannot predict.
    fn pick_key(&self) -> Option<EntityKey> {
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if keys.is_empty() {
            return None;
        }
        Some(keys[rand::random_range(0..keys.len())])
    }

    /// Every provider read at once, then the reference once per block
    /// the providers answered the entity at, then the comparison.
    async fn check(
        &self,
        providers: &[Arc<Provider>],
        finalized: &BlockFields,
        head: u64,
        key: EntityKey,
    ) -> Vec<Finding> {
        let answers: Vec<Answers> = futures::stream::iter(providers.iter().cloned())
            .map(|provider| async move { self.ask(provider, finalized.number, key).await })
            .buffer_unordered(CONCURRENT_READS)
            .collect()
            .await;

        // The reference pinned at each distinct block, read once. Not
        // for a block the reference does not have yet, nor for one
        // further back than the lag tolerance: those answers are
        // unknown or stale before any comparison, and the read is
        // metered.
        let mut pinned: HashMap<u64, Result<Vec<ArkivEntity>, ()>> = HashMap::new();
        for answer in &answers {
            let Ok((_, block)) = &answer.rows else {
                continue;
            };
            if *block > head
                || block.saturating_add(self.lag_tolerance) < head
                || pinned.contains_key(block)
            {
                continue;
            }
            let query = Query::by_key(key).at_block(*block);
            let rows = match self.reference.query(&query).await {
                Ok(page) => Ok(page.entities),
                Err(error) => {
                    tracing::warn!(%error, block, "integrity: the reference could not be read at the provider's block");
                    Err(())
                }
            };
            pinned.insert(*block, rows);
        }

        answers
            .into_iter()
            .map(|answer| {
                let (verdict, disagreed) = self.judge(&answer, finalized, head, &pinned);
                Finding {
                    provider: answer.provider,
                    verdict,
                    disagreed,
                }
            })
            .collect()
    }

    /// One provider's two reads, as a client would make them. The reader
    /// is the shared client, which holds the connections, with this
    /// provider's URL and the attempt timeout: cheap to make, and made
    /// once per provider per look.
    async fn ask(&self, provider: Arc<Provider>, height: u64, key: EntityKey) -> Answers {
        let reader = Reader::new(
            self.client.clone(),
            provider.url.clone(),
            None,
            self.attempt_timeout,
        );
        let block = reader.block(BlockAt::Number(height)).await.map_err(|error| {
            tracing::debug!(provider = %provider.id, %error, "integrity: the block read failed");
        });
        let rows = reader
            .query(&Query::by_key(key))
            .await
            .map(|page| (page.entities, page.block))
            .map_err(|error| {
                tracing::debug!(provider = %provider.id, %error, "integrity: the entity read failed");
            });
        Answers {
            provider,
            block,
            rows,
        }
    }

    /// The outcome from what the provider and the reference answered.
    /// One look's verdict, and the read that disagreed when it is a
    /// divergence. Only both reads answered and agreed is a match;
    /// stale wins over unknown, since it says something.
    fn judge(
        &self,
        answer: &Answers,
        finalized: &BlockFields,
        head: u64,
        pinned: &HashMap<u64, Result<Vec<ArkivEntity>, ()>>,
    ) -> (Verdict, Option<Read>) {
        let block = match &answer.block {
            Err(()) => return (Verdict::Unknown, None),
            Ok(None) => return (Verdict::Stale, None),
            Ok(Some(block)) => block,
        };
        if block != finalized {
            return (Verdict::Divergence, Some(Read::Block));
        }
        let (rows, at) = match &answer.rows {
            Err(()) => return (Verdict::Unknown, None),
            Ok(answer) => answer,
        };
        if at.saturating_add(self.lag_tolerance) < head {
            return (Verdict::Stale, None);
        }
        if *at > head {
            // A block ahead of the reference: nothing to compare with
            // until the reference has it.
            return (Verdict::Unknown, None);
        }
        match pinned.get(at) {
            Some(Ok(reference)) if reference == rows => (Verdict::Match, None),
            Some(Ok(_)) => (Verdict::Divergence, Some(Read::Entity)),
            _ => (Verdict::Unknown, None),
        }
    }
}
