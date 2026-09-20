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
        Batch, BatchResult, Create, Created, Delete, ErrorLink, Expiry, Identity, Operation, Patch,
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
    /// `Some(0)`: every call on the sidecar side fails. `Some(n)`: it
    /// fails after `n` more writes. `None`: the sidecar is up.
    sidecar_down_after: Option<usize>,
    /// The message the sidecar fails with.
    sidecar_down_message: Option<String>,
    /// When set, every read fails with this message.
    reference_down: Option<String>,
    /// When set, the next write lands but is answered as unresolved,
    /// the sidecar's 504: the caller never learns it landed.
    unresolved_next: bool,
    /// A batch with more operations than this is refused as too large,
    /// the way the node refuses a transaction over its size cap.
    operation_limit: usize,
}

/// A stored entity, as a test sees it.
#[derive(Debug, Clone)]
pub struct Entity {
    pub key: EntityKey,
    pub creator: Address,
    /// The head when it was written, like the node's creation block.
    pub created_at: u64,
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
    Batch(BatchLog),
}

/// What one batch did, by kind, in operation order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatchLog {
    pub created: Vec<EntityKey>,
    pub patched: Vec<EntityKey>,
    pub deleted: Vec<EntityKey>,
    pub extended: Vec<EntityKey>,
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
            sidecar_down_after: None,
            sidecar_down_message: None,
            reference_down: None,
            unresolved_next: false,
            operation_limit: usize::MAX,
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

    /// The number of operations over which a batch is refused as too
    /// large. Unlimited by default; a test sets it to exercise the split.
    pub fn set_operation_limit(&self, operations: usize) {
        self.state().operation_limit = operations;
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

    /// Every write and the identity fail with this message, until `heal`.
    pub fn fail_sidecar(&self, message: &str) {
        self.fail_sidecar_after(0, message);
    }

    /// Every read fails with this message, until `heal`.
    pub fn fail_reference(&self, message: &str) {
        self.state().reference_down = Some(message.to_owned());
    }

    /// The next write lands, but its answer is a 504: the write is
    /// unresolved from the caller's side.
    pub fn unresolved_next(&self) {
        self.state().unresolved_next = true;
    }

    /// The sidecar goes down after this many more writes land.
    pub fn fail_sidecar_after(&self, writes: usize, message: &str) {
        let mut state = self.state();
        state.sidecar_down_after = Some(writes);
        state.sidecar_down_message = Some(message.to_owned());
    }

    pub fn heal(&self) {
        let mut state = self.state();
        state.sidecar_down_after = None;
        state.sidecar_down_message = None;
        state.reference_down = None;
        state.unresolved_next = false;
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
            created_at: self.head,
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
        if let Some(left) = &mut self.sidecar_down_after {
            *left = left.saturating_sub(1);
        }
        hash
    }

    /// A landed write's answer, or a 504 when the test asked for one.
    fn answer<T>(&mut self, tx_hash: String, value: T) -> Result<T, WriteError> {
        if std::mem::take(&mut self.unresolved_next) {
            return Err(WriteError::Unresolved {
                tx_hash,
                errors: vec![ErrorLink {
                    name: "WaitForTransactionReceiptTimeoutError".to_owned(),
                    message: "timed out".to_owned(),
                    details: None,
                }],
            });
        }
        Ok(value)
    }

    fn sidecar(&self) -> Result<(), WriteError> {
        match self.sidecar_down_after {
            Some(0) => Err(WriteError::Failed(vec![ErrorLink {
                name: "FakeChain".to_owned(),
                message: self.sidecar_down_message.clone().unwrap_or_default(),
                details: None,
            }])),
            _ => Ok(()),
        }
    }

    fn reference(&self) -> Result<(), ReadError> {
        match &self.reference_down {
            Some(message) => Err(ReadError::Rpc {
                code: -32000,
                message: message.clone(),
            }),
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
                    details: None,
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
    fn apply(&mut self, patch: &Patch) {
        if let Some(set) = &patch.set {
            let mut attributes = self.attributes.clone();
            for (name, value) in set.iter() {
                attributes = attributes.with(name, value.clone());
            }
            self.attributes = attributes;
        }
        if let Some(payload) = &patch.payload {
            self.payload = payload.clone();
        }
    }

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
    pub fn as_arkiv_entity(&self) -> ArkivEntity {
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
            created_at: self.created_at,
            expires_at: self.expires_at,
            payload: Bytes::from(self.payload.clone()),
            attributes,
        }
    }
}

impl ChainReader for FakeChain {
    async fn block_number(&self) -> Result<u64, ReadError> {
        let state = self.state();
        state.reference()?;
        Ok(state.head)
    }

    async fn balance(&self, account: Address) -> Result<U256, ReadError> {
        let state = self.state();
        state.reference()?;
        Ok(if account == state.address {
            state.balance
        } else {
            U256::ZERO
        })
    }

    async fn query(&self, query: &Query) -> Result<Page, ReadError> {
        let state = self.state();
        state.reference()?;
        let mut entities = state.matching(query);
        let more = entities.len() as u64 > PAGE_LIMIT;
        entities.truncate(PAGE_LIMIT as usize);
        Ok(Page { entities, more })
    }

    async fn count(&self, query: &Query) -> Result<u64, ReadError> {
        let state = self.state();
        state.reference()?;
        Ok(state.matching(query).len() as u64)
    }
}

impl ChainWriter for FakeChain {
    async fn identity(&self) -> Result<Identity, WriteError> {
        let state = self.state();
        state.sidecar()?;
        Ok(Identity {
            address: state.address,
            chain_id: state.chain_id,
        })
    }

    async fn create(&self, create: &Create) -> Result<Created, WriteError> {
        let mut state = self.state();
        state.sidecar()?;
        let creator = state.address;
        let expires_at = state.expires_at(create.expires());
        let entity_key = state.insert(
            creator,
            create.attributes().clone(),
            create.payload().to_vec(),
            expires_at,
        );
        let tx_hash = state.transaction(Transaction::Create(entity_key));
        state.answer(
            tx_hash.clone(),
            Created {
                entity_key,
                tx_hash,
                expires_at,
            },
        )
    }

    async fn patch(&self, patch: &Patch) -> Result<Written, WriteError> {
        let mut state = self.state();
        state.sidecar()?;
        let index = state.position(patch.entity_key)?;
        state.entities[index].apply(patch);
        let tx_hash = state.transaction(Transaction::Patch(patch.entity_key));
        Ok(Written {
            entity_key: patch.entity_key,
            tx_hash,
        })
    }

    async fn delete(&self, delete: &Delete) -> Result<Written, WriteError> {
        let mut state = self.state();
        state.sidecar()?;
        let index = state.position(delete.entity_key)?;
        state.entities.remove(index);
        let tx_hash = state.transaction(Transaction::Delete(delete.entity_key));
        Ok(Written {
            entity_key: delete.entity_key,
            tx_hash,
        })
    }

    /// One transaction: every operation lands or none does. The keys
    /// are checked before anything changes, and a batch over the
    /// operation limit is refused before that, the way the node refuses
    /// a transaction over its size.
    async fn execute_batch(&self, batch: &Batch) -> Result<BatchResult, WriteError> {
        let mut state = self.state();
        state.sidecar()?;
        let operations = batch.operations();
        if operations.len() > state.operation_limit {
            return Err(WriteError::TooLarge(vec![ErrorLink {
                name: "FakeChain".to_owned(),
                message: "Missing or invalid parameters.".to_owned(),
                details: Some(format!(
                    "oversized data: {} operations, limit {}",
                    operations.len(),
                    state.operation_limit
                )),
            }]));
        }
        // Every entity a patch, delete or extend names must exist before
        // anything is applied, so a missing key fails the batch whole.
        for operation in operations {
            let key = match operation {
                Operation::Create(_) => continue,
                Operation::Patch(patch) => patch.entity_key,
                Operation::Delete(delete) => delete.entity_key,
                Operation::Extend(extend) => extend.entity_key,
            };
            state.position(key)?;
        }
        let mut log = BatchLog::default();
        for operation in operations {
            match operation {
                Operation::Create(create) => {
                    let creator = state.address;
                    let expires_at = state.expires_at(create.expires());
                    let key = state.insert(
                        creator,
                        create.attributes().clone(),
                        create.payload().to_vec(),
                        expires_at,
                    );
                    log.created.push(key);
                }
                Operation::Patch(patch) => {
                    let index = state.position(patch.entity_key)?;
                    state.entities[index].apply(patch);
                    log.patched.push(patch.entity_key);
                }
                Operation::Delete(delete) => {
                    let index = state.position(delete.entity_key)?;
                    state.entities.remove(index);
                    log.deleted.push(delete.entity_key);
                }
                Operation::Extend(extend) => {
                    let index = state.position(extend.entity_key)?;
                    let expires_at = state.expires_at(extend.expires);
                    state.entities[index].expires_at = expires_at;
                    log.extended.push(extend.entity_key);
                }
            }
        }
        let result = BatchResult {
            tx_hash: String::new(),
            created_entities: log.created.clone(),
            patched_entities: log.patched.clone(),
            deleted_entities: log.deleted.clone(),
            extended_entities: log.extended.clone(),
        };
        let tx_hash = state.transaction(Transaction::Batch(log));
        Ok(BatchResult { tx_hash, ..result })
    }
}
