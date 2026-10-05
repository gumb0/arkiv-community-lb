//! The marketplace agent: the LB's state on the chain, read back into
//! memory and kept there, and offers turned into agreements. The chain
//! is the authority; what is here is a cache of it, rebuilt at every
//! start and reconciled at every poll. One task for the writes on a
//! timer: the acceptances at the discovery poll, the refresh that
//! keeps the listing and the eligible providers' agreement records
//! alive, the flush. One more write on its own task, the extend a
//! provider is owed once its admitted tunnel has passed the probes;
//! the sidecar sends one transaction at a time, so it waits behind a
//! refresh at most.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::watch;

use crate::{
    chain::{
        ChainReader, ChainWriter,
        reader::{PAGE_LIMIT, Query, ReadError},
        records::{
            Address, Agreement, AttributeValue, Attributes, CounterRecord, CounterState, EntityKey,
            KIND_AGREEMENT, KIND_COUNTER, KIND_LB_LISTING, KIND_OFFER, LbListing, Offer, Record,
            Stored,
        },
        writer::{
            Batch, BatchResult, Create, Delete, Expiry, Extend, Identity, Operation, Patch,
            WriteError, send,
        },
    },
    config,
    integrity::IntegrityCheck,
    marketplace::admission::Agreements,
    pool::{Pool, Provider},
};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("the sidecar could not tell its identity")]
    Sidecar(#[source] WriteError),
    #[error("the chain could not be read")]
    Chain(#[source] ReadError),
    #[error(
        "{count} agreement records under this LB's key, more than the {PAGE_LIMIT} one page \
         holds: the cap keeps one LB under that, so either another LB runs with this key or \
         records were written outside the agent"
    )]
    TooManyAgreements { count: u64 },
    #[error("the listing could not be written")]
    Listing(#[source] WriteError),
    #[error(
        "{count} open counter records under this LB's key, more than the {PAGE_LIMIT} one page \
         holds: one per live agreement is expected"
    )]
    TooManyCounters { count: u64 },
}

/// One agreement's open counter record, as the chain last showed it.
/// Its count is not here: the provider entry's served count is this
/// period's count, seeded from the record when the agreement is
/// adopted, so there is one number and not two that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCounter {
    /// The counter record's own key: the entity the flush patches.
    pub key: EntityKey,
    /// The block the period counts from, which the close writes back.
    pub opened_block: u64,
}

/// The live agreements, by agreement key.
type KnownAgreements = HashMap<EntityKey, Live>;

/// What the agent knows about one live agreement: its record, its
/// provider's pool entry, and the open counter record that counts for
/// it. `counter` is `None` while the agreement has none, after a write
/// that did not land: the flush opens one.
#[derive(Debug, Clone)]
struct Live {
    agreement: Stored<Agreement>,
    /// The provider's pool entry, in the pool for as long as the
    /// agreement is in memory.
    provider: Arc<Provider>,
    counter: Option<OpenCounter>,
    /// The task waiting for this provider's probes after its tunnel
    /// was admitted, while one runs: a tunnel that reconnects before
    /// the probes pass starts no second one.
    admission_task: Option<tokio::task::AbortHandle>,
}

/// Which flush this is: the one on the timer, or the one a deliberate
/// stop makes. A stop writes the counts and nothing else, since what
/// opens, closes or deletes a record can wait for the next scheduled
/// flush and the stop should take one write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    Scheduled,
    Stop,
}

/// An agreement that has ended, as memory last had it: the record it
/// counted into, and what its provider served. Enough for the last
/// write, so what ends it needs to know nothing about records.
struct Ended {
    agreement: Stored<Agreement>,
    counter: OpenCounter,
    served: u64,
}

/// Open counter records, one per agreement, by agreement key.
type CountersByAgreement = HashMap<EntityKey, Stored<CounterRecord>>;

/// What the counter reconcile read: the record that counts for each
/// live agreement, and the ones that count for nobody.
struct Counters {
    /// The open record the LB counts into.
    open: CountersByAgreement,
    /// An agreement's younger open records: only the oldest counts,
    /// and the flush deletes these.
    duplicates: Vec<EntityKey>,
    /// Open records of agreements this LB does not have: a final write
    /// that did not land, or a record from before this start. The
    /// flush closes them.
    strays: Vec<Stored<CounterRecord>>,
}

/// Why a reconcile did not happen. At startup either is a reason not
/// to start; at a poll, a reason to skip this one.
#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("the chain could not be read")]
    Chain(#[source] ReadError),
    #[error("{count} {kind} records, more than one page holds")]
    TooMany { kind: &'static str, count: u64 },
}

pub struct Agent<R, W> {
    reader: R,
    writer: W,
    config: config::Marketplace,
    pool: Arc<Pool>,
    identity: Identity,
    listing_key: EntityKey,
    /// The listing's expiry as the start found or wrote it. A refresh
    /// does not update it: only an expiry written before this start,
    /// under another configuration, can be later than a refresh sets.
    listing_expires_at: u64,
    /// The live agreements, by key: the slot state, and the counter
    /// record each one counts into.
    agreements: Mutex<KnownAgreements>,
    /// The integrity checker, when the checks are configured: a
    /// newcomer is checked at its admission, before its extend.
    integrity: Option<Arc<dyn IntegrityCheck>>,
}

/// How often an admission looks at the entry while it waits for the
/// Monitor's probes to pass.
const ADMISSION_POLL: Duration = Duration::from_millis(200);

impl<R: ChainReader + 'static, W: ChainWriter + 'static> Agreements for Agent<R, W> {
    fn agreement(&self, key: EntityKey) -> Option<Stored<Agreement>> {
        Agent::agreement(self, key)
    }

    fn admitted(self: Arc<Self>, agreement: &Stored<Agreement>) {
        let Some(provider) = self.provider_entry(agreement.key) else {
            return;
        };
        // A reconnect is a probe due at once, as the first admission
        // was; the steps that follow are for a record that was never
        // extended, so a tunnel that reconnects after the extend
        // starts nothing and cannot make the LB write.
        provider.schedule_probe_now();
        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(live) = known.get_mut(&agreement.key) else {
            return;
        };
        if was_extended(live, self.config.agreement_life)
            || live
                .admission_task
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let task = tokio::spawn(self.clone().admission(provider, agreement.key));
        live.admission_task = Some(task.abort_handle());
    }
}

impl<R, W> std::fmt::Debug for Agent<R, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let agreements = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        f.debug_struct("Agent")
            .field("identity", &self.identity)
            .field("listing_key", &self.listing_key)
            .field("agreements", &agreements)
            .finish()
    }
}

impl<R: ChainReader, W: ChainWriter> Agent<R, W> {
    /// Reads the LB's records back from the chain and makes the listing
    /// match the configuration. Every failure here is a reason not to
    /// start: without the chain the LB does not know its own providers.
    pub async fn start(
        reader: R,
        writer: W,
        config: config::Marketplace,
        pool: Arc<Pool>,
    ) -> Result<Self, StartError> {
        let identity = writer.identity().await.map_err(StartError::Sidecar)?;
        let agent = Self {
            reader,
            writer,
            config,
            pool,
            identity,
            listing_key: EntityKey::ZERO,
            listing_expires_at: 0,
            agreements: Mutex::new(HashMap::new()),
            integrity: None,
        };
        // The two reconciles only read and can refuse the start; the
        // listing writes. This order keeps a refused start from writing
        // anything. A start is a poll with nothing remembered yet.
        let head = agent
            .reconcile_agreements()
            .await
            .map_err(|error| match error {
                ReconcileError::Chain(error) => StartError::Chain(error),
                ReconcileError::TooMany { count, .. } => StartError::TooManyAgreements { count },
            })?;
        agent
            .reconcile_counters(head)
            .await
            .map_err(|error| match error {
                ReconcileError::Chain(error) => StartError::Chain(error),
                ReconcileError::TooMany { count, .. } => StartError::TooManyCounters { count },
            })?;
        let live = agent.agreements().len();
        if live > agent.config.max_providers as usize {
            tracing::warn!(
                live,
                cap = agent.config.max_providers,
                "more agreements than the cap allows: nobody is evicted, no offer is accepted \
                 until enough expire"
            );
        }
        let head = agent
            .reader
            .block_number()
            .await
            .map_err(StartError::Chain)?;
        let (listing_key, listing_expires_at) = ensure_listing(
            &agent.reader,
            &agent.writer,
            &agent.config,
            agent.identity.address,
            head,
        )
        .await?;
        Ok(Self {
            listing_key,
            listing_expires_at,
            ..agent
        })
    }

    /// With the integrity checker, for the check at admission.
    pub fn with_integrity(mut self, integrity: Arc<dyn IntegrityCheck>) -> Self {
        self.integrity = Some(integrity);
        self
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn listing_key(&self) -> EntityKey {
        self.listing_key
    }

    /// The pool entry of a live agreement's provider.
    fn provider_entry(&self, agreement: EntityKey) -> Option<Arc<Provider>> {
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agreement)
            .map(|live| live.provider.clone())
    }

    pub fn agreement(&self, key: EntityKey) -> Option<Stored<Agreement>> {
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|live| live.agreement.clone())
    }

    /// Each live agreement's open counter record, by agreement key.
    pub fn open_counters(&self) -> HashMap<EntityKey, Option<OpenCounter>> {
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(key, live)| (*key, live.counter.clone()))
            .collect()
    }

    /// The live agreements as the agent knows them, in no particular order.
    pub fn agreements(&self) -> Vec<Stored<Agreement>> {
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|live| live.agreement.clone())
            .collect()
    }

    /// The task: a discovery poll every discovery interval and a refresh
    /// every refresh interval, until shutdown. Each interval's first
    /// tick is skipped: the start already reconciled, and the listing
    /// was just written.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let interval = |period| {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks
        };
        let mut discovery_polls = interval(self.config.discovery_interval);
        let mut refreshes = interval(self.config.refresh_interval);
        let mut flushes = interval(self.config.flush_interval);
        discovery_polls.tick().await;
        refreshes.tick().await;
        flushes.tick().await;
        loop {
            tokio::select! {
                _ = discovery_polls.tick() => self.discovery_poll().await,
                _ = refreshes.tick() => self.refresh().await,
                _ = flushes.tick() => self.flush().await,
                _ = shutdown.changed() => {
                    self.shutdown_flush().await;
                    return;
                }
            }
        }
    }

    /// One discovery poll: the two reconciles, then the offers. A poll
    /// that cannot read the chain, or finds more records than a page,
    /// changes nothing and says so; the offers are still read, so a
    /// full page of agreements does not stop acceptance, only the cap
    /// does.
    pub async fn discovery_poll(&self) {
        let head = match self.reconcile_agreements().await {
            Ok(head) => Some(head),
            Err(ReconcileError::TooMany { count, .. }) => {
                tracing::error!(
                    count,
                    "more agreement records than one page holds: this poll's reconcile is skipped"
                );
                self.reader.block_number().await.ok()
            }
            Err(ReconcileError::Chain(error)) => {
                tracing::warn!(%error, "the chain could not be read: this poll is skipped");
                None
            }
        };
        if let Some(head) = head {
            // What the reconcile read is for the flush, which reads it
            // again right before it writes. Here only its work on
            // memory matters.
            if let Err(error) = self.reconcile_counters(head).await {
                tracing::warn!(%error, "the open counter records are left as memory has them");
            }
            self.discover_offers(head).await;
        }
    }

    /// The steps after a provider's tunnel is admitted, in order: its
    /// probes pass, it is checked for integrity, then its agreement is
    /// extended.
    async fn admission(self: Arc<Self>, provider: Arc<Provider>, agreement: EntityKey) {
        // The Monitor does the probing; this only waits for its verdict,
        // at most as long as a newly accepted record lives: a node that
        // never answers is left to expire with its record.
        let deadline = std::time::Instant::now() + self.config.offer_max_lifetime;
        while !provider.healthy() {
            if std::time::Instant::now() >= deadline {
                tracing::info!(
                    provider = %provider.id,
                    agreement = %agreement,
                    "admission: the probes did not pass before the agreement record expired"
                );
                return;
            }
            tokio::time::sleep(ADMISSION_POLL).await;
        }
        // Checked now rather than at the next round, so a liar
        // serves for the seconds, not for an interval.
        // Only a divergence stops the extend, unknown verdict doesn't
        // (so a reference outage does not punish a newcomer)
        if let Some(integrity) = &self.integrity {
            integrity.check(provider.clone()).await;
            if provider.serving_wrong_data() {
                tracing::info!(
                    provider = %provider.id,
                    agreement = %agreement,
                    "admission: serving wrong data, the agreement record is not extended"
                );
                return;
            }
        }
        tracing::info!(
            provider = %provider.id,
            agreement = %agreement,
            "admission: the probes passed, the agreement record is extended"
        );
        // Extended now rather than at the hourly refresh: the refresh
        // is not aligned to the record's first life, so a provider
        // that turns healthy near the end of it would expire first.
        let extend = Extend {
            entity_key: agreement,
            expires: Expiry::Seconds(self.config.agreement_life.as_secs()),
        };
        match self.writer.extend(&extend).await {
            // Memory learns the expiry from the answer, not at the next
            // poll: until then it would still show the record as never
            // extended.
            Ok(extended) => {
                if let Some(live) = self
                    .agreements
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&agreement)
                {
                    live.agreement.expires_at = extended.expires_at;
                }
            }
            Err(error) => tracing::error!(
                %error,
                agreement = %agreement,
                "an extend at admission did not land: the next refresh extends it"
            ),
        }
    }

    /// Memory against the chain. The LB's live agreement records are
    /// read (count then page: a full page says nothing about what lies
    /// beyond it); an agreement the chain has and memory does not is
    /// adopted, one memory has and the chain does not is over: its
    /// provider leaves the pool and its slot and port are free. Expired
    /// records have vanished from the chain, so what is there is what
    /// is live. Nothing is applied on a read that failed.
    async fn reconcile_agreements(&self) -> Result<u64, ReconcileError> {
        let head = self
            .reader
            .block_number()
            .await
            .map_err(ReconcileError::Chain)?;
        let live = self.read_agreements(head).await?;
        let live_keys: HashSet<EntityKey> = live.iter().map(|stored| stored.key).collect();

        let ended = {
            let mut known = self
                .agreements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ended = self.end_agreements(&mut known, &live_keys);
            let adopted = self.adopt_agreements(&mut known, live);
            tracing::debug!(live = known.len(), adopted, "reconciled with the chain");
            ended
        };
        self.close_ended(ended, head).await;
        Ok(head)
    }

    /// This LB's live agreement records, oldest first. Count then page:
    /// a full page says nothing about what lies beyond it.
    async fn read_agreements(&self, head: u64) -> Result<Vec<Stored<Agreement>>, ReconcileError> {
        let query = Query::kind(KIND_AGREEMENT)
            .creator(self.identity.address)
            .expires_after(head);
        let count = self
            .reader
            .count(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        if count > PAGE_LIMIT {
            return Err(ReconcileError::TooMany {
                kind: "agreement",
                count,
            });
        }
        let page = self
            .reader
            .query(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        let mut live = Vec::new();
        for entity in &page.entities {
            match Stored::<Agreement>::decode(entity) {
                Ok(stored) => live.push(stored),
                Err(error) => {
                    tracing::warn!(key = %entity.key, %error, "an agreement record does not decode: skipped");
                }
            }
        }
        // By creation block: a page's own order promises nothing, and
        // an expiry moves with every refresh. Oldest first, so when two
        // records name the same provider the older one is kept.
        live.sort_by_key(|stored| (stored.created_at, stored.key));
        Ok(live)
    }

    /// The agreements memory has and the chain does not: their slots
    /// and ports are free and their providers leave the pool. Returns
    /// the ones whose counting is owed a last write.
    fn end_agreements(&self, known: &mut KnownAgreements, live: &HashSet<EntityKey>) -> Vec<Ended> {
        let mut ended = Vec::new();
        let gone: Vec<EntityKey> = known
            .keys()
            .filter(|key| !live.contains(*key))
            .copied()
            .collect();
        for key in gone {
            let Some(over) = known.remove(&key) else {
                continue;
            };
            self.pool.remove(&over.provider.id);
            tracing::info!(
                provider = %over.agreement.record.provider,
                agreement = %key,
                "agreement over: its record is gone from the chain"
            );
            // Without a counter record there is nothing to write.
            let Some(counter) = over.counter else {
                continue;
            };
            // Nothing counts for it any more, and what the entry holds
            // is the whole period, the requests since the last flush
            // included.
            ended.push(Ended {
                agreement: over.agreement,
                counter,
                served: over
                    .provider
                    .served
                    .load(std::sync::atomic::Ordering::Relaxed),
            });
        }
        ended
    }

    /// The last write the agreements that ended are owed: each counter
    /// record closed with what its provider served, or deleted when it
    /// served nothing, since a closed record at zero would be a receipt
    /// for nothing. One batch of independent operations; one that does
    /// not land leaves the record open with the count the last flush
    /// wrote, and the flush closes it as a stray.
    async fn close_ended(&self, ended: Vec<Ended>, head: u64) {
        let mut batch = Batch::new();
        for end in ended {
            let record = &end.agreement.record;
            let key = end.agreement.key;
            if end.served == 0 {
                tracing::info!(agreement = %key, counter = %end.counter.key, "the counter record is deleted at the agreement's end: it never counted");
                batch.push(Operation::Delete(Delete {
                    entity_key: end.counter.key,
                }));
                continue;
            }
            tracing::info!(agreement = %key, counter = %end.counter.key, count = end.served, "the counter record is closed at the agreement's end");
            // Built closed, from what memory holds: there is no record
            // read from the chain here to close.
            let counter = CounterRecord {
                agreement: key,
                provider: record.provider,
                state: CounterState::Closed,
                count: end.served,
                wei_per_call: record.wei_per_call,
                opened_block: end.counter.opened_block,
                closed_block: Some(head),
            };
            batch.push(Operation::Patch(patch_record(end.counter.key, &counter)));
        }
        for sent in send(&self.writer, batch).await {
            if let Err(error) = sent.result {
                tracing::error!(
                    %error,
                    operations = sent.batch.operations().len(),
                    "a final counter write did not land: the next flush closes the record"
                );
            }
        }
    }

    /// The agreements the chain has and memory does not: their
    /// providers join the pool. Returns how many, for the log.
    fn adopt_agreements(&self, known: &mut KnownAgreements, live: Vec<Stored<Agreement>>) -> usize {
        let mut adopted = 0;
        for stored in live {
            let key = stored.key;
            if let Some(held) = known.get_mut(&key) {
                // The refresh moved the expiry, and its own check leans
                // on this being current; the creation block was an
                // estimate at acceptance.
                held.agreement.created_at = stored.created_at;
                held.agreement.expires_at = stored.expires_at;
                continue;
            }
            // One agreement per provider: the oldest record won, and a
            // second one is left to expire.
            if known
                .values()
                .any(|other| other.agreement.record.provider == stored.record.provider)
            {
                tracing::warn!(
                    provider = %stored.record.provider,
                    key = %key,
                    "a second agreement record for one provider: skipped"
                );
                continue;
            }
            let provider = self.pool.add(Provider::from_marketplace(
                stored.record.provider,
                key,
                stored.record.remote_port,
            ));
            tracing::info!(provider = %stored.record.provider, agreement = %key, "agreement adopted");
            known.insert(
                key,
                Live {
                    agreement: stored,
                    provider,
                    counter: None,
                    admission_task: None,
                },
            );
            adopted += 1;
        }
        adopted
    }

    /// Matches this LB's open counter records on the chain to the
    /// agreements memory holds. Returns what it read, for the flush to
    /// write against.
    async fn reconcile_counters(&self, head: u64) -> Result<Counters, ReconcileError> {
        let records = self.read_open_counters(head).await?;
        let (oldest, duplicates) = Self::oldest_per_agreement(records);
        // A record memory held that is no longer open may have been
        // closed by a write whose answer was lost.
        self.forget_closed_counters(&oldest).await?;
        let (open, strays) = self.match_to_agreements(oldest);
        Ok(Counters {
            open,
            duplicates,
            strays,
        })
    }

    /// This LB's open counter records. Count then page: a full page
    /// says nothing about what lies beyond it.
    async fn read_open_counters(
        &self,
        head: u64,
    ) -> Result<Vec<Stored<CounterRecord>>, ReconcileError> {
        let query = Query::kind(KIND_COUNTER)
            .creator(self.identity.address)
            .attr_str("state", "open")
            .expires_after(head);
        let count = self
            .reader
            .count(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        if count > PAGE_LIMIT {
            return Err(ReconcileError::TooMany {
                kind: "open counter",
                count,
            });
        }
        let page = self
            .reader
            .query(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        let mut records = Vec::new();
        for entity in &page.entities {
            match Stored::<CounterRecord>::decode(entity) {
                Ok(stored) => records.push(stored),
                Err(error) => {
                    tracing::warn!(key = %entity.key, %error, "a counter record does not decode: skipped");
                }
            }
        }
        Ok(records)
    }

    /// Splits open counter records into the one each agreement counts
    /// into, by agreement key, and the keys of the rest.
    fn oldest_per_agreement(
        mut records: Vec<Stored<CounterRecord>>,
    ) -> (CountersByAgreement, Vec<EntityKey>) {
        // Oldest first, so an agreement's first record is the one it
        // counts into and any other is a younger duplicate.
        records.sort_by_key(creation);
        let mut oldest = HashMap::new();
        let mut duplicates = Vec::new();
        for stored in records {
            match oldest.entry(stored.record.agreement) {
                Entry::Vacant(slot) => {
                    slot.insert(stored);
                }
                // Not logged: this runs every poll, and the flush says so
                // when it deletes the duplicate.
                Entry::Occupied(_) => duplicates.push(stored.key),
            }
        }
        (oldest, duplicates)
    }

    /// Finds the records memory held that are no longer among the open
    /// ones and are closed on the chain: their counts leave the
    /// entries, and memory forgets them.
    async fn forget_closed_counters(
        &self,
        open: &CountersByAgreement,
    ) -> Result<(), ReconcileError> {
        let missing: Vec<(EntityKey, EntityKey)> = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(key, _)| !open.contains_key(*key))
            .filter_map(|(key, live)| Some((*key, live.counter.as_ref()?.key)))
            .collect();
        // Every read before memory changes, so one that fails changes
        // nothing.
        let mut closed = Vec::new();
        for (agreement, counter) in missing {
            if let Some(count) = self.closed_count(counter).await? {
                closed.push((agreement, count));
            }
        }
        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (agreement, count) in closed {
            let Some(live) = known.get_mut(&agreement) else {
                continue;
            };
            tracing::warn!(%agreement, count, "the counter record was closed by a write whose answer was lost: the next flush opens its successor");
            // The period is written, so its count leaves the entry, as
            // it does when the close is answered.
            live.provider.subtract_served(count);
            live.counter = None;
        }
        Ok(())
    }

    /// Gives each agreement in memory the open record it counts into.
    /// Returns those records by agreement key, and the records no
    /// agreement in memory took.
    fn match_to_agreements(
        &self,
        mut records: CountersByAgreement,
    ) -> (CountersByAgreement, Vec<Stored<CounterRecord>>) {
        let mut open = HashMap::new();
        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, live) in known.iter_mut() {
            let found = records.remove(key);
            match (&live.counter, &found) {
                // New to memory: the entry counts on from the count the
                // record carries, which is this period's so far. Only
                // then: counting it in twice would bill it twice.
                (None, Some(stored)) => live.provider.seed_served(stored.record.count),
                // Memory held one and the chain no longer has it. An
                // agreement that never had one is not worth a line: the
                // flush says so when it opens it.
                (Some(_), None) => {
                    tracing::warn!(agreement = %key, "the open counter record is gone from the chain: the next flush opens one");
                }
                _ => {}
            }
            // What the chain shows is what memory holds, and nothing
            // when the chain shows none.
            live.counter = found.as_ref().map(open_counter);
            if let Some(stored) = found {
                open.insert(*key, stored);
            }
        }
        (open, records.into_values().collect())
    }

    /// The count a counter record was closed with, `None` when it is
    /// not closed or no longer on the chain.
    async fn closed_count(&self, counter: EntityKey) -> Result<Option<u64>, ReconcileError> {
        let query = Query::kind(KIND_COUNTER)
            .creator(self.identity.address)
            .key(counter);
        let page = self
            .reader
            .query(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        let Some(entity) = page.entities.first() else {
            return Ok(None);
        };
        match Stored::<CounterRecord>::decode(entity) {
            Ok(stored) if stored.record.state == CounterState::Closed => {
                Ok(Some(stored.record.count))
            }
            Ok(_) => Ok(None),
            Err(error) => {
                tracing::warn!(key = %counter, %error, "a counter record does not decode: skipped");
                Ok(None)
            }
        }
    }

    /// The offers against this LB's listing, and the acceptances they
    /// earn. One page: more offers than that hides some, which is a
    /// known limit. An offer counts when it is alive, expires within
    /// `offer_max_lifetime`, names this chain, and comes from a provider
    /// with no live agreement; oldest first, one per provider, up to
    /// the free slots. The head an offer reports is not judged: it is a
    /// snapshot from posting time, and the probes decide on the node.
    async fn discover_offers(&self, head: u64) {
        let query = Query::kind(KIND_OFFER)
            .attr_key("lb_listing", self.listing_key)
            .expires_after(head)
            .expires_by(head + blocks(self.config.offer_max_lifetime));
        let page = match self.reader.query(&query).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(%error, "the offers could not be read: this poll's discovery is skipped");
                return;
            }
        };
        if page.more {
            tracing::warn!("more offers than one page holds: some are not seen");
        }
        let mut offers: Vec<Stored<Offer>> = Vec::new();
        for entity in &page.entities {
            let offer = match Stored::<Offer>::decode(entity) {
                Ok(offer) => offer,
                Err(error) => {
                    tracing::debug!(key = %entity.key, %error, "an offer does not decode: skipped");
                    continue;
                }
            };
            let specs = &offer.record.specs;
            if specs.chain_id != self.identity.chain_id {
                tracing::debug!(key = %offer.key, chain = specs.chain_id, "offer for another chain: skipped");
                continue;
            }
            offers.push(offer);
        }
        // Oldest first, by creation block: the earliest post, whatever
        // lifetime it was given.
        offers.sort_by_key(|offer| (offer.created_at, offer.key));

        let (free, taken_providers, taken_offers, taken_ports) = {
            let known = self
                .agreements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let free = (self.config.max_providers as usize).saturating_sub(known.len());
            let records = || known.values().map(|live| &live.agreement.record);
            let providers: HashSet<Address> = records().map(|record| record.provider).collect();
            let offers: HashSet<EntityKey> = records().map(|record| record.offer).collect();
            let ports: HashSet<u16> = records().map(|record| record.remote_port).collect();
            (free, providers, offers, ports)
        };
        let mut accepted_providers = taken_providers;
        let mut used_ports = taken_ports;
        let mut accepted = 0;
        for offer in &offers {
            if accepted >= free {
                tracing::info!(key = %offer.key, provider = %offer.creator, "offer waits: the cap is full");
                continue;
            }
            if taken_offers.contains(&offer.key) {
                continue;
            }
            if accepted_providers.contains(&offer.creator) {
                tracing::debug!(key = %offer.key, provider = %offer.creator, "provider already under agreement: skipped");
                continue;
            }
            // The lowest port no live agreement holds, unless the tunnel
            // server still has it bound: a client whose agreement ended
            // and that never left keeps its port, and the next holder
            // could not register on it. Such a port is skipped for this
            // poll and tried again at the next.
            let mut port = None;
            for candidate in self.config.remote_ports() {
                if used_ports.contains(&candidate) {
                    continue;
                }
                if port_is_bound(candidate).await {
                    tracing::warn!(
                        port = candidate,
                        "tunnel port bound though no agreement holds it: a stale tunnel; skipped"
                    );
                    used_ports.insert(candidate);
                    continue;
                }
                port = Some(candidate);
                break;
            }
            let Some(port) = port else {
                tracing::error!(
                    "no usable tunnel port though the cap has room: every free port is bound \
                     by a stale tunnel, or the slot state is wrong"
                );
                break;
            };
            if self.accept(offer, port, head).await {
                accepted_providers.insert(offer.creator);
                used_ports.insert(port);
                accepted += 1;
            }
        }
        tracing::info!(
            head,
            offers = offers.len(),
            accepted,
            slots_free = free.saturating_sub(accepted),
            "discovery: the offers against the listing"
        );
    }

    /// One acceptance: the agreement record, then its first counter
    /// record pointing at it, at zero. Two writes, because the counter
    /// record needs the agreement's key and a create's key is known only
    /// once it lands. An agreement whose counter record did not follow
    /// stands, and gets one at the next flush. Returns whether the
    /// port and the slot are held: they are after a landed write and
    /// after an unresolved one, which may have landed.
    async fn accept(&self, offer: &Stored<Offer>, port: u16, head: u64) -> bool {
        let agreement = Agreement {
            provider: offer.creator,
            offer: offer.key,
            wei_per_call: self.config.wei_per_call,
            remote_port: port,
        };
        // The record's first life is `offer_max_lifetime`. The offer
        // expires within that time, or it would not have been seen, so
        // the record ends at or after its offer: a provider that never
        // connects is not accepted a second time from the same offer.
        let created = match self
            .writer
            .create(&Create::new(
                agreement.encode(),
                Expiry::Seconds(self.config.offer_max_lifetime.as_secs()),
            ))
            .await
        {
            Ok(created) => created,
            Err(WriteError::Unresolved { tx_hash, .. }) => {
                // The next poll's reconcile adopts it if it landed, so the
                // port and the slot stay held until then. While the chain
                // is stalled the same offer is accepted again every poll:
                // a known limitation.
                tracing::warn!(
                    offer = %offer.key,
                    provider = %offer.creator,
                    tx = %tx_hash,
                    "acceptance unresolved: the next poll adopts it if it landed"
                );
                return true;
            }
            Err(error) => {
                tracing::error!(offer = %offer.key, provider = %offer.creator, %error, "acceptance failed");
                return false;
            }
        };
        let agreement_key = created.entity_key;
        // The create's answer carries no creation block; the head the
        // poll read is at most a few blocks early, and the reconcile
        // reads the exact one back.
        let stored = Stored {
            key: created.entity_key,
            creator: self.identity.address,
            created_at: head,
            expires_at: created.expires_at,
            record: agreement,
        };
        let provider = self.pool.add(Provider::from_marketplace(
            offer.creator,
            created.entity_key,
            port,
        ));
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                created.entity_key,
                Live {
                    agreement: stored,
                    provider,
                    counter: None,
                    admission_task: None,
                },
            );
        tracing::info!(
            provider = %offer.creator,
            agreement = %created.entity_key,
            port,
            "offer accepted"
        );

        let counter = CounterRecord {
            agreement: created.entity_key,
            provider: offer.creator,
            state: CounterState::Open,
            count: 0,
            wei_per_call: self.config.wei_per_call,
            opened_block: head,
            closed_block: None,
        };
        let open = match self
            .writer
            .create(&Create::new(
                counter.encode(),
                Expiry::Seconds(self.config.counter_record_life.as_secs()),
            ))
            .await
        {
            Ok(created) => {
                tracing::info!(agreement = %agreement_key, counter = %created.entity_key, "counter record opened");
                Some(OpenCounter {
                    key: created.entity_key,
                    opened_block: head,
                })
            }
            Err(error) => {
                tracing::warn!(
                    agreement = %agreement_key,
                    %error,
                    "the counter record did not follow the agreement: the next flush opens one"
                );
                None
            }
        };
        if let Some(live) = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&agreement_key)
        {
            live.counter = open;
        }
        true
    }

    /// One refresh: the listing and the eligible providers' agreement
    /// records extended, in one batch.
    pub async fn refresh(&self) {
        // A failed read of either does not stop the refresh: the
        // reference being down is not the sidecar being down.
        let balance = self.gas_balance().await;
        let head = match self.reader.block_number().await {
            Ok(head) => Some(head),
            Err(error) => {
                tracing::warn!(%error, "the head could not be read: no record is left out for its expiry");
                None
            }
        };
        let RefreshBatch {
            batch,
            ineligible,
            expiring_later,
        } = self.refresh_batch(head);
        let extends = batch.operations().len();

        let mut landed = 0;
        for sent in send(&self.writer, batch).await {
            match sent.result {
                Ok(result) => landed += result.extended_entities.len(),
                Err(error) => {
                    tracing::error!(
                        %error,
                        extends = sent.batch.operations().len(),
                        balance = balance.map(glm).unwrap_or_else(|| "unknown".to_owned()),
                        "a refresh did not land: its records are extended at the next one"
                    );
                }
            }
        }
        tracing::info!(
            extended = landed,
            of = extends,
            ineligible,
            expiring_later,
            "refresh: the listing and each eligible provider's record"
        );
    }

    /// The LB key's balance, with a warning when it is low; `None`
    /// when it could not be read.
    async fn gas_balance(&self) -> Option<alloy_primitives::U256> {
        match self.reader.balance(self.identity.address).await {
            Ok(balance) => {
                // A dry key is refused before execution and every write
                // stops, so a low balance is worth a warning before it
                // gets there.
                if balance < self.config.gas_warn_below.0 {
                    tracing::warn!(
                        balance = %glm(balance),
                        floor = %glm(self.config.gas_warn_below.0),
                        "the LB key is low on GLM: writes stop when it runs out"
                    );
                }
                Some(balance)
            }
            Err(error) => {
                tracing::warn!(%error, "the LB key's balance could not be read");
                None
            }
        }
    }

    /// The extends one refresh makes, and how many records it left out.
    /// `head` is for leaving records out by their expiry: a batch is
    /// one transaction, so one extend the chain refuses fails every
    /// other with it.
    fn refresh_batch(&self, head: Option<u64>) -> RefreshBatch {
        // An expiry can only be moved later. A record written under a
        // longer lifetime than is configured now already expires later
        // than this refresh would set, so its extend would be refused.
        // The write lands in a block after `head`, so an expiry up to
        // `head` plus the lifetime is still moved later by it.
        let expires_later = |expires_at: u64, life: Duration| {
            head.is_some_and(|head| expires_at > head + blocks(life))
        };
        let mut batch = Batch::new();
        let mut expiring_later = 0;
        if expires_later(self.listing_expires_at, self.config.listing_life) {
            expiring_later += 1;
        } else {
            batch.push(Operation::Extend(Extend {
                entity_key: self.listing_key,
                expires: Expiry::Seconds(self.config.listing_life.as_secs()),
            }));
        }
        // Eligibility at this moment is the one rule: an ineligible
        // provider is skipped, so its record expires `agreement_life`
        // after its last refresh, or with its first life if it never
        // passed a probe.
        let mut ineligible = 0;
        let known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, live) in known.iter() {
            let agreement = &live.agreement;
            if !live.provider.eligible() {
                ineligible += 1;
                continue;
            }
            // An extend of a record that is gone is refused too.
            if head.is_some_and(|head| agreement.expires_at <= head) {
                tracing::warn!(
                    agreement = %key,
                    provider = %agreement.record.provider,
                    "an eligible provider's agreement record has expired: not refreshed, \
                     the next poll drops it"
                );
                continue;
            }
            if expires_later(agreement.expires_at, self.config.agreement_life) {
                expiring_later += 1;
                continue;
            }
            batch.push(Operation::Extend(Extend {
                entity_key: *key,
                expires: Expiry::Seconds(self.config.agreement_life.as_secs()),
            }));
        }
        RefreshBatch {
            batch,
            ineligible,
            expiring_later,
        }
    }
}

/// Whether an agreement's record was extended: its life, expiry less
/// creation, is at least `agreement_life`, which an extend sets from
/// the moment it lands. An accepted record lives `offer_max_lifetime`,
/// shorter by configuration; its creation block is an estimate until
/// the reconcile reads it back, a few blocks early at most.
fn was_extended(live: &Live, agreement_life: Duration) -> bool {
    let record = &live.agreement;
    record.expires_at.saturating_sub(record.created_at) >= blocks(agreement_life)
}

/// What one refresh sends, and the records it leaves out, by reason.
struct RefreshBatch {
    batch: Batch,
    /// Agreements whose provider is not eligible at this moment.
    ineligible: usize,
    /// Records that already expire later than the refresh would set.
    expiring_later: usize,
}

/// What one flush's batch landed: the records the node reports
/// patched, and how many it created.
#[derive(Default)]
struct Landed {
    patched: Vec<EntityKey>,
    created: usize,
    deleted: usize,
}

/// A record whose settlement period is over, as the closing write
/// leaves it: which agreement it belongs to, and the count that write
/// carried.
struct Closing {
    agreement: EntityKey,
    counter: EntityKey,
    count: u64,
}

impl<R: ChainReader, W: ChainWriter> Agent<R, W> {
    /// One flush: what each provider served, into its agreement's
    /// counter record.
    pub async fn flush(&self) {
        self.flush_with(Flush::Scheduled).await;
    }

    /// The flush a deliberate stop makes, before the task returns: the
    /// counts alone, so a restart loses none of them and the stop
    /// waits for one write. The wait is for the chain's receipt, up to
    /// a few minutes; the stop grace in the compose file covers it.
    pub async fn shutdown_flush(&self) {
        tracing::info!("stopping: the counts go to the chain first, which can take a few minutes");
        self.flush_with(Flush::Stop).await;
        tracing::info!("stopping: the counts are written");
    }

    async fn flush_with(&self, flush: Flush) {
        let head = match self.reader.block_number().await {
            Ok(head) => head,
            Err(error) => {
                tracing::warn!(%error, "the head could not be read: this flush is skipped");
                return;
            }
        };
        // A batch is one transaction, and a key that has gone since the
        // last poll fails every other count in it, so the records are
        // read again here, right before they are written.
        let counters = match self.reconcile_counters(head).await {
            Ok(counters) => counters,
            Err(error) => {
                tracing::warn!(%error, "the open counter records could not be read: this flush is skipped");
                return;
            }
        };

        let (counts, closing) = self.counts_batch(&counters, head, flush);
        let landed = self.send_flush(counts).await;
        // A successor is only right once its predecessor is closed:
        // sent together, a close that did not land would leave the
        // agreement with two open records.
        let successors = self.end_periods(&closing, &landed, head);
        let closed = successors.operations().len();
        let followed = self.send_flush(successors).await;

        tracing::info!(
            patched = landed.patched.len() - closed,
            closed,
            opened = landed.created + followed.created,
            deleted = landed.deleted,
            "flush"
        );
    }

    /// The first batch: what every open counter record is owed, and
    /// the closes it asks for, which the second batch follows.
    fn counts_batch(&self, counters: &Counters, head: u64, flush: Flush) -> (Batch, Vec<Closing>) {
        let period = blocks(self.config.settlement_period);
        let scheduled = flush == Flush::Scheduled;
        let mut batch = Batch::new();
        let mut closing = Vec::new();
        // Find the two kinds of record no live agreement counts into.
        // Only done for scheduled flushes, a stop leaves them for the next flush.
        if scheduled {
            // An agreement counts into one record, so a second open one
            // is deleted. It carries nothing: a count is only ever
            // written into the record this LB counts into, the oldest.
            for duplicate in &counters.duplicates {
                tracing::info!(counter = %duplicate, "a second open counter record for one agreement: deleted");
                batch.push(Operation::Delete(Delete {
                    entity_key: *duplicate,
                }));
            }
            // A record nobody counts for any more: its agreement ended
            // and the write that should have closed it did not land, or
            // it predates this start. Closed with the count it holds,
            // which is what its last flush wrote, or deleted when it
            // never counted.
            for stray in &counters.strays {
                if stray.record.count == 0 {
                    tracing::info!(agreement = %stray.record.agreement, counter = %stray.key, "a counter record of an agreement that is gone: deleted, it never counted");
                    batch.push(Operation::Delete(Delete {
                        entity_key: stray.key,
                    }));
                } else {
                    tracing::info!(agreement = %stray.record.agreement, counter = %stray.key, count = stray.record.count, "a counter record of an agreement that is gone: closed");
                    batch.push(Operation::Patch(patch_record(
                        stray.key,
                        &closed(&stray.record, stray.record.count, head),
                    )));
                }
            }
        }
        let known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, live) in known.iter() {
            let record = &live.agreement.record;
            let served = live
                .provider
                .served
                .load(std::sync::atomic::Ordering::Relaxed);
            match counters.open.get(key) {
                // A period is over once the head has passed the
                // record's opening by one. A record that counted
                // nothing is not closed: it waits for a count.
                // Skipped for the shutdown (not scheduled) flush.
                Some(stored)
                    if scheduled && served > 0 && head >= stored.record.opened_block + period =>
                {
                    tracing::info!(agreement = %key, counter = %stored.key, count = served, "the settlement period is over: the counter record is closed");
                    batch.push(Operation::Patch(patch_record(
                        stored.key,
                        &closed(&stored.record, served, head),
                    )));
                    closing.push(Closing {
                        agreement: *key,
                        counter: stored.key,
                        count: served,
                    });
                }
                // The chain already says what the entry counted.
                Some(stored) if stored.record.count == served => {}
                // The record is a copy of the entry's count, not a
                // running total, so the write can be repeated freely.
                Some(stored) => {
                    batch.push(Operation::Patch(patch_record(
                        stored.key,
                        &counted(stored, served),
                    )));
                }
                // At a stop there is nowhere to write this count, and
                // opening a record would not carry it: what this
                // agreement served since the last write is lost.
                None if flush == Flush::Stop => {
                    if served > 0 {
                        tracing::warn!(agreement = %key, provider = %record.provider, count = served, "stopping: this agreement has no counter record, and what it served is lost");
                    }
                }
                // A fresh record opens at zero and its count follows at
                // the next flush. Opening it with a count would be
                // unsafe: a create whose answer is lost leaves a record
                // this LB does not know it has, and the next read would
                // count what it carries into the entry a second time.
                None => {
                    let counter = CounterRecord {
                        agreement: *key,
                        provider: record.provider,
                        state: CounterState::Open,
                        count: 0,
                        wei_per_call: record.wei_per_call,
                        opened_block: head,
                        closed_block: None,
                    };
                    tracing::info!(agreement = %key, "a counter record is opened at the flush: the next one writes its count");
                    batch.push(Operation::Create(Create::new(
                        counter.encode(),
                        Expiry::Seconds(self.config.counter_record_life.as_secs()),
                    )));
                }
            }
        }
        (batch, closing)
    }

    /// The second batch: a successor for every close that landed. A
    /// close that did not land changes nothing and is made again at
    /// the next flush; one that landed without its answer is found
    /// closed by the next counter reconcile.
    fn end_periods(&self, closing: &[Closing], landed: &Landed, head: u64) -> Batch {
        let mut successors = Batch::new();
        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for close in closing {
            if !landed.patched.contains(&close.counter) {
                continue;
            }
            let Some(live) = known.get_mut(&close.agreement) else {
                continue;
            };
            let record = &live.agreement.record;
            // The period is written, so its count is no longer the
            // entry's. Subtracted, not cleared: a request served while
            // the write was in flight belongs to the next period.
            live.provider.subtract_served(close.count);
            live.counter = None;
            successors.push(Operation::Create(Create::new(
                CounterRecord {
                    agreement: close.agreement,
                    provider: record.provider,
                    state: CounterState::Open,
                    count: 0,
                    wei_per_call: record.wei_per_call,
                    opened_block: head,
                    closed_block: None,
                }
                .encode(),
                Expiry::Seconds(self.config.counter_record_life.as_secs()),
            )));
        }
        successors
    }

    /// Sends one of the flush's batches and gathers what landed. Every
    /// operation in a batch stands on its own, so a part that fails
    /// leaves the rest.
    async fn send_flush(&self, batch: Batch) -> Landed {
        let mut landed = Landed::default();
        if batch.is_empty() {
            return landed;
        }
        for sent in send(&self.writer, batch).await {
            match sent.result {
                Ok(result) => {
                    warn_unnamed_patches(&sent.batch, &result);
                    landed.patched.extend(result.patched_entities);
                    landed.created += result.created_entities.len();
                    landed.deleted += result.deleted_entities.len();
                }
                Err(error) => {
                    tracing::error!(
                        %error,
                        operations = sent.batch.operations().len(),
                        "a flush batch was not answered as landed: the next flush writes against what the chain shows"
                    );
                }
            }
        }
        landed
    }
}

/// Whether a period's close landed is decided by the keys the answer
/// names. A batch is one transaction, so every patch in it applied,
/// and an answer naming fewer is the node or the sidecar changing
/// shape under us. Said out loud, because what follows from it is
/// silent: a close taken for lost gets no successor until the next
/// counter reconcile finds the record closed, at every period's end.
fn warn_unnamed_patches(batch: &Batch, result: &BatchResult) {
    let patches = batch
        .operations()
        .iter()
        .filter(|operation| matches!(operation, Operation::Patch(_)))
        .count();
    if result.patched_entities.len() != patches {
        tracing::warn!(
            named = result.patched_entities.len(),
            patches,
            "the answer to a flush batch does not name every record it patched: a settlement period closed in it gets its successor a flush late"
        );
    }
}

/// A counter record as it should stand. A patch writes the whole
/// payload, and a record being closed also takes the state attribute,
/// which is what settle reads records by.
fn patch_record(key: EntityKey, record: &CounterRecord) -> Patch {
    Patch {
        entity_key: key,
        set: matches!(record.state, CounterState::Closed)
            .then(|| Attributes::default().with("state", AttributeValue::Str("closed".to_owned()))),
        payload: Some(record.encode().payload),
    }
}

/// The same record with a new count.
fn counted(stored: &Stored<CounterRecord>, count: u64) -> CounterRecord {
    CounterRecord {
        count,
        ..stored.record.clone()
    }
}

/// The same record with its final count and the block it closed at.
fn closed(record: &CounterRecord, count: u64, head: u64) -> CounterRecord {
    CounterRecord {
        count,
        state: CounterState::Closed,
        closed_block: Some(head),
        ..record.clone()
    }
}

/// An amount of wei as GLM, for logs: "0.0199", the trailing zeros cut.
fn glm(wei: alloy_primitives::U256) -> String {
    let text = alloy_primitives::utils::format_ether(wei);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Whether something listens on this port on loopback, where the tunnel
/// server binds the forwarded ports. A connect on loopback is answered
/// or refused at once. A timeout, which a firewall dropping loopback
/// packets would cause, counts as free: counted as bound, it would
/// make every port look taken and nothing would ever be accepted.
async fn port_is_bound(port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    matches!(
        tokio::time::timeout(Duration::from_millis(200), connect).await,
        Ok(Ok(_))
    )
}

fn open_counter(stored: &Stored<CounterRecord>) -> OpenCounter {
    OpenCounter {
        key: stored.key,
        opened_block: stored.record.opened_block,
    }
}

/// When a record was written, for ordering: the creation block, and
/// the key between two records written in the same one.
fn creation(stored: &Stored<CounterRecord>) -> (u64, EntityKey) {
    (stored.created_at, stored.key)
}

/// A lifetime in blocks, the way the sidecar converts it.
fn blocks(lifetime: Duration) -> u64 {
    lifetime.as_secs() / 2
}

/// One live listing that says what the configuration says: created if
/// there is none, patched if it differs. When there are several, the
/// oldest is the LB's: it is the one offers have been pointing at the
/// longest. Returns its key and its expiry.
async fn ensure_listing<R: ChainReader, W: ChainWriter>(
    reader: &R,
    writer: &W,
    config: &config::Marketplace,
    lb: Address,
    head: u64,
) -> Result<(EntityKey, u64), StartError> {
    let desired = LbListing {
        wei_per_call: config.wei_per_call,
        tunnel_server: config.tunnel_server.clone(),
        max_providers: config.max_providers,
    };
    let query = Query::kind(KIND_LB_LISTING).creator(lb).expires_after(head);
    let page = reader.query(&query).await.map_err(StartError::Chain)?;
    let mut listings: Vec<Stored<LbListing>> = page
        .entities
        .iter()
        .filter_map(|entity| match Stored::<LbListing>::decode(entity) {
            Ok(stored) => Some(stored),
            Err(error) => {
                tracing::warn!(key = %entity.key, %error, "a listing record does not decode: skipped");
                None
            }
        })
        .collect();
    // Oldest first, by creation block, not by expiry: the kept listing
    // is refreshed and its expiry moves ahead of an extra's.
    listings.sort_by_key(|listing| (listing.created_at, listing.key));
    let mut listings = listings.into_iter();
    let Some(kept) = listings.next() else {
        let created = writer
            .create(&Create::new(
                desired.encode(),
                Expiry::Seconds(config.listing_life.as_secs()),
            ))
            .await
            .map_err(StartError::Listing)?;
        tracing::info!(key = %created.entity_key, listing = %desired, "listing created");
        return Ok((created.entity_key, created.expires_at));
    };
    for extra in listings {
        tracing::warn!(key = %extra.key, "an extra listing under this LB's key: ignored");
    }
    if kept.record == desired {
        tracing::info!(key = %kept.key, listing = %kept.record, "listing unchanged");
    } else {
        writer
            .patch(&Patch {
                entity_key: kept.key,
                set: None,
                payload: Some(desired.encode().payload),
            })
            .await
            .map_err(StartError::Listing)?;
        tracing::info!(
            key = %kept.key,
            was = %kept.record,
            listing = %desired,
            "listing updated to the configuration"
        );
    }
    Ok((kept.key, kept.expires_at))
}

#[cfg(test)]
mod tests {
    use super::glm;
    use alloy_primitives::U256;

    #[test]
    fn wei_reads_as_glm_in_logs() {
        assert_eq!(glm(U256::ZERO), "0");
        assert_eq!(glm(U256::from(20_000_000_000_000_000u64)), "0.02");
        assert_eq!(glm(U256::from(1_000_000_000_000_000_000u64)), "1");
        assert_eq!(glm(U256::from(1_234_500_000_000_000_000u128)), "1.2345");
        assert_eq!(glm(U256::from(1u64)), "0.000000000000000001");
    }
}
