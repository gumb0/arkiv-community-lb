//! The marketplace records, exactly as `docs/ENTITIES.md` specifies them.
//! Every record is one struct: attributes are the fields a query filters
//! by, the payload is JSON with the rest, and a field lives in one place
//! only. Encoding and decoding are symmetric, and unknown payload fields
//! are ignored so a newer writer does not break an older reader.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Schema version carried by every record as the `v` attribute.
pub const SCHEMA_VERSION: i32 = 1;

pub const KIND_LB_LISTING: &str = "rpc.lb_listing";
pub const KIND_OFFER: &str = "rpc.offer";
pub const KIND_AGREEMENT: &str = "rpc.agreement";
pub const KIND_COUNTERS: &str = "rpc.counters";
pub const KIND_RECEIPT: &str = "rpc.receipt";

pub use alloy_primitives::Address;
/// A 32-byte entity key.
pub type EntityKey = alloy_primitives::B256;
use alloy_primitives::{Bytes, U256};

/// GLM wei, serialized as a decimal string: JSON numbers cannot carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Wei(pub U256);

impl Wei {
    pub const fn new(wei: u128) -> Self {
        Self(U256::from_limbs([wei as u64, (wei >> 64) as u64, 0, 0]))
    }
}

impl Serialize for Wei {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Wei {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        U256::from_str_radix(&text, 10)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("attribute {0:?} is missing")]
    MissingAttribute(&'static str),
    #[error("attribute {name:?} has type {found:?}, expected {expected:?}")]
    AttributeType {
        name: &'static str,
        found: String,
        expected: &'static str,
    },
    #[error("payload is not valid JSON for this record: {0}")]
    Payload(String),
}

/// A typed attribute value. The tag names are the SDK's; the JSON shapes
/// differ between the write side (`{ "type", "value" }` with 64-bit
/// numbers as decimal strings) and the read side (a list with numbers as
/// the node encodes them), so both are handled here and nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeValue {
    Str(String),
    I32(i32),
    U64(u64),
    Addr(Address),
    Key(EntityKey),
}

impl AttributeValue {
    fn type_tag(&self) -> &'static str {
        match self {
            Self::Str(_) => "str",
            Self::I32(_) => "i32",
            Self::U64(_) => "u64",
            Self::Addr(_) => "addr",
            Self::Key(_) => "key",
        }
    }

    fn mismatch(&self, name: &'static str, expected: &'static str) -> RecordError {
        RecordError::AttributeType {
            name,
            found: self.type_tag().to_owned(),
            expected,
        }
    }

    /// The write-side shape, for the chain writer's `attributes` map.
    fn to_wire(&self) -> serde_json::Value {
        let value = match self {
            Self::Str(s) => serde_json::Value::String(s.clone()),
            Self::I32(n) => serde_json::Value::from(*n),
            Self::U64(n) => serde_json::Value::String(n.to_string()),
            // `{:#x}` gives lowercase hex; an address's `Display` is the
            // checksummed form, and the record spec (docs/ENTITIES.md)
            // says lowercase.
            Self::Addr(a) => serde_json::Value::String(format!("{a:#x}")),
            Self::Key(k) => serde_json::Value::String(format!("{k:#x}")),
        };
        serde_json::json!({ "type": self.type_tag(), "value": value })
    }

    /// The read-side shape: one entry of a queried entity's attribute
    /// list. A 64-bit number may arrive as a JSON number, a decimal
    /// string, or a hex quantity; all three are accepted.
    fn from_read(read: &ArkivAttribute) -> Option<Self> {
        let value = &read.value;
        match read.type_tag.as_str() {
            "str" => value.as_str().map(|s| Self::Str(s.to_owned())),
            "i32" => value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(Self::I32),
            "u64" => parse_u64(value).map(Self::U64),
            "addr" => value.as_str().and_then(|s| s.parse().ok()).map(Self::Addr),
            "key" => value.as_str().and_then(|s| s.parse().ok()).map(Self::Key),
            _ => None,
        }
    }
}

fn parse_u64(value: &serde_json::Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    let text = value.as_str()?;
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// The attributes of one record, by name. Serializes to the writer's
/// `{ "name": { "type", "value" } }` shape. A `BTreeMap` so the wire form
/// has one stable key order: the tests compare it to literals, and equal
/// records produce byte-equal request bodies.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attributes(BTreeMap<String, AttributeValue>);

impl Attributes {
    fn new(kind: &str) -> Self {
        let mut map = BTreeMap::new();
        map.insert("kind".to_owned(), AttributeValue::Str(kind.to_owned()));
        map.insert("v".to_owned(), AttributeValue::I32(SCHEMA_VERSION));
        Self(map)
    }

    fn with(mut self, name: &str, value: AttributeValue) -> Self {
        self.0.insert(name.to_owned(), value);
        self
    }

    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.0.get(name)
    }

    pub fn to_wire(&self) -> serde_json::Value {
        self.0
            .iter()
            .map(|(name, value)| (name.clone(), value.to_wire()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    fn present(&self, name: &'static str) -> Result<&AttributeValue, RecordError> {
        self.get(name).ok_or(RecordError::MissingAttribute(name))
    }

    fn str(&self, name: &'static str) -> Result<&str, RecordError> {
        match self.present(name)? {
            AttributeValue::Str(s) => Ok(s),
            other => Err(other.mismatch(name, "str")),
        }
    }

    fn u64(&self, name: &'static str) -> Result<u64, RecordError> {
        match self.present(name)? {
            AttributeValue::U64(n) => Ok(*n),
            other => Err(other.mismatch(name, "u64")),
        }
    }

    fn addr(&self, name: &'static str) -> Result<Address, RecordError> {
        match self.present(name)? {
            AttributeValue::Addr(a) => Ok(*a),
            other => Err(other.mismatch(name, "addr")),
        }
    }

    fn key(&self, name: &'static str) -> Result<EntityKey, RecordError> {
        match self.present(name)? {
            AttributeValue::Key(k) => Ok(*k),
            other => Err(other.mismatch(name, "key")),
        }
    }
}

/// One attribute as `arkiv_query` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct ArkivAttribute {
    pub name: String,
    #[serde(rename = "type")]
    pub type_tag: String,
    pub value: serde_json::Value,
}

/// An entity as `arkiv_query` returns it, with the fields every read
/// selects. The payload arrives as `0x`-prefixed hex.
#[derive(Debug, Clone, Deserialize)]
pub struct ArkivEntity {
    pub key: EntityKey,
    pub creator: Address,
    #[serde(rename = "expiresAt", deserialize_with = "deserialize_quantity")]
    pub expires_at: u64,
    pub payload: Bytes,
    /// Absent for an entity without attributes, possibly; such a row must
    /// still parse, and then it is simply not one of our records.
    #[serde(default)]
    pub attributes: Vec<ArkivAttribute>,
}

fn deserialize_quantity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    parse_u64(&value).ok_or_else(|| serde::de::Error::custom(format!("not a quantity: {value}")))
}

impl ArkivEntity {
    /// The attributes typed, with entries of unknown type left out.
    pub fn attributes(&self) -> Attributes {
        Attributes(
            self.attributes
                .iter()
                .filter_map(|read| AttributeValue::from_read(read).map(|v| (read.name.clone(), v)))
                .collect(),
        )
    }

    /// Whether this entity is a record of the given kind at the schema
    /// version this code understands. Anything else is skipped, never
    /// parsed: a reader must not misread a newer record.
    pub fn is(&self, kind: &str) -> bool {
        let attributes = self.attributes();
        let same_kind = match attributes.get("kind") {
            Some(AttributeValue::Str(found)) => found == kind,
            _ => false,
        };
        same_kind && attributes.get("v") == Some(&AttributeValue::I32(SCHEMA_VERSION))
    }

    fn payload<T: DeserializeOwned>(&self) -> Result<T, RecordError> {
        serde_json::from_slice(&self.payload).map_err(|e| RecordError::Payload(e.to_string()))
    }
}

/// What a record contributes to a write: its attributes and its payload
/// bytes. The chain writer adds the lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRecord {
    pub attributes: Attributes,
    pub payload: Vec<u8>,
}

fn encode<T: Serialize>(attributes: Attributes, payload: &T) -> EncodedRecord {
    EncodedRecord {
        attributes,
        payload: serde_json::to_vec(payload).expect("record payloads always serialize"),
    }
}

// ---------------------------------------------------------------------------
// LB listing

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbListing {
    pub wei_per_call: Wei,
    pub tunnel_server: String,
    pub max_providers: u32,
}

impl LbListing {
    pub fn encode(&self) -> EncodedRecord {
        encode(Attributes::new(KIND_LB_LISTING), self)
    }

    pub fn decode(entity: &ArkivEntity) -> Result<Self, RecordError> {
        entity.payload()
    }
}

// ---------------------------------------------------------------------------
// Offer

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Specs {
    pub chain_id: u64,
    pub head: u64,
    pub el: String,
    pub cl: String,
    pub hw: Hardware,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub cpus: u32,
    pub mem_gb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OfferPayload {
    specs: Specs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The LB the offer addresses.
    pub lb: Address,
    pub specs: Specs,
}

impl Offer {
    pub fn encode(&self) -> EncodedRecord {
        encode(
            Attributes::new(KIND_OFFER).with("lb", AttributeValue::Addr(self.lb)),
            &OfferPayload {
                specs: self.specs.clone(),
            },
        )
    }

    pub fn decode(entity: &ArkivEntity) -> Result<Self, RecordError> {
        let lb = entity.attributes().addr("lb")?;
        let payload: OfferPayload = entity.payload()?;
        Ok(Self {
            lb,
            specs: payload.specs,
        })
    }
}

// ---------------------------------------------------------------------------
// Agreement record

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgreementPayload {
    wei_per_call: Wei,
    remote_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreement {
    pub provider: Address,
    pub wei_per_call: Wei,
    pub remote_port: u16,
}

impl Agreement {
    pub fn encode(&self) -> EncodedRecord {
        encode(
            Attributes::new(KIND_AGREEMENT).with("provider", AttributeValue::Addr(self.provider)),
            &AgreementPayload {
                wei_per_call: self.wei_per_call,
                remote_port: self.remote_port,
            },
        )
    }

    pub fn decode(entity: &ArkivEntity) -> Result<Self, RecordError> {
        let provider = entity.attributes().addr("provider")?;
        let payload: AgreementPayload = entity.payload()?;
        Ok(Self {
            provider,
            wei_per_call: payload.wei_per_call,
            remote_port: payload.remote_port,
        })
    }
}

// ---------------------------------------------------------------------------
// Counters

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodState {
    Open,
    Closed,
}

impl PeriodState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterRow {
    pub agreement: EntityKey,
    pub provider: Address,
    pub count: u64,
    pub wei_per_call: Wei,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CountersPayload {
    period_end: u64,
    rows: Vec<CounterRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counters {
    /// The period's start, unix seconds.
    pub period: u64,
    pub state: PeriodState,
    pub period_end: u64,
    pub rows: Vec<CounterRow>,
}

impl Counters {
    pub fn encode(&self) -> EncodedRecord {
        encode(
            Attributes::new(KIND_COUNTERS)
                .with("period", AttributeValue::U64(self.period))
                .with("state", AttributeValue::Str(self.state.as_str().to_owned())),
            &CountersPayload {
                period_end: self.period_end,
                rows: self.rows.clone(),
            },
        )
    }

    pub fn decode(entity: &ArkivEntity) -> Result<Self, RecordError> {
        let attributes = entity.attributes();
        let period = attributes.u64("period")?;
        let state = match attributes.str("state")? {
            "open" => PeriodState::Open,
            "closed" => PeriodState::Closed,
            other => {
                return Err(RecordError::AttributeType {
                    name: "state",
                    found: other.to_owned(),
                    expected: "open or closed",
                });
            }
        };
        let payload: CountersPayload = entity.payload()?;
        Ok(Self {
            period,
            state,
            period_end: payload.period_end,
            rows: payload.rows,
        })
    }
}

// ---------------------------------------------------------------------------
// Receipt

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    pub chain_id: u64,
    /// The transfer's transaction hash; `None` for a zero-amount receipt.
    pub tx: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptPayload {
    count: u64,
    wei_per_call: Wei,
    amount_wei: Wei,
    payout: Payout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub period: u64,
    pub agreement: EntityKey,
    pub provider: Address,
    pub count: u64,
    pub wei_per_call: Wei,
    pub amount_wei: Wei,
    pub payout: Payout,
}

impl Receipt {
    pub fn encode(&self) -> EncodedRecord {
        encode(
            Attributes::new(KIND_RECEIPT)
                .with("period", AttributeValue::U64(self.period))
                .with("agreement", AttributeValue::Key(self.agreement))
                .with("provider", AttributeValue::Addr(self.provider)),
            &ReceiptPayload {
                count: self.count,
                wei_per_call: self.wei_per_call,
                amount_wei: self.amount_wei,
                payout: self.payout.clone(),
            },
        )
    }

    pub fn decode(entity: &ArkivEntity) -> Result<Self, RecordError> {
        let attributes = entity.attributes();
        let payload: ReceiptPayload = entity.payload()?;
        Ok(Self {
            period: attributes.u64("period")?,
            agreement: attributes.key("agreement")?,
            provider: attributes.addr("provider")?,
            count: payload.count,
            wei_per_call: payload.wei_per_call,
            amount_wei: payload.amount_wei,
            payout: payload.payout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LB: &str = "0x411e31d7ebbfd636af234954db5f598cd80a878c";
    const PROVIDER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    const KEY: &str = "0x8863000000000000000000000000000000000000000000000000000000009057";

    fn addr(text: &str) -> Address {
        text.parse().expect("test address parses")
    }

    fn key(text: &str) -> EntityKey {
        text.parse().expect("test key parses")
    }

    fn payload_json(encoded: &EncodedRecord) -> serde_json::Value {
        serde_json::from_slice(&encoded.payload).expect("payloads are JSON")
    }

    /// Builds the entity a read would return for what a write sent: the
    /// payload as hex, the attributes as the node's list.
    fn read_back(encoded: &EncodedRecord) -> ArkivEntity {
        let attributes = encoded
            .attributes
            .0
            .iter()
            .map(|(name, value)| {
                let wire = value.to_wire();
                json!({ "name": name, "type": wire["type"], "value": wire["value"] })
            })
            .collect::<Vec<_>>();
        let payload_hex = encoded
            .payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        serde_json::from_value(json!({
            "key": KEY,
            "creator": LB,
            "expiresAt": "0x92e21",
            "payload": format!("0x{payload_hex}"),
            "attributes": attributes,
        }))
        .expect("a read row parses")
    }

    #[test]
    fn listing_round_trips() {
        let listing = LbListing {
            wei_per_call: Wei::new(1_000_000_000_000_000),
            tunnel_server: "203.0.113.10:7000".to_owned(),
            max_providers: 100,
        };
        let encoded = listing.encode();
        assert_eq!(
            encoded.attributes.to_wire(),
            json!({
                "kind": { "type": "str", "value": "rpc.lb_listing" },
                "v": { "type": "i32", "value": 1 },
            })
        );
        assert_eq!(
            payload_json(&encoded),
            json!({
                "wei_per_call": "1000000000000000",
                "tunnel_server": "203.0.113.10:7000",
                "max_providers": 100,
            })
        );
        let entity = read_back(&encoded);
        assert!(entity.is(KIND_LB_LISTING));
        assert_eq!(LbListing::decode(&entity).unwrap(), listing);
    }

    #[test]
    fn offer_round_trips() {
        let offer = Offer {
            lb: addr(LB),
            specs: Specs {
                chain_id: 7_738_577,
                head: 123_456,
                el: "arkiv-reth/v0.2.0".to_owned(),
                cl: "lighthouse/v8.2.1".to_owned(),
                hw: Hardware {
                    cpus: 8,
                    mem_gb: 32,
                },
            },
        };
        let encoded = offer.encode();
        assert_eq!(
            encoded.attributes.to_wire()["lb"],
            json!({ "type": "addr", "value": LB })
        );
        assert_eq!(
            payload_json(&encoded),
            json!({
                "specs": {
                    "chain_id": 7738577,
                    "head": 123456,
                    "el": "arkiv-reth/v0.2.0",
                    "cl": "lighthouse/v8.2.1",
                    "hw": { "cpus": 8, "mem_gb": 32 },
                }
            })
        );
        let entity = read_back(&encoded);
        assert!(entity.is(KIND_OFFER));
        assert_eq!(Offer::decode(&entity).unwrap(), offer);
    }

    #[test]
    fn agreement_round_trips() {
        let agreement = Agreement {
            provider: addr(PROVIDER),
            wei_per_call: Wei::new(1_000_000_000_000_000),
            remote_port: 20007,
        };
        let encoded = agreement.encode();
        assert_eq!(
            encoded.attributes.to_wire()["provider"],
            json!({ "type": "addr", "value": PROVIDER })
        );
        assert_eq!(
            payload_json(&encoded),
            json!({ "wei_per_call": "1000000000000000", "remote_port": 20007 })
        );
        let entity = read_back(&encoded);
        assert!(entity.is(KIND_AGREEMENT));
        assert_eq!(Agreement::decode(&entity).unwrap(), agreement);
    }

    #[test]
    fn counters_round_trip_with_the_period_as_attributes() {
        let counters = Counters {
            period: 1_789_000_000,
            state: PeriodState::Closed,
            period_end: 1_789_086_400,
            rows: vec![CounterRow {
                agreement: key(KEY),
                provider: addr(PROVIDER),
                count: 48_213,
                wei_per_call: Wei::new(1_000_000_000_000_000),
            }],
        };
        let encoded = counters.encode();
        let wire = encoded.attributes.to_wire();
        assert_eq!(
            wire["period"],
            json!({ "type": "u64", "value": "1789000000" })
        );
        assert_eq!(wire["state"], json!({ "type": "str", "value": "closed" }));
        assert_eq!(
            payload_json(&encoded),
            json!({
                "period_end": 1789086400,
                "rows": [{
                    "agreement": KEY,
                    "provider": PROVIDER,
                    "count": 48213,
                    "wei_per_call": "1000000000000000",
                }]
            })
        );
        let entity = read_back(&encoded);
        assert!(entity.is(KIND_COUNTERS));
        assert_eq!(Counters::decode(&entity).unwrap(), counters);
    }

    #[test]
    fn receipt_round_trips() {
        let tx = "0x925d000000000000000000000000000000000000000000000000000000000033c7";
        let receipt = Receipt {
            period: 1_789_000_000,
            agreement: key(KEY),
            provider: addr(PROVIDER),
            count: 48_213,
            wei_per_call: Wei::new(1_000_000_000_000_000),
            amount_wei: Wei::new(48_213_000_000_000_000_000),
            payout: Payout {
                chain_id: 560_048,
                tx: Some(tx.to_owned()),
            },
        };
        let encoded = receipt.encode();
        let wire = encoded.attributes.to_wire();
        assert_eq!(
            wire["period"],
            json!({ "type": "u64", "value": "1789000000" })
        );
        assert_eq!(wire["agreement"], json!({ "type": "key", "value": KEY }));
        assert_eq!(
            wire["provider"],
            json!({ "type": "addr", "value": PROVIDER })
        );
        assert_eq!(
            payload_json(&encoded),
            json!({
                "count": 48213,
                "wei_per_call": "1000000000000000",
                "amount_wei": "48213000000000000000",
                "payout": { "chain_id": 560048, "tx": tx },
            })
        );
        let entity = read_back(&encoded);
        assert!(entity.is(KIND_RECEIPT));
        assert_eq!(Receipt::decode(&entity).unwrap(), receipt);
    }

    #[test]
    fn a_zero_receipt_has_no_transaction() {
        let receipt = Receipt {
            period: 1_789_000_000,
            agreement: key(KEY),
            provider: addr(PROVIDER),
            count: 0,
            wei_per_call: Wei::new(1_000_000_000_000_000),
            amount_wei: Wei::new(0),
            payout: Payout {
                chain_id: 560_048,
                tx: None,
            },
        };
        let encoded = receipt.encode();
        assert_eq!(payload_json(&encoded)["payout"]["tx"], json!(null));
        assert_eq!(Receipt::decode(&read_back(&encoded)).unwrap(), receipt);
    }

    #[test]
    fn unknown_payload_fields_are_ignored() {
        let mut encoded = Agreement {
            provider: addr(PROVIDER),
            wei_per_call: Wei::new(5),
            remote_port: 20000,
        }
        .encode();
        encoded.payload =
            br#"{"wei_per_call":"5","remote_port":20000,"added_later":true}"#.to_vec();
        assert!(Agreement::decode(&read_back(&encoded)).is_ok());
    }

    #[test]
    fn another_kind_or_version_is_not_this_record() {
        let encoded = LbListing {
            wei_per_call: Wei::new(1),
            tunnel_server: "h:1".to_owned(),
            max_providers: 1,
        }
        .encode();
        let entity = read_back(&encoded);
        assert!(!entity.is(KIND_OFFER));

        let newer = EncodedRecord {
            attributes: encoded.attributes.clone().with("v", AttributeValue::I32(2)),
            payload: encoded.payload.clone(),
        };
        assert!(!read_back(&newer).is(KIND_LB_LISTING));
    }

    #[test]
    fn a_missing_attribute_is_an_error() {
        let mut encoded = Agreement {
            provider: addr(PROVIDER),
            wei_per_call: Wei::new(5),
            remote_port: 20000,
        }
        .encode();
        encoded.attributes.0.remove("provider");
        assert_eq!(
            Agreement::decode(&read_back(&encoded)),
            Err(RecordError::MissingAttribute("provider"))
        );
    }

    #[test]
    fn an_attribute_of_the_wrong_type_is_an_error() {
        let mut encoded = Agreement {
            provider: addr(PROVIDER),
            wei_per_call: Wei::new(5),
            remote_port: 20000,
        }
        .encode();
        // The provider written as a string, where the record needs an address.
        encoded.attributes = encoded
            .attributes
            .with("provider", AttributeValue::Str(PROVIDER.to_owned()));
        assert_eq!(
            Agreement::decode(&read_back(&encoded)),
            Err(RecordError::AttributeType {
                name: "provider",
                found: "str".to_owned(),
                expected: "addr",
            })
        );
    }

    #[test]
    fn sixty_four_bit_attributes_read_in_every_encoding() {
        for value in [
            json!(1789000000u64),
            json!("1789000000"),
            json!("0x6aa1f940"),
        ] {
            let read = ArkivAttribute {
                name: "period".to_owned(),
                type_tag: "u64".to_owned(),
                value,
            };
            assert_eq!(
                AttributeValue::from_read(&read),
                Some(AttributeValue::U64(1_789_000_000))
            );
        }
    }

    #[test]
    fn addresses_and_keys_encode_as_lowercase_hex() {
        let mixed: Address = "0x411E31d7eBbfd636Af234954db5f598Cd80a878C"
            .parse()
            .unwrap();
        assert_eq!(
            AttributeValue::Addr(mixed).to_wire()["value"],
            json!("0x411e31d7ebbfd636af234954db5f598cd80a878c")
        );
        let mixed_key = key("0xABCDEF0000000000000000000000000000000000000000000000000000abcdef");
        assert_eq!(
            AttributeValue::Key(mixed_key).to_wire()["value"],
            json!("0xabcdef0000000000000000000000000000000000000000000000000000abcdef")
        );
        assert!("0x1234".parse::<Address>().is_err());
        assert!(LB.parse::<EntityKey>().is_err());
    }

    #[test]
    fn wei_is_a_decimal_string() {
        assert_eq!(
            serde_json::to_string(&Wei::new(u128::MAX)).unwrap(),
            format!("\"{}\"", u128::MAX)
        );
        assert!(serde_json::from_str::<Wei>("1000").is_err());
        assert!(serde_json::from_str::<Wei>("\"0x3e8\"").is_err());
        assert_eq!(
            serde_json::from_str::<Wei>("\"1000\"").unwrap(),
            Wei::new(1000)
        );
    }
}
