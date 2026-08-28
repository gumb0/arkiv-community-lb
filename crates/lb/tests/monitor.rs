//! The Monitor against fake providers that speak real JSON-RPC: the
//! born-ineligible boot window, admission, quarantine, readmission.
//! Millisecond intervals and condition polling — never paused time with
//! real sockets.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{Router, body::Bytes, response::IntoResponse};
use lb::config::{Config, Provider};
use serde_json::{Value, json};

/// A provider that answers `eth_blockNumber` and `eth_chainId` from
/// settable state, with a switch to play dead (503 to everything).
struct Rpc {
    height: AtomicU64,
    chain_id: u64,
    down: AtomicBool,
    /// Client requests answered. Probes send id 0, the tests' client
    /// sends id 1 — the only way to tell them apart at the fake.
    served: AtomicU64,
}

async fn rpc_provider(chain_id: u64) -> (SocketAddr, Arc<Rpc>) {
    let rpc = Arc::new(Rpc {
        height: AtomicU64::new(1),
        chain_id,
        down: AtomicBool::new(false),
        served: AtomicU64::new(0),
    });
    let state = rpc.clone();
    let app = Router::new().fallback(move |body: Bytes| {
        let state = state.clone();
        async move {
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
                Some("eth_chainId") => json!(format!("{:#x}", state.chain_id)),
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

const CHAIN_ID: u64 = 1337;

/// Boots the LB with a fast-probing Monitor over the given providers.
async fn start_monitored(addrs: &[SocketAddr], tune: impl Fn(&mut Config)) -> lb::service::Service {
    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.health.probe_interval = Duration::from_millis(20);
    config.health.flip_after = 2;
    config.providers = addrs
        .iter()
        .enumerate()
        .map(|(i, addr)| Provider {
            id: format!("p{i}"),
            url: format!("http://{addr}"),
        })
        .collect();
    tune(&mut config);
    lb::service::start(config).await.expect("service boots")
}

/// One `eth_blockNumber` call through the LB's public listener.
async fn block_number(client: &reqwest::Client, public: &str) -> Value {
    client
        .post(public)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []}))
        .send()
        .await
        .expect("lb answers")
        .json()
        .await
        .expect("json")
}

/// Polls until the condition holds; panics after two seconds.
async fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn probes_admit_a_healthy_provider() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[addr], |_| {}).await;
    let provider = &service.pool.providers()[0];

    assert!(
        !provider.eligible(),
        "born ineligible: nothing is served before the first probes pass"
    );
    wait_for("admission", || provider.eligible()).await;
    assert!(
        provider.height.load(Ordering::Relaxed) >= 1,
        "a passing probe records the height it saw"
    );
}

#[tokio::test]
async fn a_killed_provider_is_quarantined_and_readmitted_on_recovery() {
    let (addr, rpc) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[addr], |_| {}).await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;

    rpc.down.store(true, Ordering::Relaxed);
    wait_for("quarantine", || !provider.eligible()).await;

    rpc.down.store(false, Ordering::Relaxed);
    wait_for("readmission", || provider.eligible()).await;
}

#[tokio::test]
async fn the_boot_window_answers_no_healthy_provider_then_serves() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    // Probes slow enough that admission (the second round, at +250ms)
    // cannot win the race against the first request.
    let service = start_monitored(&[addr], |config| {
        config.health.probe_interval = Duration::from_millis(250);
    })
    .await;
    let public = format!("http://{}", service.public_addr);
    let client = reqwest::Client::new();

    // A request inside the boot window gets the truthful error.
    let first = block_number(&client, &public).await;
    assert_eq!(first["error"]["code"], -32051);

    // ...and the window closes by itself.
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;
    let served = block_number(&client, &public).await;
    assert_eq!(served["result"], "0x1", "the provider's answer, relayed");
}

#[tokio::test]
async fn failover_keeps_serving_while_a_provider_dies() {
    let (a, rpc_a) = rpc_provider(CHAIN_ID).await;
    let (b, _rpc_b) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[a, b], |_| {}).await;
    wait_for("both admitted", || {
        service
            .pool
            .providers()
            .iter()
            .all(|provider| provider.eligible())
    })
    .await;

    let public = format!("http://{}", service.public_addr);
    let client = reqwest::Client::new();

    // Both in rotation: every request relays.
    for _ in 0..4 {
        let answer = block_number(&client, &public).await;
        assert_eq!(answer["result"], "0x1");
    }

    // One dies mid-service. Requests keep landing throughout: failover
    // covers the window before quarantine, rotation covers after.
    rpc_a.down.store(true, Ordering::Relaxed);
    for _ in 0..20 {
        let answer = block_number(&client, &public).await;
        assert_eq!(answer["result"], "0x1", "the client never notices");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 100ms of failing probes at a 20ms cadence: long since quarantined.
    assert!(!service.pool.providers()[0].eligible());
}

#[tokio::test]
async fn a_recovered_provider_returns_to_rotation() {
    let (a, rpc_a) = rpc_provider(CHAIN_ID).await;
    let (b, _rpc_b) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[a, b], |_| {}).await;
    wait_for("both admitted", || {
        service
            .pool
            .providers()
            .iter()
            .all(|provider| provider.eligible())
    })
    .await;

    let provider_a = &service.pool.providers()[0];
    rpc_a.down.store(true, Ordering::Relaxed);
    wait_for("quarantine", || !provider_a.eligible()).await;
    rpc_a.down.store(false, Ordering::Relaxed);
    wait_for("readmission", || provider_a.eligible()).await;

    // Back in rotation for real: round robin over two eligible
    // providers must land some of these on the recovered one.
    let public = format!("http://{}", service.public_addr);
    let client = reqwest::Client::new();
    let before = rpc_a.served.load(Ordering::Relaxed);
    for _ in 0..10 {
        let answer = block_number(&client, &public).await;
        assert_eq!(answer["result"], "0x1");
    }
    assert!(
        rpc_a.served.load(Ordering::Relaxed) > before,
        "a readmitted provider serves client traffic again"
    );
}
