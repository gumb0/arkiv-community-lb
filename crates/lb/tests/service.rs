//! The service's wiring, checked from the outside: both listeners
//! bind and route independently, a zero-config boot answers
//! truthfully, and shutdown completes.

use std::time::Duration;

use lb::{
    config::{Config, Provider},
    jsonrpc::NO_HEALTHY_PROVIDER,
    pool::HealthSignal,
};

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
    service.pool.providers()[0].set_eligible(true);
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
            "url": "http://127.0.0.1:18545/",
            "eligible": false,
            "ineligibility_reason": "probe",
            "chain_verified": false,
            "health_streak": 0,
            "last_height": null,
            "served": 0,
            "transport_failures": 0,
            "last_probe_ms": null
        }])
    );

    // Change the entry after the first request. The next response must
    // load the atomics again, not serve a cached snapshot.
    let provider = &service.pool.providers()[0];
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
    assert_eq!(node["url"], "http://127.0.0.1:18545/");
    assert_eq!(node["eligible"], true);
    assert_eq!(
        node["ineligibility_reason"],
        serde_json::Value::Null,
        "an eligible provider has no reason to be out"
    );
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
