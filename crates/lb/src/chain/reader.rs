//! The read client: plain JSON-RPC against the reference endpoint,
//! with its bearer key, for everything the LB reads from the chain —
//! marketplace records, the entity count behind them, the head height.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use reqwest::{Url, header};
use serde::Deserialize;
use serde_json::{Value, json};

use super::records::{ArkivEntity, SCHEMA_VERSION};

/// The node's page maximum. Asking for more is an error, not a smaller
/// page, so no read ever asks for more.
pub const PAGE_LIMIT: u64 = 200;

/// A filter for `arkiv_query`, in the node's query language. Built from
/// typed parts so that attribute names and literal syntax are written in
/// one place; the text is conditions joined by `AND`. Every query starts
/// from a record kind at the schema version this code understands, so
/// a newer record never takes a row of the page or a unit of the count.
#[derive(Debug, Clone)]
pub struct Query(Vec<String>);

impl Query {
    pub fn kind(kind: &str) -> Self {
        Self(vec![
            format!("kind = str('{}')", escape(kind)),
            format!("v = i32({SCHEMA_VERSION})"),
        ])
    }

    pub fn creator(mut self, creator: Address) -> Self {
        self.0.push(format!("$creator = addr({creator:#x})"));
        self
    }

    pub fn attr_addr(mut self, name: &str, value: Address) -> Self {
        self.0.push(format!("{name} = addr({value:#x})"));
        self
    }

    pub fn attr_str(mut self, name: &str, value: &str) -> Self {
        self.0.push(format!("{name} = str('{}')", escape(value)));
        self
    }

    pub fn attr_u64(mut self, name: &str, value: u64) -> Self {
        self.0.push(format!("{name} = u64({value})"));
        self
    }

    /// Only records that are still alive at `head`. The node hides expired
    /// records anyway; this is boundary exactness, not correctness.
    pub fn not_expired(mut self, head: u64) -> Self {
        self.0.push(format!("$expiresAt > u64({head})"));
        self
    }

    /// Only records expiring at or before `block`: how discovery ignores
    /// offers with a lifetime longer than it accepts.
    pub fn expires_by(mut self, block: u64) -> Self {
        self.0.push(format!("$expiresAt <= u64({block})"));
        self
    }

    pub fn text(&self) -> String {
        self.0.join(" AND ")
    }
}

/// A string literal's only escape: a quote doubled.
fn escape(text: &str) -> String {
    text.replace('\'', "''")
}

/// One page of a query. `more` means the node had rows beyond the page
/// limit; the callers that must see everything check the count first.
#[derive(Debug, Clone)]
pub struct Page {
    pub entities: Vec<ArkivEntity>,
    pub more: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("the reference could not be reached: {0}")]
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
        quantity(&result)
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
        let params = json!([
            query.text(),
            {
                "select": {
                    "key": true,
                    "creator": true,
                    "expiresAt": true,
                    "payload": true,
                    "attributes": true,
                },
                "limit": format!("{PAGE_LIMIT:#x}"),
            }
        ]);
        let result = self.call("arkiv_query", params).await?;
        let result: QueryResult =
            serde_json::from_value(result).map_err(|e| ReadError::Unexpected(e.to_string()))?;
        Ok(Page {
            entities: result.data,
            more: result.cursor.is_some(),
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
        response
            .result
            .ok_or_else(|| ReadError::Unexpected("neither result nor error".to_owned()))
    }
}

/// A JSON-RPC quantity: `0x`-prefixed hex.
fn quantity(value: &Value) -> Result<u64, ReadError> {
    value
        .as_str()
        .and_then(|text| text.strip_prefix("0x"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .ok_or_else(|| ReadError::Unexpected(format!("not a hex quantity: {value}")))
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
        let query = Query::kind(KIND_OFFER)
            .attr_addr("lb", lb)
            .not_expired(1000)
            .expires_by(87_400);
        assert_eq!(
            query.text(),
            "kind = str('rpc.offer') AND v = i32(1) \
             AND lb = addr(0x411e31d7ebbfd636af234954db5f598cd80a878c) \
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
