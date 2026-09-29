//! The service's wiring, checked from the outside: both listeners
//! bind and route independently, a zero-config boot answers
//! truthfully, shutdown completes, and a configured integrity round
//! reaches the pool.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use alloy_primitives::Address;
use lb::{
    chain::{
        records::{Agreement, Record, Wei},
        writer::Expiry,
    },
    config::{Config, Integrity, Provider},
    integrity::Verdict,
    jsonrpc::NO_HEALTHY_PROVIDER,
    pool::HealthSignal,
};

mod common;
use common::{
    fake_chain::{FINALITY_LAG, FakeChain},
    fake_provider::{FakeProvider, rpc_provider_on},
};

const CHAIN_ID: u64 = 1337;

#[tokio::test]
async fn boots_serves_and_shuts_down() {
    let mut config = Config::default();
    // No Monitor: even over an empty pool a probe round would complete
    // and flip `ready`, making its assertion below racy.
    config.health.disable_probing = true;
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    let service = lb::service::start(config).await.expect("service boots");
    let public = format!("http://{}", service.public_addr);
    let admin = format!("http://{}", service.admin_addr);
    // Gives up instead of hanging the suite when a listener never answers.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    // Admin liveness.
    let health = client
        .get(format!("{admin}/health"))
        .send()
        .await
        .expect("health answers");
    assert_eq!(health.status(), 200);
    let body: serde_json::Value = health.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["ready"], false,
        "probing is disabled, so ready must stay false"
    );

    let nodes: serde_json::Value = client
        .get(format!("{admin}/nodes"))
        .send()
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json");
    assert_eq!(nodes, serde_json::json!([]), "an empty pool is visible");

    // An empty pool answers truthfully on the public listener.
    let response = client
        .post(&public)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}))
        .send()
        .await
        .expect("public answers");
    assert_eq!(response.status(), 503); // Service Unavailable
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], NO_HEALTHY_PROVIDER);
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.starts_with("lb: "), "{message}");

    // The listeners do not leak into each other.
    let cross = client
        .get(format!("{public}/health"))
        .send()
        .await
        .expect("answers");
    assert_eq!(
        cross.status(),
        404, // Not Found
        "admin routes must not exist on the public listener"
    );
    let unknown = client
        .get(format!("{admin}/nope"))
        .send()
        .await
        .expect("answers");
    assert_eq!(unknown.status(), 404); // Not Found
    let rpc_on_admin = client
        .post(format!("{admin}/"))
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}))
        .send()
        .await
        .expect("answers");
    assert_eq!(
        rpc_on_admin.status(),
        404, // Not Found
        "the admin listener must not serve JSON-RPC"
    );

    tokio::time::timeout(Duration::from_secs(5), service.shutdown())
        .await
        .expect("shutdown completes");
}

#[tokio::test]
async fn shutdown_lets_an_in_flight_request_finish() {
    use axum::{Router, response::IntoResponse};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::Notify;

    // A provider that answers only when the test releases it, so the
    // test controls the ordering: no timer decides anything.
    let arrived = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let app = {
        let (arrived, release) = (arrived.clone(), release.clone());
        Router::new().fallback(move || {
            let (arrived, release) = (arrived.clone(), release.clone());
            async move {
                arrived.store(true, Ordering::Relaxed);
                release.notified().await;
                r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#.into_response()
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let provider_addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let mut config = Config::default();
    config.health.disable_probing = true;
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.providers = vec![Provider {
        id: "held".into(),
        url: format!("http://{provider_addr}"),
    }];
    let service = lb::service::start(config).await.expect("service boots");
    service.pool.snapshot()[0].set_health(true);
    let public = format!("http://{}", service.public_addr);

    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(public)
            .json(
                &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}),
            )
            .send()
            .await
            .expect("the in-flight request completes")
            .status()
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !arrived.load(Ordering::Relaxed) {
        assert!(
            std::time::Instant::now() < deadline,
            "the request never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // With the request held at the provider, a working drain cannot
    // finish; a broken one finishes at once.
    let shutdown = tokio::spawn(service.shutdown());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for the request in flight, not cut it"
    );

    release.notify_one();
    shutdown.await.expect("shutdown task");
    assert_eq!(request.await.expect("request task"), 200);
}

#[tokio::test]
async fn nodes_is_a_current_view_of_the_pool() {
    let mut config = Config::default();
    config.health.disable_probing = true;
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.providers = vec![Provider {
        id: "node-1".into(),
        url: "http://127.0.0.1:18545".into(),
    }];
    let service = lb::service::start(config).await.expect("service boots");
    let admin = format!("http://{}", service.admin_addr);
    let client = reqwest::Client::new();

    let initial: serde_json::Value = client
        .get(format!("{admin}/nodes"))
        .send()
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json");
    assert_eq!(
        initial,
        serde_json::json!([{
            "id": "node-1",
            "source": "static",
            "url": "http://127.0.0.1:18545/",
            "agreement_id": null,
            "eligible": false,
            "ineligibility_reason": "probe",
            "chain_verified": false,
            "health_streak": 0,
            "last_height": null,
            "served": 0,
            "transport_failures": 0,
            "last_probe_ms": null,
            "integrity_verdict": null,
            "integrity_height": null
        }])
    );

    // Change the entry after the first request. The next response must
    // load the atomics again, not serve a cached snapshot.
    let provider = service.pool.snapshot()[0].clone();
    provider.record_health(false, 3, HealthSignal::Traffic);
    let before_flip: serde_json::Value = client
        .get(format!("{admin}/nodes"))
        .send()
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json");
    assert_eq!(before_flip[0]["eligible"], false);
    assert_eq!(before_flip[0]["health_streak"], -1);
    assert_eq!(
        before_flip[0]["ineligibility_reason"], "traffic",
        "the latest signal is visible even when eligibility did not flip"
    );

    provider.record_probe_duration(Duration::from_millis(7));
    provider.record_height(0);
    for _ in 0..3 {
        provider.record_health(true, 3, HealthSignal::Probe);
    }
    provider.record_served();
    provider.record_served();
    provider.record_transport_failure();
    provider.record_integrity(Verdict::Divergence, 1_204_000);

    let current: serde_json::Value = client
        .get(format!("{admin}/nodes"))
        .send()
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json");
    let node = &current[0];
    assert_eq!(node["id"], "node-1");
    assert_eq!(node["source"], "static");
    assert_eq!(node["url"], "http://127.0.0.1:18545/");
    assert_eq!(node["agreement_id"], serde_json::Value::Null);
    assert_eq!(
        node["eligible"], false,
        "healthy, but found serving wrong data"
    );
    assert_eq!(node["ineligibility_reason"], "integrity");
    assert_eq!(node["integrity_verdict"], "divergence");
    assert_eq!(node["integrity_height"], 1_204_000);
    assert_eq!(node["chain_verified"], false);
    assert_eq!(node["health_streak"], 3);
    assert_eq!(node["last_height"], 0, "genesis height is not 'unknown'");
    assert_eq!(node["served"], 2);
    assert_eq!(node["transport_failures"], 1);
    assert_eq!(node["last_probe_ms"], 7);

    service.shutdown().await;
}

#[tokio::test]
async fn a_bad_reference_url_refuses_to_start() {
    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.reference = Some("not a url".into());

    let error = lb::service::start(config)
        .await
        .expect_err("must refuse a reference that cannot be probed");
    assert!(error.to_string().contains("not a url"), "{error}");
}

#[tokio::test]
async fn an_integrity_section_without_a_reference_refuses_to_start() {
    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.integrity = Some(lb::config::Integrity::default());

    let error = lb::service::start(config)
        .await
        .expect_err("the rounds need a reference to compare against");
    assert!(error.to_string().contains("ARKIV_RPC_URL"), "{error}");
}

/// A chain with one entity, a fake reference at its head, an honest
/// provider and one lying about the entity, and a config that probes
/// fast with the reference set; the tests add the integrity section.
struct IntegrityFleet {
    chain: FakeChain,
    reference: Arc<FakeProvider>,
    honest: Arc<FakeProvider>,
    liar: Arc<FakeProvider>,
    config: Config,
}

async fn integrity_fleet() -> IntegrityFleet {
    let chain = FakeChain::new(Address::ZERO, CHAIN_ID);
    chain.advance(200);
    chain.write_as(
        Address::ZERO,
        Agreement {
            provider: Address::ZERO,
            offer: alloy_primitives::B256::ZERO,
            wei_per_call: Wei::new(5),
            remote_port: 20001,
        }
        .encode(),
        Expiry::Seconds(3600),
    );
    let (reference_addr, reference) = rpc_provider_on(&chain).await;
    let (honest_addr, honest) = rpc_provider_on(&chain).await;
    let (liar_addr, liar) = rpc_provider_on(&chain).await;
    for rpc in [&reference, &honest, &liar] {
        rpc.height.store(chain.head(), Ordering::Relaxed);
    }
    liar.lie_entity.store(true, Ordering::Relaxed);

    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.reference = Some(format!("http://{reference_addr}"));
    config.health.probe_interval = Duration::from_millis(20);
    config.health.flip_after = 2;
    config.health.chain_id = Some(CHAIN_ID);
    config.providers = vec![
        Provider {
            id: "honest".into(),
            url: format!("http://{honest_addr}"),
        },
        Provider {
            id: "liar".into(),
            url: format!("http://{liar_addr}"),
        },
    ];
    IntegrityFleet {
        chain,
        reference,
        honest,
        liar,
        config,
    }
}

const INTEGRITY: Integrity = Integrity {
    interval: Duration::from_millis(500),
    confirm_after: Duration::from_millis(50),
};

async fn nodes(admin: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{admin}/nodes"))
        .send()
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json")
}

/// `/nodes` once `accept` is true of it, within ten seconds.
async fn nodes_until(
    admin: &str,
    accept: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let nodes = nodes(admin).await;
        if accept(&nodes) {
            return nodes;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the nodes view never showed what was waited for: {nodes}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The whole path over the wire: fake reference, fake providers, the
/// configured checker in the service, the verdict on `/nodes`.
#[tokio::test]
async fn a_configured_integrity_round_takes_a_liar_out_of_rotation() {
    let mut fleet = integrity_fleet().await;
    fleet.config.integrity = Some(INTEGRITY);
    let service = lb::service::start(fleet.config)
        .await
        .expect("service boots");
    let admin = format!("http://{}", service.admin_addr);

    // The first round runs once the probes have admitted the providers;
    // the liar's mismatch is confirmed inside it.
    let nodes = nodes_until(&admin, |nodes| {
        nodes[1]["integrity_verdict"] == "divergence"
    })
    .await;
    assert_eq!(nodes[0]["id"], "honest");
    assert_eq!(nodes[0]["integrity_verdict"], "match");
    assert_eq!(nodes[0]["eligible"], true);
    assert_eq!(nodes[1]["id"], "liar");
    assert_eq!(nodes[1]["eligible"], false);
    assert_eq!(nodes[1]["ineligibility_reason"], "integrity");
    assert_eq!(
        nodes[1]["integrity_height"],
        fleet.chain.head() - FINALITY_LAG,
        "a verdict is stamped with the round's finalized height"
    );

    service.shutdown().await;
}

#[tokio::test]
async fn without_an_integrity_section_nobody_is_checked() {
    let fleet = integrity_fleet().await;
    assert!(fleet.config.integrity.is_none());
    let service = lb::service::start(fleet.config)
        .await
        .expect("service boots");
    let admin = format!("http://{}", service.admin_addr);

    // Admitted by the probes, the point at which a configured checker
    // would run its first round; a little longer, so a round that did
    // run would have left its reads on the reference.
    nodes_until(&admin, |nodes| {
        nodes[0]["eligible"] == true && nodes[1]["eligible"] == true
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let nodes = nodes(&admin).await;
    assert_eq!(nodes[1]["id"], "liar");
    assert_eq!(nodes[1]["eligible"], true, "a liar nobody checks serves");
    assert_eq!(nodes[1]["integrity_verdict"], serde_json::Value::Null);
    assert_eq!(fleet.reference.blocks.load(Ordering::Relaxed), 0);
    assert!(fleet.reference.asked_keys.lock().expect("keys").is_empty());
    assert_eq!(fleet.liar.blocks.load(Ordering::Relaxed), 0);

    service.shutdown().await;
}

/// The probes keep their short timeout while the round's reads, which
/// are heavier, run under the client's: a fleet that answers integrity
/// reads slower than a probe may wait is still judged.
#[tokio::test]
async fn integrity_reads_run_under_the_client_timeout_not_the_probes() {
    let mut fleet = integrity_fleet().await;
    fleet.config.integrity = Some(INTEGRITY);
    fleet.config.health.probe_timeout = Duration::from_millis(100);
    fleet.config.proxy.attempt_timeout = Duration::from_secs(2);
    for rpc in [&fleet.reference, &fleet.honest, &fleet.liar] {
        rpc.delay_ms.store(300, Ordering::Relaxed);
    }
    let service = lb::service::start(fleet.config)
        .await
        .expect("service boots");
    let admin = format!("http://{}", service.admin_addr);

    // A read cut off by the probe's timeout would be unknown, never a
    // match.
    let nodes = nodes_until(&admin, |nodes| nodes[0]["integrity_verdict"] == "match").await;
    assert_eq!(nodes[0]["id"], "honest");
    assert_eq!(nodes[0]["eligible"], true, "probes stayed quick");

    service.shutdown().await;
}
