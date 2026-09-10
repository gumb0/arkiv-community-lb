//! An in-memory chain behind the two chain traits: what the LB reads
//! and writes, without a node or a sidecar. The head is a number the
//! test moves; nothing advances on its own. Lifetimes turn into blocks
//! the way the sidecar does it, at two seconds per block. Writes land
//! at once, keys and transaction hashes are counters, and a query is
//! evaluated from its typed conditions.

use std::sync::{Arc, Mutex};

use alloy_primitives::{B256, Bytes, U256};
use lb::chain::{
    ChainReader, ChainWriter,
    reader::{Condition, PAGE_LIMIT, Page, Query, ReadError},
    records::{Address, ArkivAttribute, ArkivEntity, Attributes, EncodedRecord, EntityKey},
    writer::{
        Batch, BatchResult, Create, Created, Delete, ErrorLink, Expiry, Identity, Patch,
        WriteError, Written,
    },
};

#[derive(Clone)]
pub struct FakeChain(Arc<Mutex<State>>);

struct State {
    head: u64,
    address: Address,
    chain_id: u64,
    balance: U256,
    next_key: u64,
    next_tx: u64,
    /// In creation order, which is the order a query returns.
    entities: Vec<Entity>,
    transactions: Vec<Transaction>,
    /// When set, every write fails with this message.
    failing: Option<String>,
}

/// A stored entity, as a test sees it.
#[derive(Debug, Clone)]
pub struct Entity {
    pub key: EntityKey,
    pub creator: Address,
    pub expires_at: u64,
    pub attributes: Attributes,
    pub payload: Vec<u8>,
}

/// One write, as the fake landed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transaction {
    Create(EntityKey),
    Patch(EntityKey),
    Delete(EntityKey),
    Extend(Vec<EntityKey>),
}

const SECONDS_PER_BLOCK: u64 = 2;

impl FakeChain {
    /// A chain at head 1 whose writer signs as `address`, holding one GLM.
    pub fn new(address: Address, chain_id: u64) -> Self {
        Self(Arc::new(Mutex::new(State {
            head: 1,
            address,
            chain_id,
            balance: U256::from(1_000_000_000_000_000_000u64),
            next_key: 1,
            next_tx: 1,
            entities: Vec::new(),
            transactions: Vec::new(),
            failing: None,
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn head(&self) -> u64 {
        self.state().head
    }

    /// Moves the head forward. Records whose expiry it passes vanish
    /// from reads, the way they do on the node.
    pub fn advance(&self, blocks: u64) {
        self.state().head += blocks;
    }

    pub fn set_balance(&self, balance: U256) {
        self.state().balance = balance;
    }

    /// A record written by someone else, for example a provider's offer.
    pub fn write_as(&self, creator: Address, record: EncodedRecord, expires: Expiry) -> EntityKey {
        let mut state = self.state();
        let expires_at = state.expires_at(expires);
        state.insert(creator, record.attributes, record.payload, expires_at)
    }

    /// The entity as stored, expired or not; `None` once deleted.
    pub fn entity(&self, key: EntityKey) -> Option<Entity> {
        self.state()
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .cloned()
    }

    pub fn transactions(&self) -> Vec<Transaction> {
        self.state().transactions.clone()
    }

    /// Every write from now on fails with this message, until `heal`.
    pub fn fail_writes(&self, message: &str) {
        self.state().failing = Some(message.to_owned());
    }

    pub fn heal(&self) {
        self.state().failing = None;
    }
}

impl State {
    fn expires_at(&self, expires: Expiry) -> u64 {
        match expires {
            Expiry::Permanent => u64::MAX,
            Expiry::Seconds(seconds) => self.head + seconds / SECONDS_PER_BLOCK,
        }
    }

    fn insert(
        &mut self,
        creator: Address,
        attributes: Attributes,
        payload: Vec<u8>,
        expires_at: u64,
    ) -> EntityKey {
        let key = B256::from(U256::from(self.next_key));
        self.next_key += 1;
        self.entities.push(Entity {
            key,
            creator,
            expires_at,
            attributes,
            payload,
        });
        key
    }

    fn transaction(&mut self, transaction: Transaction) -> String {
        let hash = format!("{:#x}", B256::from(U256::from(self.next_tx)));
        self.next_tx += 1;
        self.transactions.push(transaction);
        hash
    }

    fn failure(&self) -> Result<(), WriteError> {
        match &self.failing {
            Some(message) => Err(WriteError::Failed(vec![ErrorLink {
                name: "FakeChain".to_owned(),
                message: message.clone(),
            }])),
            None => Ok(()),
        }
    }

    fn position(&self, key: EntityKey) -> Result<usize, WriteError> {
        self.entities
            .iter()
            .position(|entity| entity.key == key)
            .ok_or_else(|| {
                WriteError::Failed(vec![ErrorLink {
                    name: "FakeChain".to_owned(),
                    message: format!("no entity {key:#x}"),
                }])
            })
    }

    /// The rows a query returns: alive at the head, and matching every
    /// condition.
    fn matching(&self, query: &Query) -> Vec<ArkivEntity> {
        self.entities
            .iter()
            .filter(|entity| entity.expires_at > self.head)
            .filter(|entity| {
                query
                    .conditions
                    .iter()
                    .all(|condition| entity.matches(condition))
            })
            .map(Entity::as_arkiv_entity)
            .collect()
    }
}

impl Entity {
    fn matches(&self, condition: &Condition) -> bool {
        match condition {
            Condition::Creator(creator) => self.creator == *creator,
            Condition::Attribute(name, value) => self.attributes.get(name) == Some(value),
            Condition::ExpiresAfter(head) => self.expires_at > *head,
            Condition::ExpiresBy(block) => self.expires_at <= *block,
        }
    }

    /// The entity the way a query returns it: attributes as the node's
    /// list, in the same shapes the write sent.
    fn as_arkiv_entity(&self) -> ArkivEntity {
        let wire = self.attributes.to_wire();
        let attributes = wire
            .as_object()
            .expect("attributes serialize to an object")
            .iter()
            .map(|(name, typed)| ArkivAttribute {
                name: name.clone(),
                type_tag: typed["type"].as_str().expect("a type tag").to_owned(),
                value: typed["value"].clone(),
            })
            .collect();
        ArkivEntity {
            key: self.key,
            creator: self.creator,
            expires_at: self.expires_at,
            payload: Bytes::from(self.payload.clone()),
            attributes,
        }
    }
}

impl ChainReader for FakeChain {
    async fn block_number(&self) -> Result<u64, ReadError> {
        Ok(self.state().head)
    }

    async fn balance(&self, account: Address) -> Result<U256, ReadError> {
        let state = self.state();
        Ok(if account == state.address {
            state.balance
        } else {
            U256::ZERO
        })
    }

    async fn query(&self, query: &Query) -> Result<Page, ReadError> {
        let mut entities = self.state().matching(query);
        let more = entities.len() as u64 > PAGE_LIMIT;
        entities.truncate(PAGE_LIMIT as usize);
        Ok(Page { entities, more })
    }

    async fn count(&self, query: &Query) -> Result<u64, ReadError> {
        Ok(self.state().matching(query).len() as u64)
    }
}

impl ChainWriter for FakeChain {
    async fn identity(&self) -> Result<Identity, WriteError> {
        let state = self.state();
        Ok(Identity {
            address: state.address,
            chain_id: state.chain_id,
        })
    }

    async fn create(&self, create: &Create) -> Result<Created, WriteError> {
        let mut state = self.state();
        state.failure()?;
        let creator = state.address;
        let expires_at = state.expires_at(create.expires());
        let entity_key = state.insert(
            creator,
            create.attributes().clone(),
            create.payload().to_vec(),
            expires_at,
        );
        let tx_hash = state.transaction(Transaction::Create(entity_key));
        Ok(Created {
            entity_key,
            tx_hash,
            expires_at,
        })
    }

    async fn patch(&self, patch: &Patch) -> Result<Written, WriteError> {
        let mut state = self.state();
        state.failure()?;
        let index = state.position(patch.entity_key)?;
        let entity = &mut state.entities[index];
        if let Some(set) = &patch.set {
            let mut attributes = entity.attributes.clone();
            for (name, value) in set.iter() {
                attributes = attributes.with(name, value.clone());
            }
            entity.attributes = attributes;
        }
        if let Some(payload) = &patch.payload {
            entity.payload = payload.clone();
        }
        let tx_hash = state.transaction(Transaction::Patch(patch.entity_key));
        Ok(Written {
            entity_key: patch.entity_key,
            tx_hash,
        })
    }

    async fn delete(&self, delete: &Delete) -> Result<Written, WriteError> {
        let mut state = self.state();
        state.failure()?;
        let index = state.position(delete.entity_key)?;
        state.entities.remove(index);
        let tx_hash = state.transaction(Transaction::Delete(delete.entity_key));
        Ok(Written {
            entity_key: delete.entity_key,
            tx_hash,
        })
    }

    /// All extends land or none does: the keys are checked before any
    /// expiry moves.
    async fn execute_batch(&self, batch: &Batch) -> Result<BatchResult, WriteError> {
        let mut state = self.state();
        state.failure()?;
        let mut positions = Vec::with_capacity(batch.extensions.len());
        for extend in &batch.extensions {
            positions.push((state.position(extend.entity_key)?, extend.expires));
        }
        for (index, expires) in positions {
            let expires_at = state.expires_at(expires);
            state.entities[index].expires_at = expires_at;
        }
        let extended_entities: Vec<EntityKey> =
            batch.extensions.iter().map(|e| e.entity_key).collect();
        let tx_hash = state.transaction(Transaction::Extend(extended_entities.clone()));
        Ok(BatchResult {
            tx_hash,
            extended_entities,
        })
    }
}
