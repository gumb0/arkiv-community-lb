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
    chain_id: AtomicU64,
    down: AtomicBool,
    /// Client requests answered. Probes send id 0, the tests' client
    /// sends id 1 — the only way to tell them apart at the fake.
    served: AtomicU64,
    /// Every request that arrived, probes and errors included.
    requests: AtomicU64,
}

async fn rpc_provider(chain_id: u64) -> (SocketAddr, Arc<Rpc>) {
    let rpc = Arc::new(Rpc {
        height: AtomicU64::new(1),
        chain_id: AtomicU64::new(chain_id),
        down: AtomicBool::new(false),
        served: AtomicU64::new(0),
        requests: AtomicU64::new(0),
    });
    let state = rpc.clone();
    let app = Router::new().fallback(move |body: Bytes| {
        let state = state.clone();
        async move {
            state.requests.fetch_add(1, Ordering::Relaxed);
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

const CHAIN_ID: u64 = 1337;

/// Boots the LB with a fast-probing Monitor over the given providers.
async fn start_monitored(
    addrs: &[SocketAddr],
    tune: impl FnOnce(&mut Config),
) -> lb::service::Service {
    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.health.probe_interval = Duration::from_millis(20);
    config.health.flip_after = 2;
    config.health.chain_id = Some(CHAIN_ID);
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

#[tokio::test]
async fn lagging_behind_the_reference_quarantines_and_catchup_readmits() {
    let (addr, rpc) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[addr], move |config| {
        config.reference = Some(reference_url);
        config.health.lag_tolerance_blocks = 3;
    })
    .await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;

    // One block beyond the allowed lag quarantines...
    rpc.height.store(96, Ordering::Relaxed);
    reference.height.store(100, Ordering::Relaxed);
    wait_for("lag quarantine", || !provider.eligible()).await;

    // ...and exactly the allowed lag is healthy.
    rpc.height.store(97, Ordering::Relaxed);
    wait_for("readmission at the tolerance", || provider.eligible()).await;
}

#[tokio::test]
async fn a_dead_reference_faults_nobody() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    reference.down.store(true, Ordering::Relaxed);
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[addr], move |config| {
        config.reference = Some(reference_url);
        // Any lag would be beyond this tolerance...
        config.health.lag_tolerance_blocks = 0;
    })
    .await;

    // ...but with the reference unreachable there are no lag verdicts:
    // the provider is admitted and stays.
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(provider.eligible(), "a reference outage must fault nobody");
}

#[tokio::test]
async fn a_lag_quarantined_provider_readmits_when_the_reference_dies() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[addr], move |config| {
        config.reference = Some(reference_url);
        config.health.lag_tolerance_blocks = 3;
    })
    .await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;
    reference.height.store(100, Ordering::Relaxed);
    wait_for("lag quarantine", || !provider.eligible()).await;

    // The reference dies: the cache clears, lag verdicts stop, and the
    // provider's passing probes readmit it. The accepted loss: without
    // a reference, a lagging provider cannot be told from a healthy one.
    reference.down.store(true, Ordering::Relaxed);
    wait_for("readmission without a reference", || provider.eligible()).await;
}

#[tokio::test]
async fn a_lag_quarantined_provider_stops_serving_while_the_other_carries_on() {
    let (a, rpc_a) = rpc_provider(CHAIN_ID).await;
    let (b, rpc_b) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[a, b], move |config| {
        config.reference = Some(reference_url);
        config.health.lag_tolerance_blocks = 3;
    })
    .await;
    wait_for("both admitted", || {
        service
            .pool
            .providers()
            .iter()
            .all(|provider| provider.eligible())
    })
    .await;

    // The chain advances; one provider stays behind — alive, answering,
    // and serving stale data if asked.
    reference.height.store(100, Ordering::Relaxed);
    rpc_b.height.store(100, Ordering::Relaxed);
    wait_for("lag quarantine", || !service.pool.providers()[0].eligible()).await;

    // Every answer now comes from the provider at the chain head.
    let public = format!("http://{}", service.public_addr);
    let client = reqwest::Client::new();
    let before = rpc_a.served.load(Ordering::Relaxed);
    for _ in 0..10 {
        let answer = block_number(&client, &public).await;
        assert_eq!(answer["result"], "0x64", "no stale answers");
    }
    assert_eq!(
        rpc_a.served.load(Ordering::Relaxed),
        before,
        "a live but stale provider serves nothing"
    );
}

#[tokio::test]
async fn a_pool_larger_than_the_probe_concurrency_cap_is_fully_probed() {
    // More providers than probe_all runs at once (16): the cap must
    // queue the rest of a round, never drop them.
    let mut addrs = Vec::new();
    for _ in 0..20 {
        let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
        addrs.push(addr);
    }
    let service = start_monitored(&addrs, |_| {}).await;

    wait_for("all 20 admitted", || {
        service
            .pool
            .providers()
            .iter()
            .all(|provider| provider.eligible())
    })
    .await;
    for provider in service.pool.providers() {
        assert!(
            provider.height.load(Ordering::Relaxed) >= 1,
            "a probe reached {}",
            provider.id
        );
    }
}

#[tokio::test]
async fn a_wrong_chain_provider_is_never_admitted() {
    let (addr, rpc) = rpc_provider(999).await;
    let service = start_monitored(&[addr], |_| {}).await;
    let provider = &service.pool.providers()[0];

    // Ten-plus rounds of opportunity.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!provider.eligible(), "wrong chain never enters rotation");
    assert_eq!(
        provider.height.load(Ordering::Relaxed),
        0,
        "a wrong-chain provider is not even asked for its height"
    );
    assert_eq!(rpc.served.load(Ordering::Relaxed), 0, "and serves nothing");
}

#[tokio::test]
async fn a_chain_id_change_after_admission_evicts_and_a_fix_readmits() {
    let (addr, rpc) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[addr], |config| {
        config.health.chainid_check_interval = Duration::from_millis(60);
    })
    .await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;

    // The node is switched to another chain: the next chain round
    // evicts it, and probe successes must not bring it back.
    rpc.chain_id.store(999, Ordering::Relaxed);
    wait_for("chain eviction", || !provider.eligible()).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        !provider.eligible(),
        "must stay out while on the wrong chain"
    );

    // Switched back: verified again on the next chain round, then
    // readmitted by ordinary probe successes.
    rpc.chain_id.store(CHAIN_ID, Ordering::Relaxed);
    wait_for("readmission after the fix", || provider.eligible()).await;
}

#[tokio::test]
async fn a_dead_provider_is_probed_ever_more_rarely() {
    let (a, rpc_a) = rpc_provider(CHAIN_ID).await;
    let (b, rpc_b) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[a, b], |config| {
        config.health.max_probe_backoff = Duration::from_millis(80);
    })
    .await;
    wait_for("both admitted", || {
        service
            .pool
            .providers()
            .iter()
            .all(|provider| provider.eligible())
    })
    .await;

    // One dies (still answering 503, so its probes are countable). The
    // probes thin out toward max_probe_backoff while the live provider
    // stays on the 20ms beat.
    rpc_a.down.store(true, Ordering::Relaxed);
    let dead_before = rpc_a.requests.load(Ordering::Relaxed);
    let live_before = rpc_b.requests.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let dead = rpc_a.requests.load(Ordering::Relaxed) - dead_before;
    let live = rpc_b.requests.load(Ordering::Relaxed) - live_before;

    assert!(!service.pool.providers()[0].eligible());
    assert!(
        dead >= 3,
        "backoff caps at max_probe_backoff, it never stops probing: {dead}"
    );
    assert!(
        dead * 2 < live,
        "a dead provider must be probed much more rarely: dead {dead}, live {live}"
    );
}

#[tokio::test]
async fn the_reference_is_asked_on_its_own_cadence() {
    let probe_interval = Duration::from_millis(20);
    let ref_height_interval = Duration::from_millis(100);
    let (addr, rpc) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[addr], move |config| {
        config.reference = Some(reference_url);
        config.health.probe_interval = probe_interval;
        config.health.ref_height_interval = ref_height_interval;
    })
    .await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;

    let reference_before = reference.requests.load(Ordering::Relaxed);
    let provider_before = rpc.requests.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let reference_asked = reference.requests.load(Ordering::Relaxed) - reference_before;
    let provider_probed = rpc.requests.load(Ordering::Relaxed) - provider_before;

    assert!(
        reference_asked >= 2,
        "the reference is still sampled: {reference_asked}"
    );
    // The provider is probed every round; the reference is asked once
    // per `rounds_per_ask` rounds. If the interval were ignored, the two
    // counts would be equal — the factor 2 is slack for timing jitter,
    // keeping the bar halfway between the two behaviors.
    let rounds_per_ask =
        u64::try_from(ref_height_interval.as_millis() / probe_interval.as_millis()).expect("small");
    assert!(
        reference_asked * rounds_per_ask < provider_probed * 2,
        "one reference ask per {rounds_per_ask} probe rounds, give or take: \
         reference {reference_asked}, provider {provider_probed}"
    );
}

#[tokio::test]
async fn a_dead_reference_keeps_being_sampled() {
    // Deliberately no backoff for the reference: it is the operator's
    // own endpoint, and lag checks must resume the moment it returns.
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    let (ref_addr, reference) = rpc_provider(CHAIN_ID).await;
    reference.down.store(true, Ordering::Relaxed);
    let reference_url = format!("http://{ref_addr}");
    let service = start_monitored(&[addr], move |config| {
        config.reference = Some(reference_url);
    })
    .await;
    let provider = &service.pool.providers()[0];
    wait_for("admission", || provider.eligible()).await;

    let before = reference.requests.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked = reference.requests.load(Ordering::Relaxed) - before;
    // Full cadence is ~15 asks in this window (300ms / 20ms rounds);
    // provider-style backoff would leave ~5. The bar sits between.
    assert!(
        asked >= 10,
        "a failing reference stays on its full cadence: {asked}"
    );
}

#[tokio::test]
async fn health_reports_ready_after_the_first_probe_round() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    // Slow probes hold the first round back long enough to observe
    // ready = false.
    let service = start_monitored(&[addr], |config| {
        config.health.probe_interval = Duration::from_millis(250);
    })
    .await;
    let admin = format!("http://{}", service.admin_addr);
    let client = reqwest::Client::new();
    let health = |client: reqwest::Client, admin: String| async move {
        let body: Value = client
            .get(format!("{admin}/health"))
            .send()
            .await
            .expect("health answers")
            .json()
            .await
            .expect("json");
        body
    };

    let body = health(client.clone(), admin.clone()).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["ready"], false, "the first round has not finished");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let body = health(client.clone(), admin.clone()).await;
        if body["ready"] == true {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        service.pool.providers()[0].eligible(),
        "ready means the boot window closed: the healthy provider is already admitted"
    );
}

#[tokio::test]
async fn shutdown_completes_with_a_running_monitor() {
    let (addr, _rpc) = rpc_provider(CHAIN_ID).await;
    let service = start_monitored(&[addr], |_| {}).await;
    wait_for("admission", || service.pool.providers()[0].eligible()).await;

    tokio::time::timeout(Duration::from_secs(5), service.shutdown())
        .await
        .expect("shutdown must not hang on the Monitor");
}

#[tokio::test]
async fn an_empty_pool_still_becomes_ready() {
    // No providers configured: the boot window closes trivially, and
    // ready must not wait for admissions that can never come.
    let service = start_monitored(&[], |_| {}).await;
    let admin = format!("http://{}", service.admin_addr);
    let client = reqwest::Client::new();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let body: Value = client
            .get(format!("{admin}/health"))
            .send()
            .await
            .expect("health answers")
            .json()
            .await
            .expect("json");
        if body["ready"] == true {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for ready over an empty pool"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
