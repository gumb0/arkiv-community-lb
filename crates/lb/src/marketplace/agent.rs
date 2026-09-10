//! The marketplace agent's startup: the LB's state on the chain, read
//! back into memory. The chain is the authority; what is here is a
//! cache of it, rebuilt at every start.

use std::{collections::HashMap, sync::Mutex};

use crate::{
    chain::{
        ChainReader, ChainWriter,
        reader::{PAGE_LIMIT, Query, ReadError},
        records::{
            Agreement, EntityKey, KIND_AGREEMENT, KIND_LB_LISTING, LbListing, Record, Stored,
        },
        writer::{Create, Expiry, Identity, Patch, WriteError},
    },
    config,
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
}

#[derive(Debug)]
pub struct Agent {
    identity: Identity,
    listing_key: EntityKey,
    /// The live agreement records, by key: the slot state.
    agreements: Mutex<HashMap<EntityKey, Stored<Agreement>>>,
}

impl Agent {
    /// Reads the LB's records back from the chain and makes the listing
    /// match the configuration. Every failure here is a reason not to
    /// start: without the chain the LB does not know its own providers.
    pub async fn start<R: ChainReader, W: ChainWriter>(
        reader: &R,
        writer: &W,
        config: &config::Marketplace,
        pool: &Pool,
    ) -> Result<Self, StartError> {
        let identity = writer.identity().await.map_err(StartError::Sidecar)?;
        let head = reader.block_number().await.map_err(StartError::Chain)?;
        // The reload only reads and can refuse the start; the listing
        // writes. This order keeps a refused start from writing anything.
        let agreements = reload_agreements(reader, pool, identity.address, head).await?;
        if agreements.len() > config.max_providers as usize {
            tracing::warn!(
                live = agreements.len(),
                cap = config.max_providers,
                "more agreements than the cap allows: nobody is evicted, no offer is accepted \
                 until enough expire"
            );
        }
        let listing_key = ensure_listing(reader, writer, config, identity.address, head).await?;
        Ok(Self {
            identity,
            listing_key,
            agreements: Mutex::new(agreements),
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
}

/// The LB's own agreement records, each one a marketplace provider in
/// the pool. Expired records have vanished from the chain, so what is
/// there is what is live. One page must hold them all: the count is
/// checked first, since a page that is full says nothing about what
/// lies beyond it.
async fn reload_agreements<R: ChainReader>(
    reader: &R,
    pool: &Pool,
    lb: alloy_primitives::Address,
    head: u64,
) -> Result<HashMap<EntityKey, Stored<Agreement>>, StartError> {
    let query = Query::kind(KIND_AGREEMENT).creator(lb).expires_after(head);
    let count = reader.count(&query).await.map_err(StartError::Chain)?;
    if count > PAGE_LIMIT {
        return Err(StartError::TooManyAgreements { count });
    }
    let page = reader.query(&query).await.map_err(StartError::Chain)?;
    let mut agreements = HashMap::new();
    for entity in &page.entities {
        let stored = match Stored::<Agreement>::decode(entity) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(key = %entity.key, %error, "an agreement record does not decode: skipped");
                continue;
            }
        };
        if agreements
            .values()
            .any(|known: &Stored<Agreement>| known.record.provider == stored.record.provider)
        {
            tracing::warn!(
                provider = %stored.record.provider,
                key = %stored.key,
                "a second agreement record for one provider: skipped"
            );
            continue;
        }
        pool.add(Provider::from_marketplace(
            stored.record.provider,
            stored.key,
            stored.record.remote_port,
        ));
        agreements.insert(stored.key, stored);
    }
    tracing::info!(
        agreements = agreements.len(),
        "marketplace providers reloaded from the chain"
    );
    Ok(agreements)
}

/// One live listing that says what the configuration says: created if
/// there is none, patched if it differs. When there are several, the
/// oldest is the LB's: it is the one offers have been pointing at the
/// longest.
async fn ensure_listing<R: ChainReader, W: ChainWriter>(
    reader: &R,
    writer: &W,
    config: &config::Marketplace,
    lb: alloy_primitives::Address,
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
