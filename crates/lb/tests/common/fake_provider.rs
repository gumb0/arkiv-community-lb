//! A provider that speaks real JSON-RPC, for the suites that run the
//! LB against fake nodes: a node serving a fake chain, whose height is
//! how much of that chain it has. It answers block and entity reads
//! as an honest node would, and it can be told to lie in the ways the
//! integrity checks are meant to catch: a wrong block, a wrong entity,
//! a head kept behind.

use std::{
    net::SocketAddr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256, keccak256};
use axum::{Router, body::Bytes, http::HeaderMap, response::IntoResponse};
use lb::chain::{
    ChainReader,
    reader::{BlockAt, BlockFields},
    records::ArkivEntity,
};
use serde_json::{Value, json};

use super::fake_chain::FakeChain;

/// A provider that answers `eth_blockNumber` and `eth_chainId` from
/// settable state, with a switch to play dead (503 to everything), and
/// `eth_getBlockByNumber` and `arkiv_query` by key from the chain it
/// serves, up to its own height.
pub struct Rpc {
    pub height: AtomicU64,
    pub chain_id: AtomicU64,
    pub down: AtomicBool,
    /// Client requests answered. Probes send id 0, the tests' client
    /// sends id 1 — the only way to tell them apart at the fake.
    pub served: AtomicU64,
    /// Every request that arrived, probes and errors included.
    pub requests: AtomicU64,
    /// The last `Authorization` header seen, if any request carried one.
    pub authorization: std::sync::Mutex<Option<String>>,
    /// The chain it serves. A provider made by id gets a chain of its
    /// own that nothing else writes to; one made on a shared chain
    /// serves what the reference and the other providers see.
    chain: FakeChain,
    /// Block reads and entity reads answered, the integrity reads.
    pub blocks: AtomicU64,
    pub queries: AtomicU64,
    /// Lies, while the switch is on. A wrong block: every block
    /// answered with another hash. A wrong entity: every entity
    /// answered with an altered payload. Every, not one: a lie that
    /// should stop is the switch turned off between reads, and no test
    /// has needed a provider honest about some keys and not others.
    pub lie_block: AtomicBool,
    pub lie_entity: AtomicBool,
    /// Block and entity reads held back this long, to time them out
    /// while the probes, which have the shorter timeout, stay quick.
    pub delay_ms: AtomicU64,
}

pub async fn rpc_provider(chain_id: u64) -> (SocketAddr, Arc<Rpc>) {
    serve(FakeChain::new(Address::ZERO, chain_id)).await
}

/// A provider serving the fake chain, at the chain's id.
pub async fn rpc_provider_on(chain: &FakeChain) -> (SocketAddr, Arc<Rpc>) {
    serve(chain.clone()).await
}

async fn serve(chain: FakeChain) -> (SocketAddr, Arc<Rpc>) {
    let rpc = Arc::new(Rpc {
        height: AtomicU64::new(1),
        chain_id: AtomicU64::new(chain.chain_id()),
        down: AtomicBool::new(false),
        served: AtomicU64::new(0),
        requests: AtomicU64::new(0),
        authorization: std::sync::Mutex::new(None),
        chain,
        blocks: AtomicU64::new(0),
        queries: AtomicU64::new(0),
        lie_block: AtomicBool::new(false),
        lie_entity: AtomicBool::new(false),
        delay_ms: AtomicU64::new(0),
    });
    let state = rpc.clone();
    let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let state = state.clone();
        async move {
            state.requests.fetch_add(1, Ordering::Relaxed);
            if let Some(value) = headers.get("authorization") {
                *state.authorization.lock().expect("authorization") =
                    Some(value.to_str().unwrap_or_default().to_string());
            }
            if state.down.load(Ordering::Relaxed) {
                return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
            }
            let request: Value = serde_json::from_slice(&body).expect("json request");
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            if id != json!(0) {
                state.served.fetch_add(1, Ordering::Relaxed);
            }
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            let result = match request.get("method").and_then(Value::as_str) {
                Some("eth_blockNumber") => {
                    json!(format!("{:#x}", state.height.load(Ordering::Relaxed)))
                }
                Some("eth_chainId") => {
                    json!(format!("{:#x}", state.chain_id.load(Ordering::Relaxed)))
                }
                Some("eth_getBlockByNumber") => state.block(&params).await,
                Some("arkiv_query") => state.query(&params).await,
                other => panic!("unexpected method probed: {other:?}"),
            };
            axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (addr, rpc)
}

impl Rpc {
    async fn delay(&self) {
        let delay = self.delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    /// `eth_getBlockByNumber`: the chain's block, as a node renders it,
    /// or null past this provider's own height.
    async fn block(&self, params: &Value) -> Value {
        self.blocks.fetch_add(1, Ordering::Relaxed);
        self.delay().await;
        let number = params[0]
            .as_str()
            .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
            .expect("a block number in hex");
        if number > self.height.load(Ordering::Relaxed) {
            return Value::Null;
        }
        let Some(mut block) = self
            .chain
            .block(BlockAt::Number(number))
            .await
            .expect("chain up")
        else {
            return Value::Null;
        };
        if self.lie_block.load(Ordering::Relaxed) {
            block.hash = keccak256(block.hash);
        }
        render_block(&block)
    }

    /// `arkiv_query` by `$key`: the chain's entity if it is alive at
    /// this provider's height, answered at that height.
    async fn query(&self, params: &Value) -> Value {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.delay().await;
        let text = params[0].as_str().expect("a query text");
        let key = text
            .split("$key = key(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .and_then(|hex| B256::from_str(hex).ok())
            .expect("a query by $key");
        let height = self.height.load(Ordering::Relaxed);
        let data: Vec<Value> = self
            .chain
            .entity(key)
            .filter(|entity| entity.expires_at > height)
            .map(|entity| {
                let mut entity = entity.as_arkiv_entity();
                if self.lie_entity.load(Ordering::Relaxed) {
                    let mut altered = entity.payload.to_vec();
                    altered.push(0);
                    entity.payload = altered.into();
                }
                render_entity(&entity)
            })
            .into_iter()
            .collect();
        json!({ "data": data, "blockNumber": format!("{height:#x}") })
    }
}

/// A block in the node's JSON, with the fields the reader decodes and
/// one it does not, the way a real answer has more than is read.
fn render_block(block: &BlockFields) -> Value {
    json!({
        "number": format!("{:#x}", block.number),
        "hash": block.hash,
        "parentHash": block.parent_hash,
        "stateRoot": block.state_root,
        "transactionsRoot": block.transactions_root,
        "receiptsRoot": block.receipts_root,
        "transactions": block.transactions,
        "size": "0x220",
    })
}

/// An entity as one row of a query answer.
fn render_entity(entity: &ArkivEntity) -> Value {
    let attributes: Vec<Value> = entity
        .attributes
        .iter()
        .map(|attribute| {
            json!({ "name": attribute.name, "type": attribute.type_tag, "value": attribute.value })
        })
        .collect();
    json!({
        "key": entity.key,
        "creator": entity.creator,
        "createdAt": format!("{:#x}", entity.created_at),
        "expiresAt": format!("{:#x}", entity.expires_at),
        "payload": entity.payload,
        "attributes": attributes,
    })
}
