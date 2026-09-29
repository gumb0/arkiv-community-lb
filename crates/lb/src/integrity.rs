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
    future::Future,
    pin::Pin,
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

/// How often the page of keys to sample from is read again. A page
/// read costs the reference as much as a sample, a page goes stale
/// only as its entities expire, which the pick already skips, and two
/// hundred keys hold long-lived records enough for a day.
const KEY_PAGE_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// One provider checked on demand, the way a round checks the fleet:
/// what the marketplace agent holds to check a newcomer at its
/// admission, without the checker's reference type in its signature.
pub trait IntegrityCheck: Send + Sync {
    /// The future comes boxed: a trait object cannot return the unnamed
    /// type an `async fn` would, and one allocation per admission is
    /// nothing.
    fn check(&self, provider: Arc<Provider>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

impl<R: ChainReader + 'static> IntegrityCheck for IntegrityChecker<R> {
    fn check(&self, provider: Arc<Provider>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Pinned because a future must not move once polled.
        Box::pin(self.round_over(vec![provider]))
    }
}

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
    /// Keys to sample from, each with its entity's expiry: a page of
    /// live entities read from the reference, and when it was read.
    /// Written by the round only, and rounds run one after another, so
    /// a check and a later store never race; the mutex is there because
    /// the checker is shared behind `&self`.
    keys: Mutex<KeyPage>,
    /// How long a page is kept before it is read again: a day, unless
    /// a test shortens it.
    key_page_interval: Duration,
}

/// One provider's verdict from one look, and the two answers that
/// disagreed when it is a divergence.
struct Finding {
    provider: Arc<Provider>,
    verdict: Verdict,
    evidence: Option<Evidence>,
}

/// What the reference and the provider answered where they differed:
/// the evidence a confirmed divergence is logged with.
enum Evidence {
    Block {
        reference: Box<BlockFields>,
        provider: Box<BlockFields>,
    },
    Entity {
        key: EntityKey,
        /// The block the provider answered at, and the reference was
        /// pinned to.
        at: u64,
        reference: Vec<ArkivEntity>,
        provider: Vec<ArkivEntity>,
    },
}

/// The fields of two blocks that differ, each with both values, as a
/// log field: a divergence can be in any compared field, and the ones
/// that agree would say nothing.
fn block_differences(reference: &BlockFields, provider: &BlockFields) -> String {
    let mut differing = Vec::new();
    let mut field = |name: &str, ours: &dyn std::fmt::Display, theirs: &dyn std::fmt::Display| {
        differing.push(format!("{name} reference={ours} provider={theirs}"));
    };
    if reference.hash != provider.hash {
        field("hash", &reference.hash, &provider.hash);
    }
    if reference.parent_hash != provider.parent_hash {
        field("parentHash", &reference.parent_hash, &provider.parent_hash);
    }
    if reference.state_root != provider.state_root {
        field("stateRoot", &reference.state_root, &provider.state_root);
    }
    if reference.transactions_root != provider.transactions_root {
        field(
            "transactionsRoot",
            &reference.transactions_root,
            &provider.transactions_root,
        );
    }
    if reference.receipts_root != provider.receipts_root {
        field(
            "receiptsRoot",
            &reference.receipts_root,
            &provider.receipts_root,
        );
    }
    if reference.transactions != provider.transactions {
        let (ours, theirs) = (&reference.transactions, &provider.transactions);
        let first = ours
            .iter()
            .zip(theirs)
            .position(|(a, b)| a != b)
            .unwrap_or(ours.len().min(theirs.len()));
        differing.push(format!(
            "transactions reference={} provider={} first difference at {first}",
            ours.len(),
            theirs.len()
        ));
    }
    differing.join("; ")
}

/// Entity rows as a log field: every attribute in full, the payload as
/// its hash, since a payload can be large and a hash says whether two
/// differ. No rows is the answer "no such entity".
fn describe(rows: &[ArkivEntity]) -> String {
    if rows.is_empty() {
        return "no such entity".to_owned();
    }
    rows.iter()
        .map(|row| {
            let attributes: Vec<String> = row
                .attributes
                .iter()
                .map(|attribute| {
                    format!(
                        "{}:{}={}",
                        attribute.name, attribute.type_tag, attribute.value
                    )
                })
                .collect();
            format!(
                "creator {:#x} expires {} attributes [{}] payload hash {:#x}",
                row.creator,
                row.expires_at,
                attributes.join(", "),
                alloy_primitives::keccak256(&row.payload)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The keys to sample from and when the page was read; `read_at` is
/// `None` before the first read.
#[derive(Default)]
struct KeyPage {
    keys: Vec<(EntityKey, u64)>,
    read_at: Option<std::time::Instant>,
}

/// What a provider answered, before the reference is asked.
struct Answers {
    provider: Arc<Provider>,
    /// The key it was asked for.
    key: EntityKey,
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
            keys: Mutex::new(KeyPage::default()),
            key_page_interval: KEY_PAGE_INTERVAL,
        }
    }

    /// The page of keys read again this often instead of daily. For
    /// tests, which cannot wait a day.
    pub fn with_key_page_interval(mut self, interval: Duration) -> Self {
        self.key_page_interval = interval;
        self
    }

    /// Rounds until shutdown: the first as soon as the boot window
    /// closes, so a restart, which forgets the verdicts, does not serve
    /// a known liar for a whole interval; then every interval.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
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
        self.round_over(providers).await;
    }

    /// A round over the given providers: the whole healthy fleet on the
    /// timer, one newcomer at its admission.
    async fn round_over(&self, providers: Vec<Arc<Provider>>) {
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
        self.load_keys_if_due(head).await;
        let Some(key) = self.pick_key(head) else {
            tracing::info!(
                providers = providers.len(),
                "integrity round: no live key to sample, nobody is judged"
            );
            return;
        };

        let mut findings = self.check(&providers, &finalized, head, key).await;
        self.confirm(&mut findings, &finalized, head, key).await;
        self.record(&findings, finalized.number);
    }

    /// A divergence from one look is looked at again after a wait: a
    /// reorganisation of the tip has resolved by then, a liar has not.
    /// The second look is the finding, whatever it said: a confirmed
    /// divergence is logged with the answers the verdict rests on.
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
            *first = second;
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
                // The evidence is the event: both answers side by side,
                // held at the moment of the verdict and logged whole.
                Verdict::Divergence => match finding
                    .evidence
                    .as_ref()
                    .expect("a divergence carries the answers that differed")
                {
                    Evidence::Block {
                        reference,
                        provider: served,
                    } => tracing::warn!(
                        provider = %provider.id,
                        agreement = provider.agreement_id().map(|id| format!("{id:#x}")),
                        height,
                        check = "block",
                        reference_hash = %reference.hash,
                        provider_hash = %served.hash,
                        differing = block_differences(reference, served),
                        "integrity: divergence confirmed"
                    ),
                    Evidence::Entity {
                        key,
                        at,
                        reference,
                        provider: served,
                    } => tracing::warn!(
                        provider = %provider.id,
                        agreement = provider.agreement_id().map(|id| format!("{id:#x}")),
                        height,
                        check = "entity",
                        key = %key,
                        at,
                        reference = describe(reference),
                        served = describe(served),
                        "integrity: divergence confirmed"
                    ),
                },
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
    /// their keys and expiries, at the first round, once the page is a
    /// day old, and once no key on it is alive any more. A read that
    /// fails keeps the page as it was and is tried again at the next
    /// round. The lock is never held across the read.
    async fn load_keys_if_due(&self, head: u64) {
        let due = {
            let page = self
                .keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A page of short-lived entities has none left long before
            // the day is over, and the chain has others by then.
            let none_alive = !page.keys.iter().any(|(_, expires_at)| *expires_at > head);
            none_alive
                || page
                    .read_at
                    .is_none_or(|at| at.elapsed() >= self.key_page_interval)
        };
        if !due {
            return;
        }
        let query = Query {
            conditions: vec![Condition::ExpiresAfter(head)],
            at_block: None,
        };
        match self.reference.query(&query).await {
            Ok(page) => {
                let keys: Vec<(EntityKey, u64)> = page
                    .entities
                    .iter()
                    .map(|entity| (entity.key, entity.expires_at))
                    .collect();
                tracing::info!(
                    keys = keys.len(),
                    head,
                    last_expiry = keys.iter().map(|(_, expires_at)| *expires_at).max(),
                    "integrity: the page of live entities was read"
                );
                *self
                    .keys
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = KeyPage {
                    keys,
                    read_at: Some(std::time::Instant::now()),
                };
            }
            Err(error) => {
                tracing::warn!(%error, "integrity: the page of live entities could not be read");
            }
        }
    }

    /// One key to read this round, at random among the page's keys
    /// whose entities are still alive at the head: the pick only has
    /// to be one a provider cannot predict. Sampling an expired key
    /// would not be wrong, only useless, since both sides would answer
    /// that there is no such entity.
    fn pick_key(&self, head: u64) -> Option<EntityKey> {
        let page = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let alive: Vec<EntityKey> = page
            .keys
            .iter()
            .filter(|(_, expires_at)| *expires_at > head)
            .map(|(key, _)| *key)
            .collect();
        if alive.is_empty() {
            return None;
        }
        Some(alive[rand::random_range(0..alive.len())])
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
                let (verdict, evidence) = self.judge(&answer, finalized, head, &pinned);
                Finding {
                    provider: answer.provider,
                    verdict,
                    evidence,
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
            key,
            block,
            rows,
        }
    }

    /// The outcome from what the provider and the reference answered.
    /// One look's verdict, and the two answers that differed when it is
    /// a divergence. Only both reads answered and agreed is a match;
    /// stale wins over unknown, since it says something.
    fn judge(
        &self,
        answer: &Answers,
        finalized: &BlockFields,
        head: u64,
        pinned: &HashMap<u64, Result<Vec<ArkivEntity>, ()>>,
    ) -> (Verdict, Option<Evidence>) {
        let block = match &answer.block {
            Err(()) => return (Verdict::Unknown, None),
            Ok(None) => return (Verdict::Stale, None),
            Ok(Some(block)) => block,
        };
        if block != finalized {
            return (
                Verdict::Divergence,
                Some(Evidence::Block {
                    reference: Box::new(finalized.clone()),
                    provider: Box::new(block.clone()),
                }),
            );
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
            Some(Ok(reference)) => (
                Verdict::Divergence,
                Some(Evidence::Entity {
                    key: answer.key,
                    at: *at,
                    reference: reference.clone(),
                    provider: rows.clone(),
                }),
            ),
            _ => (Verdict::Unknown, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::records::ArkivAttribute;
    use alloy_primitives::B256;

    #[test]
    fn block_evidence_names_only_the_fields_that_differ() {
        let reference = BlockFields {
            number: 7,
            hash: B256::repeat_byte(0x01),
            parent_hash: B256::repeat_byte(0x02),
            state_root: B256::repeat_byte(0x03),
            transactions_root: B256::repeat_byte(0x04),
            receipts_root: B256::repeat_byte(0x05),
            transactions: vec![B256::repeat_byte(0x10), B256::repeat_byte(0x11)],
        };
        let mut provider = reference.clone();
        provider.receipts_root = B256::repeat_byte(0x55);
        provider.transactions[1] = B256::repeat_byte(0x99);
        let differing = block_differences(&reference, &provider);
        assert!(
            differing.contains("receiptsRoot reference=0x0505"),
            "{differing}"
        );
        assert!(differing.contains("provider=0x5555"), "{differing}");
        assert!(
            differing.contains("transactions reference=2 provider=2 first difference at 1"),
            "{differing}"
        );
        assert!(
            !differing.contains("stateRoot"),
            "an equal field is not named: {differing}"
        );
        assert!(!differing.contains("hash reference"), "{differing}");
    }

    #[test]
    fn a_transaction_list_cut_short_differs_where_it_ends() {
        let reference = BlockFields {
            number: 7,
            hash: B256::repeat_byte(0x01),
            parent_hash: B256::repeat_byte(0x02),
            state_root: B256::repeat_byte(0x03),
            transactions_root: B256::repeat_byte(0x04),
            receipts_root: B256::repeat_byte(0x05),
            transactions: vec![B256::repeat_byte(0x10), B256::repeat_byte(0x11)],
        };
        let mut provider = reference.clone();
        provider.transactions.pop();
        assert_eq!(
            block_differences(&reference, &provider),
            "transactions reference=2 provider=1 first difference at 1"
        );
        assert_eq!(block_differences(&reference, &reference), "");
    }

    #[test]
    fn evidence_names_every_attribute_and_hashes_the_payload() {
        let row = ArkivEntity {
            key: EntityKey::repeat_byte(0x11),
            creator: crate::chain::records::Address::repeat_byte(0x22),
            created_at: 5,
            expires_at: 900,
            payload: vec![1, 2, 3].into(),
            attributes: vec![ArkivAttribute {
                name: "kind".into(),
                type_tag: "str".into(),
                value: serde_json::json!("rpc.offer"),
            }],
        };
        let described = describe(std::slice::from_ref(&row));
        assert!(described.contains("kind:str=\"rpc.offer\""), "{described}");
        assert!(described.contains("expires 900"), "{described}");
        assert!(
            described.contains(&format!("{:#x}", alloy_primitives::keccak256([1u8, 2, 3]))),
            "{described}"
        );
        assert!(
            !described.contains("[1, 2, 3]"),
            "the payload itself is not logged"
        );
        assert_eq!(describe(&[]), "no such entity");
    }
}
