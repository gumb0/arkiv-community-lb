//! The marketplace agent: the LB's state on the chain, read back into
//! memory and kept there. The chain is the authority; what is here is a
//! cache of it, rebuilt at every start and reconciled at every poll.
//! One task, and every chain write of the LB goes through it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::{
    chain::{
        ChainReader, ChainWriter,
        reader::{PAGE_LIMIT, Query, ReadError},
        records::{
            Address, Agreement, EntityKey, KIND_AGREEMENT, KIND_LB_LISTING, LbListing, Record,
            Stored,
        },
        writer::{Create, Expiry, Identity, Patch, WriteError},
    },
    config,
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
}

/// Why a reconcile did not happen. At startup either is a reason not
/// to start; at a poll, a reason to skip this one.
#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("the chain could not be read")]
    Chain(#[source] ReadError),
    #[error("{count} agreement records, more than one page holds")]
    TooMany { count: u64 },
}

pub struct Agent<R, W> {
    reader: R,
    writer: W,
    config: config::Marketplace,
    pool: Arc<Pool>,
    identity: Identity,
    listing_key: EntityKey,
    /// The live agreement records, by key: the slot state.
    agreements: Mutex<HashMap<EntityKey, Stored<Agreement>>>,
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
        // The reconcile only reads and can refuse the start; the listing
        // writes. This order keeps a refused start from writing anything.
        agent.reconcile().await.map_err(|error| match error {
            ReconcileError::Chain(error) => StartError::Chain(error),
            ReconcileError::TooMany { count } => StartError::TooManyAgreements { count },
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

    /// The live agreements as the agent knows them, in no particular order.
    pub fn agreements(&self) -> Vec<Stored<Agreement>> {
        self.agreements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// The task: a poll every discovery interval until shutdown. The
    /// start already reconciled, so the first tick is skipped.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = tokio::time::interval(self.config.discovery_interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            tokio::select! {
                _ = ticks.tick() => self.poll().await,
                _ = shutdown.changed() => return,
            }
        }
    }

    /// One discovery poll. The reconcile comes first; a poll that cannot
    /// read the chain, or finds more records than a page, changes
    /// nothing and says so.
    pub async fn poll(&self) {
        match self.reconcile().await {
            Ok(()) => {}
            Err(ReconcileError::TooMany { count }) => tracing::error!(
                count,
                "more agreement records than one page holds: this poll's reconcile is skipped"
            ),
            Err(ReconcileError::Chain(error)) => {
                tracing::warn!(%error, "the chain could not be read: this poll's reconcile is skipped");
            }
        }
    }

    /// Memory against the chain. The LB's live agreement records are
    /// read (count then page: a full page says nothing about what lies
    /// beyond it); an agreement the chain has and memory does not is
    /// adopted, one memory has and the chain does not is over: its
    /// provider leaves the pool and its slot and port are free. Expired
    /// records have vanished from the chain, so what is there is what
    /// is live. Nothing is applied on a read that failed.
    async fn reconcile(&self) -> Result<(), ReconcileError> {
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
            return Err(ReconcileError::TooMany { count });
        }
        let page = self
            .reader
            .query(&query)
            .await
            .map_err(ReconcileError::Chain)?;
        // In the page's order: when two records name the same provider,
        // the first one the page lists is the one kept.
        let mut live = Vec::new();
        for entity in &page.entities {
            match Stored::<Agreement>::decode(entity) {
                Ok(stored) => live.push(stored),
                Err(error) => {
                    tracing::warn!(key = %entity.key, %error, "an agreement record does not decode: skipped");
                }
            }
        }
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
            let Some(agreement) = known.remove(&key) else {
                continue;
            };
            self.pool.remove(&marketplace_id(agreement.record.provider));
            tracing::info!(
                provider = %agreement.record.provider,
                agreement = %key,
                "agreement over: its record is gone from the chain"
            );
        }
        let mut adopted = 0;
        for stored in live {
            let key = stored.key;
            if known.contains_key(&key) {
                continue;
            }
            if known
                .values()
                .any(|other| other.record.provider == stored.record.provider)
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
            known.insert(key, stored);
            adopted += 1;
        }
        tracing::debug!(live = known.len(), adopted, "reconciled with the chain");
        Ok(())
    }
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
    // Oldest first: the earliest expiry is the earliest write.
    listings.sort_by_key(|listing| listing.expires_at);
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
