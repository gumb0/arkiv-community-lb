//! What an integrity round says about a provider. The checker itself
//! comes with the round; these are the words the pool entry records.

/// One provider's verdict from one round. Only a match lifts an
/// integrity quarantine; only a divergence, once confirmed, sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both reads answered and both agreed with the reference.
    Match,
    /// Behind the chain: no block at the finalized height yet. The lag
    /// path's business, not integrity's.
    Stale,
    /// Different from the reference in either read, or an answer over
    /// the size cap. From one look it is
    /// provisional; a round records it only once a second look agrees
    /// with the first. The evidence event says which read.
    Divergence,
    /// Nothing could be judged: the reference or the provider did not
    /// answer.
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
        reader::{BlockAt, BlockFields, Condition, Query, ReadError, Reader},
        records::{ArkivEntity, EntityKey},
    },
    config,
    pool::{Pool, Provider},
};

/// Provider reads in flight at once, the probe sweep's bound.
const CONCURRENT_READS: usize = 16;

/// The most bytes a provider's answer to a check may have. The honest
/// bound is far below it: a payload is at most 128 KiB, so the one
/// entity a query by key answers is under 300 KiB as hex, and a block's
/// transaction list is 66 bytes a hash. A round holds at most
/// `CONCURRENT_READS` answers at once, 64 MiB at this cap.
pub const RESPONSE_CAP: usize = 4 * 1024 * 1024;

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
/// whose health is fine, each round two reads per provider at the
/// reference's finalized height, compared against the reference's own,
/// and one verdict per provider recorded on its entry
/// (`docs/INTEGRITY.md`).
pub struct IntegrityChecker<R> {
    pool: Arc<Pool>,
    /// The client providers are read with, the probe client; the
    /// reference has its own reader.
    client: reqwest::Client,
    reference: R,
    config: config::Integrity,
    /// Provider reads are a block and an entity, heavier than a probe,
    /// so they get the client's timeout, not the probe's.
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
        /// `None` when the provider answered that it has no such block.
        provider: Option<Box<BlockFields>>,
    },
    Entity {
        key: EntityKey,
        reference: Vec<ArkivEntity>,
        provider: Vec<ArkivEntity>,
    },
    /// The answer to this read was over the size cap, so there is no
    /// answer to show.
    TooLarge { read: &'static str },
}

/// Why a provider's read gave nothing to compare.
enum ReadFailure {
    /// A transport error, a timeout, a bad status, an error envelope:
    /// unknown, since liveness is the health check's job.
    Error,
    /// An answer over the size cap: the reference's answer to the same
    /// read fits with room to spare, so this is not the data, and as
    /// unknown it would let a provider pad its way past every round.
    TooLarge,
}

/// What the reference answered this round: its finalized block, and
/// the sampled entity read at that height. Every provider is asked the
/// same two reads at the same height, so the answers are comparable as
/// they are, whatever block either side is at by then.
struct Expected {
    finalized: BlockFields,
    key: EntityKey,
    /// `Err` when the reference's entity read failed: the entity is
    /// unknown for every provider this round; the block is still judged.
    rows: Result<Vec<ArkivEntity>, ()>,
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
    keys: Vec<SampleKey>,
    read_at: Option<std::time::Instant>,
}

/// A key on the page, with the heights its entity exists between.
struct SampleKey {
    key: EntityKey,
    created_at: u64,
    expires_at: u64,
}

impl SampleKey {
    /// Whether a read pinned at `height` finds the entity: created at
    /// or before it, not expired by it.
    fn exists_at(&self, height: u64) -> bool {
        self.created_at <= height && height < self.expires_at
    }
}

/// What a provider answered.
struct Answers {
    provider: Arc<Provider>,
    block: Result<Option<BlockFields>, ReadFailure>,
    /// The rows a query by key answered, one or none from an honest
    /// node, kept as answered so the comparison is exactly what the two
    /// nodes said.
    rows: Result<Vec<ArkivEntity>, ReadFailure>,
}

impl<R: ChainReader> IntegrityChecker<R> {
    pub fn new(
        pool: Arc<Pool>,
        client: reqwest::Client,
        reference: R,
        config: config::Integrity,
        attempt_timeout: Duration,
        ready: Arc<AtomicBool>,
    ) -> Self {
        Self {
            pool,
            client,
            reference,
            config,
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

    /// One round: the reference's finalized block, a key that exists at
    /// that height, the reference's entity there, every healthy
    /// provider read at once at that height, the comparisons, a second
    /// look at any mismatch, and a verdict per provider.
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
        let Some(finalized) = self.finalized_block().await else {
            tracing::info!(
                providers = providers.len(),
                "integrity round: the reference could not be read, nobody is judged"
            );
            return;
        };
        self.load_keys_if_due(finalized.number).await;
        let Some(key) = self.pick_key(finalized.number) else {
            tracing::info!(
                providers = providers.len(),
                "integrity round: no live key to sample, nobody is judged"
            );
            return;
        };

        let rows = self.reference_rows(key, finalized.number).await;
        let expected = Expected {
            finalized,
            key,
            rows,
        };

        let mut findings = self.check(&providers, &expected).await;
        self.confirm(&mut findings, &expected).await;
        self.record(&findings, expected.finalized.number);
    }

    /// A divergence from one look is looked at again after a wait, and
    /// only a second mismatch counts: one wrong read is not a verdict.
    /// The provider is asked again; the reference's answers are the
    /// round's, since a finalized height does not change. The second
    /// look is the finding, whatever it said: a confirmed divergence is
    /// logged with the answers the verdict rests on.
    async fn confirm(&self, findings: &mut [Finding], expected: &Expected) {
        let doubtful: Vec<Arc<Provider>> = findings
            .iter()
            .filter(|finding| finding.verdict == Verdict::Divergence)
            .map(|finding| finding.provider.clone())
            .collect();
        if doubtful.is_empty() {
            return;
        }
        tokio::time::sleep(self.config.confirm_after).await;
        for second in self.check(&doubtful, expected).await {
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
                        probed_head = provider.last_height(),
                        check = "block",
                        reference_hash = %reference.hash,
                        provider_hash = served.as_ref().map(|block| format!("{:#x}", block.hash)),
                        differing = match served {
                            Some(served) => block_differences(reference, served),
                            None => "no such block".to_owned(),
                        },
                        "integrity: divergence confirmed"
                    ),
                    Evidence::Entity {
                        key,
                        reference,
                        provider: served,
                    } => tracing::warn!(
                        provider = %provider.id,
                        agreement = provider.agreement_id().map(|id| format!("{id:#x}")),
                        height,
                        probed_head = provider.last_height(),
                        check = "entity",
                        key = %key,
                        reference = describe(reference),
                        served = describe(served),
                        "integrity: divergence confirmed"
                    ),
                    Evidence::TooLarge { read } => tracing::warn!(
                        provider = %provider.id,
                        agreement = provider.agreement_id().map(|id| format!("{id:#x}")),
                        height,
                        probed_head = provider.last_height(),
                        check = read,
                        cap = RESPONSE_CAP,
                        "integrity: divergence confirmed, the answer was over the size cap"
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

    /// The reference's finalized block, the height everything in a
    /// round is read at.
    async fn finalized_block(&self) -> Option<BlockFields> {
        match self.reference.block(BlockAt::Finalized).await {
            Ok(Some(block)) => Some(block),
            Ok(None) => {
                tracing::warn!("integrity: the reference has no finalized block");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "integrity: the finalized block could not be read");
                None
            }
        }
    }

    /// Reads one page of live entities from the reference and keeps
    /// their keys with their creation and expiry heights, at the first
    /// round, once the page is a day old, and once no key on it is
    /// alive at the finalized height any more. A read that fails keeps
    /// the page as it was and is tried again at the next round. The
    /// lock is never held across the read.
    async fn load_keys_if_due(&self, height: u64) {
        let due = {
            let page = self
                .keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A page of short-lived entities has none left long before
            // the day is over, and the chain has others by then. Keys
            // too new for the height are not "none left": they are
            // there at the next round.
            let none_alive = !page.keys.iter().any(|key| key.expires_at > height);
            none_alive
                || page
                    .read_at
                    .is_none_or(|at| at.elapsed() >= self.key_page_interval)
        };
        if !due {
            return;
        }
        let query = Query {
            conditions: vec![Condition::ExpiresAfter(height)],
            at_block: None,
        };
        match self.reference.query(&query).await {
            Ok(page) => {
                let keys: Vec<SampleKey> = page
                    .entities
                    .iter()
                    .map(|entity| SampleKey {
                        key: entity.key,
                        created_at: entity.created_at,
                        expires_at: entity.expires_at,
                    })
                    .collect();
                tracing::info!(
                    keys = keys.len(),
                    height,
                    last_expiry = keys.iter().map(|key| key.expires_at).max(),
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
    /// whose entities exist at the finalized height: the pick only has
    /// to be one a provider cannot predict. Sampling a key expired by
    /// then, or created after it, would not be wrong, only useless,
    /// since both sides would answer that there is no such entity.
    fn pick_key(&self, height: u64) -> Option<EntityKey> {
        let page = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let alive: Vec<EntityKey> = page
            .keys
            .iter()
            .filter(|key| key.exists_at(height))
            .map(|key| key.key)
            .collect();
        if alive.is_empty() {
            return None;
        }
        Some(alive[rand::random_range(0..alive.len())])
    }

    /// The sampled entity as the reference has it at the finalized
    /// height: the one metered entity read of a round, which every
    /// provider's answer is compared with.
    async fn reference_rows(&self, key: EntityKey, height: u64) -> Result<Vec<ArkivEntity>, ()> {
        let query = Query::by_key(key).at_block(height);
        self.reference
            .query(&query)
            .await
            .map(|page| page.entities)
            .map_err(|error| {
                tracing::warn!(%error, height, "integrity: the reference's entity could not be read, no entity is judged");
            })
    }

    /// Every provider read at once, then the comparison.
    async fn check(&self, providers: &[Arc<Provider>], expected: &Expected) -> Vec<Finding> {
        futures::stream::iter(providers.iter().cloned())
            .map(|provider| async move { self.ask(provider, expected).await })
            .buffer_unordered(CONCURRENT_READS)
            .map(|answer| {
                let (verdict, evidence) = judge(&answer, expected);
                Finding {
                    provider: answer.provider,
                    verdict,
                    evidence,
                }
            })
            .collect()
            .await
    }

    /// One provider's two reads, both at the finalized height: the block
    /// there, and the entity pinned to it. The reader is the shared
    /// client, which holds the connections, with this provider's URL and
    /// the attempt timeout: cheap to make, and made once per provider
    /// per look.
    async fn ask(&self, provider: Arc<Provider>, expected: &Expected) -> Answers {
        let reader = Reader::new(
            self.client.clone(),
            provider.url.clone(),
            None,
            self.attempt_timeout,
        )
        .with_response_cap(RESPONSE_CAP);
        let height = expected.finalized.number;
        let block = reader
            .block(BlockAt::Number(height))
            .await
            .map_err(|error| read_failure(&provider, "block", &error));
        let rows = reader
            .query(&Query::by_key(expected.key).at_block(height))
            .await
            .map(|page| page.entities)
            .map_err(|error| read_failure(&provider, "entity", &error));
        Answers {
            provider,
            block,
            rows,
        }
    }
}

/// The outcome from what the provider and the reference answered. One
/// look's verdict, and the two answers that differed when it is a
/// divergence. Only both reads answered and agreed is a match; stale
/// wins over unknown, since it says something.
/// Logs a provider's failed read and says what it means for the verdict.
fn read_failure(provider: &Provider, read: &'static str, error: &ReadError) -> ReadFailure {
    tracing::debug!(provider = %provider.id, read, %error, "integrity: a read failed");
    match error {
        ReadError::TooLarge { .. } => ReadFailure::TooLarge,
        _ => ReadFailure::Error,
    }
}

fn judge(answer: &Answers, expected: &Expected) -> (Verdict, Option<Evidence>) {
    let finalized = &expected.finalized;
    let block = match &answer.block {
        Err(ReadFailure::Error) => return (Verdict::Unknown, None),
        Err(ReadFailure::TooLarge) => {
            return (
                Verdict::Divergence,
                Some(Evidence::TooLarge { read: "block" }),
            );
        }
        Ok(None) => {
            return match answer.provider.last_height() {
                // Its probes reported a head at or past the finalized
                // height, and it says it has no block there: it refused
                // a block older than its own head. This is not lag.
                // Counted as stale, it would let a provider avoid ever
                // being judged.
                Some(probed) if probed >= finalized.number => (
                    Verdict::Divergence,
                    Some(Evidence::Block {
                        reference: Box::new(finalized.clone()),
                        provider: None,
                    }),
                ),
                _ => (Verdict::Stale, None),
            };
        }
        Ok(Some(block)) => block,
    };
    if block != finalized {
        return (
            Verdict::Divergence,
            Some(Evidence::Block {
                reference: Box::new(finalized.clone()),
                provider: Some(Box::new(block.clone())),
            }),
        );
    }
    let rows = match &answer.rows {
        Err(ReadFailure::Error) => return (Verdict::Unknown, None),
        Err(ReadFailure::TooLarge) => {
            return (
                Verdict::Divergence,
                Some(Evidence::TooLarge { read: "entity" }),
            );
        }
        Ok(rows) => rows,
    };
    let Ok(reference) = &expected.rows else {
        return (Verdict::Unknown, None);
    };
    if same_entities(rows, reference) {
        (Verdict::Match, None)
    } else {
        (
            Verdict::Divergence,
            Some(Evidence::Entity {
                key: expected.key,
                reference: reference.clone(),
                provider: rows.clone(),
            }),
        )
    }
}

/// Whether two answers carry the same entities. Attributes are compared
/// by name, not in the order each node rendered them, so a client
/// version that renders them in another order is not a divergence.
/// Names are unique per entity, so the name alone orders them.
fn same_entities(rows: &[ArkivEntity], reference: &[ArkivEntity]) -> bool {
    fn sorted(rows: &[ArkivEntity]) -> Vec<ArkivEntity> {
        let mut rows = rows.to_vec();
        for row in &mut rows {
            row.attributes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        rows
    }
    sorted(rows) == sorted(reference)
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

    #[test]
    fn the_same_entity_with_its_attributes_in_another_order_is_a_match() {
        let attribute = |name: &str, value: &str| ArkivAttribute {
            name: name.into(),
            type_tag: "str".into(),
            value: serde_json::json!(value),
        };
        let reference_row = ArkivEntity {
            key: EntityKey::repeat_byte(0x11),
            creator: crate::chain::records::Address::repeat_byte(0x22),
            created_at: 5,
            expires_at: 900,
            payload: vec![1, 2, 3].into(),
            attributes: vec![attribute("kind", "rpc.offer"), attribute("lb", "0x01")],
        };
        let mut reordered = reference_row.clone();
        reordered.attributes.reverse();
        let mut changed = reordered.clone();
        changed.attributes[0].value = serde_json::json!("0x02");

        let finalized = BlockFields {
            number: 7,
            hash: B256::repeat_byte(0x01),
            parent_hash: B256::repeat_byte(0x02),
            state_root: B256::repeat_byte(0x03),
            transactions_root: B256::repeat_byte(0x04),
            receipts_root: B256::repeat_byte(0x05),
            transactions: vec![],
        };
        let expected = Expected {
            finalized: finalized.clone(),
            key: reference_row.key,
            rows: Ok(vec![reference_row]),
        };
        let pool = Pool::new(&[config::Provider {
            id: "node".into(),
            url: "http://127.0.0.1:1/".into(),
        }])
        .expect("a valid url");
        let provider = pool.snapshot()[0].clone();
        let answers = |row: ArkivEntity| Answers {
            provider: provider.clone(),
            block: Ok(Some(finalized.clone())),
            rows: Ok(vec![row]),
        };

        assert_eq!(judge(&answers(reordered), &expected).0, Verdict::Match);
        // The order is forgiven, a value is not.
        let (verdict, evidence) = judge(&answers(changed), &expected);
        assert_eq!(verdict, Verdict::Divergence);
        assert!(matches!(evidence, Some(Evidence::Entity { .. })));
    }
}
