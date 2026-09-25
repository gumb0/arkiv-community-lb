//! The read client: plain JSON-RPC against the reference endpoint,
//! with its bearer key, for everything the LB reads from the chain —
//! marketplace records, the entity count behind them, the head height.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use reqwest::{Url, header};
use serde::Deserialize;
use serde_json::{Value, json};

use super::records::{ArkivEntity, AttributeValue, EntityKey, SCHEMA_VERSION, parse_u64};

/// The node's page maximum. Asking for more is an error, not a smaller
/// page, so no read ever asks for more.
pub const PAGE_LIMIT: u64 = 200;

/// A filter for `arkiv_query`. Built from typed conditions, so that a
/// fake chain can evaluate the same query the node is sent; `text()`
/// renders them in the node's query language, joined by `AND`. Every
/// query starts from a record kind at the schema version this code
/// understands, so a newer record never takes a row of the page or a
/// unit of the count.
#[derive(Debug, Clone)]
pub struct Query {
    pub conditions: Vec<Condition>,
    /// The block the query is answered at, instead of the head. Not a
    /// condition: it goes in the request's options, not its text.
    pub at_block: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Creator(Address),
    Attribute(&'static str, AttributeValue),
    /// `$expiresAt > head`: only records still alive at `head`. The node
    /// hides expired records anyway; this is boundary exactness.
    ExpiresAfter(u64),
    /// `$expiresAt <= block`: how discovery ignores offers with a
    /// lifetime longer than it accepts.
    ExpiresBy(u64),
}

impl Query {
    pub fn kind(kind: &str) -> Self {
        Self {
            conditions: vec![
                Condition::Attribute("kind", AttributeValue::Str(kind.to_owned())),
                Condition::Attribute("v", AttributeValue::I32(SCHEMA_VERSION)),
            ],
            at_block: None,
        }
    }

    /// Answered at this block rather than at the head.
    pub fn at_block(mut self, block: u64) -> Self {
        self.at_block = Some(block);
        self
    }

    pub fn creator(mut self, creator: Address) -> Self {
        self.conditions.push(Condition::Creator(creator));
        self
    }

    pub fn attr_addr(self, name: &'static str, value: Address) -> Self {
        self.attribute(name, AttributeValue::Addr(value))
    }

    pub fn attr_str(self, name: &'static str, value: &str) -> Self {
        self.attribute(name, AttributeValue::Str(value.to_owned()))
    }

    pub fn attr_u64(self, name: &'static str, value: u64) -> Self {
        self.attribute(name, AttributeValue::U64(value))
    }

    pub fn attr_key(self, name: &'static str, value: EntityKey) -> Self {
        self.attribute(name, AttributeValue::Key(value))
    }

    fn attribute(mut self, name: &'static str, value: AttributeValue) -> Self {
        self.conditions.push(Condition::Attribute(name, value));
        self
    }

    pub fn expires_after(mut self, head: u64) -> Self {
        self.conditions.push(Condition::ExpiresAfter(head));
        self
    }

    pub fn expires_by(mut self, block: u64) -> Self {
        self.conditions.push(Condition::ExpiresBy(block));
        self
    }

    pub fn text(&self) -> String {
        self.conditions
            .iter()
            .map(Condition::text)
            .collect::<Vec<_>>()
            .join(" AND ")
    }
}

impl Condition {
    fn text(&self) -> String {
        match self {
            Self::Creator(creator) => format!("$creator = addr({creator:#x})"),
            Self::Attribute(name, value) => format!("{name} = {}", literal(value)),
            Self::ExpiresAfter(head) => format!("$expiresAt > u64({head})"),
            Self::ExpiresBy(block) => format!("$expiresAt <= u64({block})"),
        }
    }
}

/// A typed literal in the query language. A string's only escape is a
/// quote doubled.
fn literal(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Str(s) => format!("str('{}')", s.replace('\'', "''")),
        AttributeValue::I32(n) => format!("i32({n})"),
        AttributeValue::U64(n) => format!("u64({n})"),
        AttributeValue::Addr(a) => format!("addr({a:#x})"),
        AttributeValue::Key(k) => format!("key({k:#x})"),
    }
}

/// One page of a query. `more` means the node had rows beyond the page
/// limit; the callers that must see everything check the count first.
/// `block` is the block the node answered at: the head, unless the
/// query was pinned.
#[derive(Debug, Clone)]
pub struct Page {
    pub entities: Vec<ArkivEntity>,
    pub more: bool,
    pub block: u64,
}

/// Which block to read: the latest finalized one, or one by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockAt {
    Finalized,
    Number(u64),
}

/// The fields of a block that say whether two nodes hold the same one:
/// the hashes and roots the header commits to, and the transaction
/// hashes the body is made of. Nothing else, so the JSON rendering of
/// other fields, which differs between client versions, plays no part.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockFields {
    #[serde(deserialize_with = "super::records::deserialize_quantity")]
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub transactions: Vec<B256>,
}

impl BlockAt {
    fn param(self) -> Value {
        match self {
            Self::Finalized => json!("finalized"),
            Self::Number(number) => json!(format!("{number:#x}")),
        }
    }
}

/// What the LB reads from the chain. `Reader` is the real one; a test
/// implements it over an in-memory store. The futures are `Send` so a
/// generic caller can run under `tokio::spawn`.
pub trait ChainReader: Send + Sync {
    fn block_number(&self) -> impl Future<Output = Result<u64, ReadError>> + Send;
    fn block(
        &self,
        at: BlockAt,
    ) -> impl Future<Output = Result<Option<BlockFields>, ReadError>> + Send;
    fn balance(&self, account: Address) -> impl Future<Output = Result<U256, ReadError>> + Send;
    fn query(&self, query: &Query) -> impl Future<Output = Result<Page, ReadError>> + Send;
    fn count(&self, query: &Query) -> impl Future<Output = Result<u64, ReadError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("the reference could not be reached: {}", super::transport_causes(.0))]
    Transport(#[from] reqwest::Error),
    #[error("the reference answered {0}")]
    Status(u16),
    #[error("the reference returned an error: {code} {message}")]
    Rpc { code: i64, message: String },
    #[error("the reference answered with an unexpected shape: {0}")]
    Unexpected(String),
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct QueryResult {
    #[serde(default)]
    data: Vec<ArkivEntity>,
    cursor: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Value,
}

#[derive(Clone)]
pub struct Reader {
    client: reqwest::Client,
    url: Url,
    key: Option<String>,
    timeout: Duration,
}

impl Reader {
    /// The client is the service's shared one; the timeout applies per
    /// request.
    pub fn new(client: reqwest::Client, url: Url, key: Option<String>, timeout: Duration) -> Self {
        Self {
            client,
            url,
            key,
            timeout,
        }
    }

    /// The head height.
    pub async fn block_number(&self) -> Result<u64, ReadError> {
        let result = self.call("eth_blockNumber", json!([])).await?;
        parse_u64(&result).ok_or_else(|| ReadError::Unexpected(format!("not a quantity: {result}")))
    }

    /// The fields that identify a block, with its transactions as
    /// hashes; `None` when the node does not have the block.
    pub async fn block(&self, at: BlockAt) -> Result<Option<BlockFields>, ReadError> {
        let result = self
            .call("eth_getBlockByNumber", json!([at.param(), false]))
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| ReadError::Unexpected(format!("not a block: {e}")))
    }

    /// An account's balance in wei, at the head. The LB asks about its
    /// own key: a dry key stops every write.
    pub async fn balance(&self, account: Address) -> Result<U256, ReadError> {
        let params = json!([format!("{account:#x}"), "latest"]);
        let result = self.call("eth_getBalance", params).await?;
        // alloy's U256 reads the JSON-RPC hex quantity itself.
        serde_json::from_value(result.clone())
            .map_err(|_| ReadError::Unexpected(format!("not a hex quantity: {result}")))
    }

    /// One page of records matching the query, with every field a record
    /// needs selected.
    pub async fn query(&self, query: &Query) -> Result<Page, ReadError> {
        let mut options = json!({
            "select": {
                "key": true,
                "creator": true,
                "createdAt": true,
                "expiresAt": true,
                "payload": true,
                "attributes": true,
            },
            "limit": format!("{PAGE_LIMIT:#x}"),
        });
        if let Some(block) = query.at_block {
            options["atBlock"] = json!(format!("{block:#x}"));
        }
        let result = self
            .call("arkiv_query", json!([query.text(), options]))
            .await?;
        let result: QueryResult =
            serde_json::from_value(result).map_err(|e| ReadError::Unexpected(e.to_string()))?;
        let block = parse_u64(&result.block_number).ok_or_else(|| {
            ReadError::Unexpected(format!(
                "blockNumber is not a quantity: {}",
                result.block_number
            ))
        })?;
        Ok(Page {
            entities: result.data,
            more: result.cursor.is_some(),
            block,
        })
    }

    /// How many records match the query, without paging.
    pub async fn count(&self, query: &Query) -> Result<u64, ReadError> {
        let result = self
            .call("arkiv_getEntityCount", json!([{ "query": query.text() }]))
            .await?;
        result
            .as_u64()
            .ok_or_else(|| ReadError::Unexpected(format!("count is not a number: {result}")))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ReadError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut request = self
            .client
            .post(self.url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .timeout(self.timeout);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(ReadError::Status(status.as_u16()));
        }
        let response: RpcResponse = response
            .json()
            .await
            .map_err(|e| ReadError::Unexpected(e.to_string()))?;
        if let Some(error) = response.error {
            return Err(ReadError::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        // A null result is an answer (a block the node does not have);
        // an absent one reads the same, and the caller's parse says
        // what it expected instead.
        Ok(response.result.unwrap_or(Value::Null))
    }
}

impl ChainReader for Reader {
    async fn block_number(&self) -> Result<u64, ReadError> {
        Reader::block_number(self).await
    }

    async fn block(&self, at: BlockAt) -> Result<Option<BlockFields>, ReadError> {
        Reader::block(self, at).await
    }

    async fn balance(&self, account: Address) -> Result<U256, ReadError> {
        Reader::balance(self, account).await
    }

    async fn query(&self, query: &Query) -> Result<Page, ReadError> {
        Reader::query(self, query).await
    }

    async fn count(&self, query: &Query) -> Result<u64, ReadError> {
        Reader::count(self, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::records::KIND_OFFER;

    #[test]
    fn a_query_is_typed_literals_joined_by_and() {
        let lb: Address = "0x411e31d7ebbfd636af234954db5f598cd80a878c"
            .parse()
            .unwrap();
        let listing: EntityKey =
            "0x8863000000000000000000000000000000000000000000000000000000009057"
                .parse()
                .unwrap();
        let query = Query::kind(KIND_OFFER)
            .attr_addr("lb", lb)
            .attr_key("lb_listing", listing)
            .expires_after(1000)
            .expires_by(87_400);
        assert_eq!(
            query.text(),
            "kind = str('rpc.offer') AND v = i32(1) \
             AND lb = addr(0x411e31d7ebbfd636af234954db5f598cd80a878c) \
             AND lb_listing = key(0x8863000000000000000000000000000000000000000000000000000000009057) \
             AND $expiresAt > u64(1000) AND $expiresAt <= u64(87400)"
        );
    }

    #[test]
    fn a_quote_in_a_string_literal_is_doubled() {
        assert_eq!(
            Query::kind("it's").attr_str("state", "o'k").text(),
            "kind = str('it''s') AND v = i32(1) AND state = str('o''k')"
        );
    }

    #[test]
    fn the_page_limit_is_the_node_maximum() {
        assert_eq!(PAGE_LIMIT, 200);
    }
}
