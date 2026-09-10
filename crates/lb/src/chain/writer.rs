//! The client for the chain-writer sidecar (`docs/CHAIN_WRITER.md`): five
//! POST routes, JSON bodies, and three outcomes for every write. A 504 is
//! the one to handle with care: the transaction was sent and may still
//! land, so the caller polls its hash and never resends.

use std::time::Duration;

use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::records::{Attributes, EncodedRecord, EntityKey};

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

/// Several extends in one transaction: all land or none does. Extends are
/// the only operation the LB batches — the hourly refresh, one per
/// eligible provider plus the listing — so the type holds nothing else;
/// every other write goes one operation at a time. The sidecar's route
/// accepts creates, patches and deletes too, if a batch of those is ever
/// wanted.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Batch {
    pub extensions: Vec<Extend>,
}

/// Extends per transaction. An extend is 10,000 gas flat and about a
/// hundred bytes of calldata, so this many stay an order of magnitude
/// under the node's caps of 30 M gas and 128 KiB per transaction.
pub const MAX_BATCH_OPS: usize = 200;

impl Batch {
    /// Splits into batches of at most `MAX_BATCH_OPS` extends, in order.
    pub fn chunk(self) -> Vec<Batch> {
        let mut extensions = self.extensions;
        let mut chunks = Vec::new();
        while !extensions.is_empty() {
            let rest = extensions.split_off(extensions.len().min(MAX_BATCH_OPS));
            chunks.push(Batch { extensions });
            extensions = rest;
        }
        chunks
    }
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

/// A landed batch, with the keys it extended, in operation order.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub tx_hash: String,
    #[serde(default)]
    pub extended_entities: Vec<EntityKey>,
}

fn deserialize_decimal<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

/// One link of the sidecar's walked error chain.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorLink {
    pub name: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// 400: the body did not decode; nothing was sent.
    #[error("the sidecar refused the request: {}", causes(.0))]
    Refused(Vec<ErrorLink>),
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
        .map(|link| format!("{}: {}", link.name, link.message))
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

    /// One transaction. The caller chunks (`Batch::chunk`) so it fits.
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
            (500, _) => WriteError::Failed(body.error),
            (504, Some(pending)) => WriteError::Unresolved {
                tx_hash: pending.tx_hash,
                errors: body.error,
            },
            _ => WriteError::Unexpected { status, body: text },
        })
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

    fn extends(n: usize) -> Batch {
        Batch {
            extensions: (0..n)
                .map(|i| Extend {
                    entity_key: B256::with_last_byte(i as u8),
                    expires: Expiry::Seconds(1),
                })
                .collect(),
        }
    }

    #[test]
    fn a_batch_serializes_to_the_sidecar_shape() {
        let wire = serde_json::to_value(extends(1)).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "extensions": [{
                "entityKey": format!("{:#x}", B256::ZERO),
                "expires": { "seconds": 1 },
            }] })
        );
    }

    #[test]
    fn a_batch_within_the_cap_is_one_chunk() {
        let chunks = extends(MAX_BATCH_OPS).chunk();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].extensions.len(), MAX_BATCH_OPS);
    }

    #[test]
    fn chunking_splits_at_the_cap_and_keeps_order() {
        let chunks = extends(MAX_BATCH_OPS + 3).chunk();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].extensions.len(), MAX_BATCH_OPS);
        assert_eq!(chunks[1].extensions.len(), 3);
        let last_of_first = chunks[0].extensions[MAX_BATCH_OPS - 1].entity_key;
        assert_eq!(
            last_of_first,
            B256::with_last_byte((MAX_BATCH_OPS - 1) as u8)
        );
        assert_eq!(
            chunks[1].extensions[0].entity_key,
            B256::with_last_byte(MAX_BATCH_OPS as u8)
        );
    }

    #[test]
    fn an_empty_batch_has_no_chunks() {
        assert!(Batch::default().chunk().is_empty());
    }
}
