//! The client for the chain-writer sidecar (`docs/CHAIN_WRITER.md`): five
//! POST routes, JSON bodies, and three outcomes for every write, plus one
//! GET that tells who the sidecar writes as. A 504 is the one to handle
//! with care: the transaction was sent and may still land, so the caller
//! polls its hash and never resends.

use std::time::Duration;

use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::records::{Address, Attributes, EncodedRecord, EntityKey};

/// Above the sidecar's own 180 s receipt wait, so its 504 arrives before
/// this client gives up.
const TIMEOUT: Duration = Duration::from_secs(200);

/// The lifetime a write asks for. Seconds are converted to blocks by the
/// sidecar at the network's block time.
#[derive(Debug, Clone, Copy)]
pub enum Expiry {
    Permanent,
    Seconds(u64),
}

impl Serialize for Expiry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Permanent => serializer.serialize_str("permanent"),
            Self::Seconds(n) => serde_json::json!({ "seconds": n }).serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Create {
    #[serde(serialize_with = "serialize_base64")]
    payload: Vec<u8>,
    content_type: &'static str,
    #[serde(serialize_with = "serialize_attributes")]
    attributes: Attributes,
    expires: Expiry,
}

impl Create {
    pub fn new(record: EncodedRecord, expires: Expiry) -> Self {
        Self {
            payload: record.payload,
            content_type: "application/json",
            attributes: record.attributes,
            expires,
        }
    }

    pub fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn expires(&self) -> Expiry {
        self.expires
    }
}

/// Sets attributes and replaces the payload; nothing here unsets an
/// attribute or changes the content type, because no record needs it.
/// The sidecar route supports both.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub entity_key: EntityKey,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_attributes_opt"
    )]
    pub set: Option<Attributes>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_base64_opt"
    )]
    pub payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Extend {
    pub entity_key: EntityKey,
    pub expires: Expiry,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Delete {
    pub entity_key: EntityKey,
}

/// One operation of a batch.
#[derive(Debug, Clone)]
pub enum Operation {
    Create(Create),
    Patch(Patch),
    Delete(Delete),
    Extend(Extend),
}

/// Operations that must land together: an acceptance's two creates, an
/// agreement's close patch and its successor's create. A chunk never
/// splits a group, and a chunk is one transaction, so a group lands
/// whole or not at all.
#[derive(Debug, Clone)]
pub struct Group(Vec<Operation>);

impl Group {
    pub fn new(operations: Vec<Operation>) -> Self {
        Self(operations)
    }

    pub fn single(operation: Operation) -> Self {
        Self(vec![operation])
    }

    pub fn operations(&self) -> &[Operation] {
        &self.0
    }
}

/// Operations for one or more transactions, in groups. `send` sends it
/// as one transaction and splits it when the node refuses that as too
/// large.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    groups: Vec<Group>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single(operation: Operation) -> Self {
        let mut batch = Self::new();
        batch.push(Group::single(operation));
        batch
    }

    pub fn push(&mut self, group: Group) {
        self.groups.push(group);
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Splits into two halves at a group boundary. `None` when there is
    /// only one group.
    fn halves(self) -> Option<(Batch, Batch)> {
        if self.groups.len() < 2 {
            return None;
        }
        let mut first = self.groups;
        let second = first.split_off(first.len() / 2);
        Some((Batch { groups: first }, Batch { groups: second }))
    }
}

/// The sidecar's batch body: the operations by kind.
impl Serialize for Batch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut creates = Vec::new();
        let mut patches = Vec::new();
        let mut deletes = Vec::new();
        let mut extensions = Vec::new();
        for operation in self.groups.iter().flat_map(|group| group.0.iter()) {
            match operation {
                Operation::Create(create) => creates.push(create),
                Operation::Patch(patch) => patches.push(patch),
                Operation::Delete(delete) => deletes.push(delete),
                Operation::Extend(extend) => extensions.push(extend),
            }
        }
        let mut body = serde_json::Map::new();
        if !creates.is_empty() {
            body.insert(
                "creates".into(),
                serde_json::to_value(creates).map_err(serde::ser::Error::custom)?,
            );
        }
        if !patches.is_empty() {
            body.insert(
                "patches".into(),
                serde_json::to_value(patches).map_err(serde::ser::Error::custom)?,
            );
        }
        if !deletes.is_empty() {
            body.insert(
                "deletes".into(),
                serde_json::to_value(deletes).map_err(serde::ser::Error::custom)?,
            );
        }
        if !extensions.is_empty() {
            body.insert(
                "extensions".into(),
                serde_json::to_value(extensions).map_err(serde::ser::Error::custom)?,
            );
        }
        serde_json::Value::Object(body).serialize(serializer)
    }
}

/// One batch as sent, with what came back.
#[derive(Debug)]
pub struct Sent {
    pub batch: Batch,
    pub result: Result<BatchResult, WriteError>,
}

/// Sends a batch as one transaction. When the node refuses it as too
/// large (a refusal, before anything is sent), it is split at a group
/// boundary and its halves sent, in order, down to a single group,
/// which is then reported as too large. Any other failure is reported
/// for that part and the rest go on. Nothing sizes a batch in advance:
/// the node's refusal is cheap and rare, and an estimate would be one
/// more thing to keep true.
pub async fn send<W: ChainWriter>(writer: &W, batch: Batch) -> Vec<Sent> {
    let mut queue = std::collections::VecDeque::new();
    if !batch.is_empty() {
        queue.push_back(batch);
    }
    let mut sent = Vec::new();
    while let Some(chunk) = queue.pop_front() {
        let result = writer.execute_batch(&chunk).await;
        if matches!(result, Err(WriteError::TooLarge(_)))
            && let Some((first, second)) = chunk.clone().halves()
        {
            queue.push_front(second);
            queue.push_front(first);
            continue;
        }
        sent.push(Sent {
            batch: chunk,
            result,
        });
    }
    sent
}

fn serialize_base64<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn serialize_base64_opt<S: serde::Serializer>(
    bytes: &Option<Vec<u8>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serialize_base64(bytes.as_deref().unwrap_or_default(), serializer)
}

fn serialize_attributes<S: serde::Serializer>(
    attributes: &Attributes,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    attributes.to_wire().serialize(serializer)
}

fn serialize_attributes_opt<S: serde::Serializer>(
    attributes: &Option<Attributes>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match attributes {
        Some(attributes) => serialize_attributes(attributes, serializer),
        None => serializer.serialize_none(),
    }
}

/// A landed create: the new entity's key and the block it expires at.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub entity_key: EntityKey,
    pub tx_hash: String,
    #[serde(deserialize_with = "deserialize_decimal")]
    pub expires_at: u64,
}

/// A landed patch or delete.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Written {
    pub entity_key: EntityKey,
    pub tx_hash: String,
}

/// A landed extend: the entity's new expiry block.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extended {
    pub entity_key: EntityKey,
    pub tx_hash: String,
    #[serde(deserialize_with = "deserialize_decimal")]
    pub expires_at: u64,
}

/// A landed batch, with the keys of what it did, in operation order.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub tx_hash: String,
    #[serde(default)]
    pub created_entities: Vec<EntityKey>,
    #[serde(default)]
    pub patched_entities: Vec<EntityKey>,
    #[serde(default)]
    pub deleted_entities: Vec<EntityKey>,
    #[serde(default)]
    pub extended_entities: Vec<EntityKey>,
}

fn deserialize_decimal<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

/// Who the sidecar writes as: the address of the key it holds, and the
/// chain it writes to. The LB reads its own records by this address.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub address: Address,
    pub chain_id: u64,
}

/// One link of the sidecar's walked error chain. `details` is the
/// node's own message where the sidecar had it.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorLink {
    pub name: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<String>,
}

/// What the LB writes to the chain, through the sidecar. `Writer` is the
/// real one; a test implements it over an in-memory store. A batch is
/// one transaction; `send` above it does the chunking. The futures are
/// `Send` so a generic caller can run under `tokio::spawn`.
pub trait ChainWriter: Send + Sync {
    fn identity(&self) -> impl Future<Output = Result<Identity, WriteError>> + Send;
    fn create(&self, create: &Create) -> impl Future<Output = Result<Created, WriteError>> + Send;
    fn patch(&self, patch: &Patch) -> impl Future<Output = Result<Written, WriteError>> + Send;
    fn delete(&self, delete: &Delete) -> impl Future<Output = Result<Written, WriteError>> + Send;
    fn execute_batch(
        &self,
        batch: &Batch,
    ) -> impl Future<Output = Result<BatchResult, WriteError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// 400: the body did not decode; nothing was sent.
    #[error("the sidecar refused the request: {}", causes(.0))]
    Refused(Vec<ErrorLink>),
    /// 413: the node refused the transaction as oversized; nothing was
    /// sent. A batch is split and sent again.
    #[error("the transaction is too large: {}", causes(.0))]
    TooLarge(Vec<ErrorLink>),
    /// 500: the transaction reverted or could not be sent.
    #[error("the write failed: {}", causes(.0))]
    Failed(Vec<ErrorLink>),
    /// 504: sent, no receipt in time. It may still land — poll the hash,
    /// never resend.
    #[error("the write is unresolved, transaction {tx_hash}")]
    Unresolved {
        tx_hash: String,
        errors: Vec<ErrorLink>,
    },
    #[error("the sidecar answered {status} with an unexpected body: {body}")]
    Unexpected { status: u16, body: String },
    #[error("the sidecar could not be reached: {0}")]
    Transport(#[from] reqwest::Error),
}

fn causes(links: &[ErrorLink]) -> String {
    links
        .iter()
        .map(|link| match &link.details {
            Some(details) => format!("{}: {} ({details})", link.name, link.message),
            None => format!("{}: {}", link.name, link.message),
        })
        .collect::<Vec<_>>()
        .join(" <- ")
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Vec<ErrorLink>,
    pending: Option<Pending>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    tx_hash: String,
}

#[derive(Clone)]
pub struct Writer {
    client: reqwest::Client,
    base: Url,
}

impl Writer {
    /// The client is the service's shared one; the write timeout is set
    /// per request, so sharing it with shorter-lived reads is fine.
    pub fn new(client: reqwest::Client, base: Url) -> Self {
        Self { client, base }
    }

    /// The one read on the sidecar. Answered from memory, so a short
    /// wait is enough, and a failure means the sidecar is not there.
    pub async fn identity(&self) -> Result<Identity, WriteError> {
        let url = self
            .base
            .join("identity")
            .expect("a route joins onto the base");
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        if status != 200 {
            return Err(WriteError::Unexpected { status, body: text });
        }
        serde_json::from_str(&text).map_err(|_| WriteError::Unexpected { status, body: text })
    }

    pub async fn create(&self, create: &Create) -> Result<Created, WriteError> {
        self.post("create", create).await
    }

    pub async fn patch(&self, patch: &Patch) -> Result<Written, WriteError> {
        self.post("patch", patch).await
    }

    pub async fn delete(&self, delete: &Delete) -> Result<Written, WriteError> {
        self.post("delete", delete).await
    }

    pub async fn extend(&self, extend: &Extend) -> Result<Extended, WriteError> {
        self.post("extend", extend).await
    }

    /// One transaction. `send` chunks and splits so it fits.
    pub async fn execute_batch(&self, batch: &Batch) -> Result<BatchResult, WriteError> {
        self.post("execute-batch", batch).await
    }

    async fn post<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        body: &T,
    ) -> Result<R, WriteError> {
        let url = self.base.join(route).expect("a route joins onto the base");
        let response = self
            .client
            .post(url)
            .json(body)
            .timeout(TIMEOUT)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        if status == 200 {
            return serde_json::from_str(&text)
                .map_err(|_| WriteError::Unexpected { status, body: text });
        }
        let Ok(body) = serde_json::from_str::<ErrorBody>(&text) else {
            return Err(WriteError::Unexpected { status, body: text });
        };
        Err(match (status, body.pending) {
            (400, _) => WriteError::Refused(body.error),
            (413, _) => WriteError::TooLarge(body.error),
            (500, _) => WriteError::Failed(body.error),
            (504, Some(pending)) => WriteError::Unresolved {
                tx_hash: pending.tx_hash,
                errors: body.error,
            },
            _ => WriteError::Unexpected { status, body: text },
        })
    }
}

impl ChainWriter for Writer {
    async fn identity(&self) -> Result<Identity, WriteError> {
        Writer::identity(self).await
    }

    async fn create(&self, create: &Create) -> Result<Created, WriteError> {
        Writer::create(self, create).await
    }

    async fn patch(&self, patch: &Patch) -> Result<Written, WriteError> {
        Writer::patch(self, patch).await
    }

    async fn delete(&self, delete: &Delete) -> Result<Written, WriteError> {
        Writer::delete(self, delete).await
    }

    async fn execute_batch(&self, batch: &Batch) -> Result<BatchResult, WriteError> {
        Writer::execute_batch(self, batch).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::records::{Agreement, Record, Wei};
    use alloy_primitives::{Address, B256};

    fn agreement() -> EncodedRecord {
        Agreement {
            provider: Address::ZERO,
            offer: B256::ZERO,
            wei_per_call: Wei::new(1),
            remote_port: 20000,
        }
        .encode()
    }

    #[test]
    fn a_create_serializes_to_the_sidecar_shape() {
        let create = Create::new(agreement(), Expiry::Seconds(7200));
        let wire = serde_json::to_value(&create).unwrap();
        assert_eq!(wire["contentType"], "application/json");
        assert_eq!(wire["expires"], serde_json::json!({ "seconds": 7200 }));
        assert_eq!(wire["attributes"]["kind"]["value"], "rpc.agreement");
        let payload = base64::engine::general_purpose::STANDARD
            .decode(wire["payload"].as_str().unwrap())
            .unwrap();
        assert_eq!(payload, agreement().payload);
    }

    #[test]
    fn a_permanent_expiry_is_the_string() {
        assert_eq!(
            serde_json::to_value(Expiry::Permanent).unwrap(),
            serde_json::json!("permanent")
        );
    }

    fn extend(i: usize) -> Operation {
        Operation::Extend(Extend {
            entity_key: B256::with_last_byte(i as u8),
            expires: Expiry::Seconds(1),
        })
    }

    fn extends(n: usize) -> Batch {
        let mut batch = Batch::new();
        for i in 0..n {
            batch.push(Group::single(extend(i)));
        }
        batch
    }

    #[test]
    fn a_batch_serializes_to_the_sidecar_shape_by_kind() {
        let mut batch = Batch::new();
        batch.push(Group::new(vec![
            Operation::Create(Create::new(agreement(), Expiry::Seconds(2))),
            Operation::Delete(Delete {
                entity_key: B256::ZERO,
            }),
        ]));
        batch.push(Group::single(extend(1)));
        let wire = serde_json::to_value(&batch).unwrap();
        assert_eq!(wire["creates"].as_array().unwrap().len(), 1);
        assert_eq!(
            wire["creates"][0]["expires"],
            serde_json::json!({ "seconds": 2 })
        );
        assert_eq!(
            wire["deletes"],
            serde_json::json!([{ "entityKey": format!("{:#x}", B256::ZERO) }])
        );
        assert_eq!(
            wire["extensions"],
            serde_json::json!([{
                "entityKey": format!("{:#x}", B256::with_last_byte(1)),
                "expires": { "seconds": 1 },
            }])
        );
        assert!(wire.get("patches").is_none(), "an empty kind is left out");
    }

    #[test]
    fn halves_split_at_a_group_boundary_in_order() {
        let (first, second) = extends(5).halves().expect("two or more groups");
        assert_eq!(first.groups().len(), 2);
        assert_eq!(second.groups().len(), 3);
        let Operation::Extend(extend) = &second.groups()[0].operations()[0] else {
            panic!("an extend");
        };
        assert_eq!(extend.entity_key, B256::with_last_byte(2));
        assert!(extends(1).halves().is_none(), "one group cannot be split");
    }
}
