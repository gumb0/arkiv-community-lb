//! Boots the real service in-process and checks the two listeners from
//! the outside.

use std::time::Duration;

use lb::config::Config;

#[tokio::test]
async fn boots_serves_and_shuts_down() {
    let mut config = Config::default();
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

    // An empty pool answers truthfully on the public listener.
    let response = client
        .post(&public)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}))
        .send()
        .await
        .expect("public answers");
    assert_eq!(response.status(), 503);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], -32051);
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
        405,
        "admin routes must not exist on the public listener"
    );
    let unknown = client
        .get(format!("{admin}/nope"))
        .send()
        .await
        .expect("answers");
    assert_eq!(unknown.status(), 404);

    tokio::time::timeout(Duration::from_secs(5), service.shutdown())
        .await
        .expect("shutdown completes");
}
