//! A provider that speaks real JSON-RPC from settable state, for the
//! suites that run the LB against fake nodes.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use axum::{Router, body::Bytes, http::HeaderMap, response::IntoResponse};
use serde_json::{Value, json};

/// A provider that answers `eth_blockNumber` and `eth_chainId` from
/// settable state, with a switch to play dead (503 to everything).
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
}

pub async fn rpc_provider(chain_id: u64) -> (SocketAddr, Arc<Rpc>) {
    let rpc = Arc::new(Rpc {
        height: AtomicU64::new(1),
        chain_id: AtomicU64::new(chain_id),
        down: AtomicBool::new(false),
        served: AtomicU64::new(0),
        requests: AtomicU64::new(0),
        authorization: std::sync::Mutex::new(None),
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
            let result = match request.get("method").and_then(Value::as_str) {
                Some("eth_blockNumber") => {
                    json!(format!("{:#x}", state.height.load(Ordering::Relaxed)))
                }
                Some("eth_chainId") => {
                    json!(format!("{:#x}", state.chain_id.load(Ordering::Relaxed)))
                }
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
