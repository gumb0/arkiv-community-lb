//! The marketplace agent: the LB's state on the chain, read back into
//! memory and kept there, and offers turned into agreements. The chain
//! is the authority; what is here is a cache of it, rebuilt at every
//! start and reconciled at every poll. One task, and every chain write
//! of the LB goes through it: the acceptances at the discovery poll,
//! and the refresh that keeps the listing and the eligible providers'
//! agreement records alive.

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
            Address, Agreement, CounterRecord, CounterState, EntityKey, KIND_AGREEMENT,
            KIND_COUNTER, KIND_LB_LISTING, KIND_OFFER, LbListing, Offer, Record, Stored,
        },
        writer::{Batch, Create, Expiry, Extend, Identity, Operation, Patch, WriteError, send},
    },
    config,
    marketplace::admission::Agreements,
    pool::{Pool, Provider, marketplace_id},
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

/// What the agent knows about one live agreement: its record, and the
/// open counter record that counts for it. `counter` is `None` while
/// the agreement has none, after a write that did not land: the flush
/// opens one.
#[derive(Debug, Clone)]
struct Live {
    agreement: Stored<Agreement>,
    counter: Option<OpenCounter>,
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
    /// The live agreements, by key: the slot state, and the counter
    /// record each one counts into.
    agreements: Mutex<HashMap<EntityKey, Live>>,
}

impl<R: ChainReader, W: ChainWriter> Agreements for Agent<R, W> {
    fn agreement(&self, key: EntityKey) -> Option<Stored<Agreement>> {
        Agent::agreement(self, key)
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
            agreements: Mutex::new(HashMap::new()),
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
        let listing_key = ensure_listing(
            &agent.reader,
            &agent.writer,
            &agent.config,
            agent.identity.address,
            head,
        )
        .await?;
        Ok(Self {
            listing_key,
            ..agent
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn listing_key(&self) -> EntityKey {
        self.listing_key
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
        discovery_polls.tick().await;
        refreshes.tick().await;
        loop {
            tokio::select! {
                _ = discovery_polls.tick() => self.discovery_poll().await,
                _ = refreshes.tick() => self.refresh().await,
                _ = shutdown.changed() => return,
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
            if let Err(error) = self.reconcile_counters(head).await {
                tracing::warn!(%error, "the open counter records are left as memory has them");
            }
            self.discover_offers(head).await;
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
        // Oldest first, so when two records name the same provider the
        // older one is the one kept. By creation block: a page's own
        // order promises nothing, and an expiry moves with every refresh.
        live.sort_by_key(|stored| (stored.created_at, stored.key));
        let live_keys: HashSet<EntityKey> = live.iter().map(|stored| stored.key).collect();

        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gone: Vec<EntityKey> = known
            .keys()
            .filter(|key| !live_keys.contains(*key))
            .copied()
            .collect();
        for key in gone {
            let Some(live) = known.remove(&key) else {
                continue;
            };
            self.pool
                .remove(&marketplace_id(live.agreement.record.provider));
            tracing::info!(
                provider = %live.agreement.record.provider,
                agreement = %key,
                "agreement over: its record is gone from the chain"
            );
        }
        let mut adopted = 0;
        for stored in live {
            let key = stored.key;
            if let Some(live) = known.get_mut(&key) {
                // The refresh moved the expiry, and its own check leans
                // on this being current; the creation block was an
                // estimate at acceptance.
                live.agreement.created_at = stored.created_at;
                live.agreement.expires_at = stored.expires_at;
                continue;
            }
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
            self.pool.add(Provider::from_marketplace(
                stored.record.provider,
                key,
                stored.record.remote_port,
            ));
            tracing::info!(provider = %stored.record.provider, agreement = %key, "agreement adopted");
            known.insert(
                key,
                Live {
                    agreement: stored,
                    counter: None,
                },
            );
            adopted += 1;
        }
        tracing::debug!(live = known.len(), adopted, "reconciled with the chain");
        Ok(head)
    }

    /// Memory against the chain again, for the counter records: every
    /// open record of this LB, one page, matched to the agreements
    /// memory holds. The same read at start and at every poll, so an
    /// agreement adopted at a poll finds its record the same way a
    /// restart does. What the chain does not have is not remembered,
    /// and the flush opens, closes or deletes what this leaves: a
    /// record for an agreement without one, a younger duplicate, a
    /// record whose agreement is gone.
    async fn reconcile_counters(&self, head: u64) -> Result<(), ReconcileError> {
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
        // One record per agreement, the oldest: which one counts is
        // decided here, so what reads this has a single record. The
        // younger one is not logged here, since this runs every poll
        // and the flush says so when it deletes it.
        let mut by_agreement: HashMap<EntityKey, Stored<CounterRecord>> = HashMap::new();
        for entity in &page.entities {
            let stored = match Stored::<CounterRecord>::decode(entity) {
                Ok(stored) => stored,
                Err(error) => {
                    tracing::warn!(key = %entity.key, %error, "a counter record does not decode: skipped");
                    continue;
                }
            };
            match by_agreement.entry(stored.record.agreement) {
                Entry::Vacant(slot) => {
                    slot.insert(stored);
                }
                Entry::Occupied(mut slot) => {
                    if creation(&stored) < creation(slot.get()) {
                        slot.insert(stored);
                    }
                }
            }
        }
        let mut known = self
            .agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, live) in known.iter_mut() {
            let Some(stored) = by_agreement.remove(key) else {
                // Only a record memory held and the chain no longer has
                // is worth a line: an agreement that never had one is
                // waiting for the flush, which says so when it opens it.
                if live.counter.is_some() {
                    tracing::warn!(agreement = %key, "the open counter record is gone from the chain: the next flush opens one");
                }
                live.counter = None;
                continue;
            };
            // The count the record carries is this period's so far, so
            // the entry counts on from it. Only when the record was
            // unknown: counting it in twice would bill it twice.
            if live.counter.is_none()
                && let Some(entry) = self
                    .pool
                    .get(&marketplace_id(live.agreement.record.provider))
            {
                entry.seed_served(stored.record.count);
            }
            live.counter = Some(open_counter(&stored));
        }
        Ok(())
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
        for offer in offers {
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
            if self.accept(&offer, port, head).await {
                accepted_providers.insert(offer.creator);
                used_ports.insert(port);
                accepted += 1;
            }
        }
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
        let created = match self
            .writer
            .create(&Create::new(
                agreement.encode(),
                Expiry::Seconds(self.config.accept_window.as_secs()),
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
        self.pool.add(Provider::from_marketplace(
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
                    counter: None,
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
        // A dry key is refused before execution and every write stops,
        // so a low balance is worth a warning before it gets there. A
        // failed read does not stop the refresh: the reference being
        // down is not the sidecar being down.
        let balance = match self.reader.balance(self.identity.address).await {
            Ok(balance) => {
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
        };
        // The head is for leaving out records memory knows expired:
        // extending a gone record fails the whole batch.
        let head = match self.reader.block_number().await {
            Ok(head) => Some(head),
            Err(error) => {
                tracing::warn!(%error, "the head could not be read: expired records are not filtered");
                None
            }
        };

        let mut batch = Batch::single(Operation::Extend(Extend {
            entity_key: self.listing_key,
            expires: Expiry::Seconds(self.config.listing_life.as_secs()),
        }));
        // Eligibility at this moment is the one rule: an ineligible
        // provider is skipped, so its record expires `agreement_life`
        // after its last refresh, or at the accept window if it never
        // passed a probe.
        let eligible: HashSet<String> = self
            .pool
            .snapshot()
            .iter()
            .filter(|provider| provider.eligible())
            .map(|provider| provider.id.clone())
            .collect();
        let mut skipped = 0;
        {
            let known = self
                .agreements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (key, live) in known.iter() {
                let agreement = &live.agreement;
                if !eligible.contains(&marketplace_id(agreement.record.provider)) {
                    skipped += 1;
                    continue;
                }
                if head.is_some_and(|head| agreement.expires_at <= head) {
                    tracing::warn!(
                        agreement = %key,
                        provider = %agreement.record.provider,
                        "an eligible provider's agreement record has expired: not refreshed, \
                         the next poll drops it"
                    );
                    continue;
                }
                batch.push(Operation::Extend(Extend {
                    entity_key: *key,
                    expires: Expiry::Seconds(self.config.agreement_life.as_secs()),
                }));
            }
        }
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
            ineligible = skipped,
            "refresh"
        );
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
/// longest.
async fn ensure_listing<R: ChainReader, W: ChainWriter>(
    reader: &R,
    writer: &W,
    config: &config::Marketplace,
    lb: Address,
    head: u64,
) -> Result<EntityKey, StartError> {
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
        return Ok(created.entity_key);
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
    Ok(kept.key)
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
