//! The read client against a fake reference: what each call sends, with
//! the bearer key, and how each answer is read.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use axum::{Router, body::Bytes, http::HeaderMap, response::IntoResponse, routing::post};
use lb::chain::{
    reader::{PAGE_LIMIT, Query, ReadError, Reader},
    records::{Agreement, KIND_AGREEMENT, Record, Stored},
};
use serde_json::{Value, json};

const LB: &str = "0x411e31d7ebbfd636af234954db5f598cd80a878c";
const KEY: &str = "0x8863000000000000000000000000000000000000000000000000000000009057";

struct Reference {
    /// Every request: the bearer header, if any, and the JSON-RPC body.
    seen: Mutex<Vec<(Option<String>, Value)>>,
    /// The JSON-RPC response body to return, whatever the method.
    response: Value,
}

async fn reference(response: Value, key: Option<&str>) -> (Reader, Arc<Reference>) {
    let state = Arc::new(Reference {
        seen: Mutex::new(Vec::new()),
        response,
    });
    let app = Router::new().route(
        "/",
        post({
            let state = state.clone();
            move |headers: HeaderMap, body: Bytes| {
                let state = state.clone();
                async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let body: Value = serde_json::from_slice(&body).expect("json body");
                    state.seen.lock().expect("seen").push((bearer, body));
                    axum::Json(state.response.clone()).into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let reader = Reader::new(
        reqwest::Client::new(),
        format!("http://{addr}/").parse().expect("url"),
        key.map(str::to_owned),
        Duration::from_secs(2),
    );
    (reader, state)
}

fn ok(result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "result": result })
}

fn lb() -> Address {
    LB.parse().expect("address")
}

#[tokio::test]
async fn a_query_selects_every_record_field_at_the_page_limit_with_the_key() {
    let (reader, reference) = reference(ok(json!({ "data": [] })), Some("secret")).await;
    let query = Query::kind(KIND_AGREEMENT).creator(lb());
    let page = reader.query(&query).await.expect("page");
    assert!(page.entities.is_empty());
    assert!(!page.more);

    let seen = reference.seen.lock().expect("seen");
    let (bearer, body) = &seen[0];
    assert_eq!(bearer.as_deref(), Some("Bearer secret"));
    assert_eq!(body["method"], "arkiv_query");
    assert_eq!(body["params"][0], query.text());
    assert_eq!(
        body["params"][1]["select"],
        json!({ "key": true, "creator": true, "expiresAt": true, "payload": true, "attributes": true })
    );
    assert_eq!(body["params"][1]["limit"], format!("{PAGE_LIMIT:#x}"));
}

#[tokio::test]
async fn rows_parse_into_stored_records_and_a_cursor_means_more() {
    let encoded = Agreement {
        provider: Address::ZERO,
        wei_per_call: lb::chain::records::Wei::new(5),
        remote_port: 20001,
    }
    .encode();
    let payload_hex: String = encoded.payload.iter().map(|b| format!("{b:02x}")).collect();
    let attributes: Vec<Value> = encoded
        .attributes
        .to_wire()
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, wire)| json!({ "name": name, "type": wire["type"], "value": wire["value"] }))
        .collect();
    let row = json!({
        "key": KEY,
        "creator": LB,
        "expiresAt": "0x92e21",
        "payload": format!("0x{payload_hex}"),
        "attributes": attributes,
    });
    let (reader, _) = reference(ok(json!({ "data": [row], "cursor": "b64:more" })), None).await;
    let page = reader
        .query(&Query::kind(KIND_AGREEMENT))
        .await
        .expect("page");
    assert!(page.more);
    assert!(page.entities[0].is(KIND_AGREEMENT));
    let stored = Stored::<Agreement>::decode(&page.entities[0]).expect("decodes");
    assert_eq!(stored.creator, lb());
    assert_eq!(stored.expires_at, 0x92e21);
    assert_eq!(stored.record.remote_port, 20001);
}

#[tokio::test]
async fn a_count_sends_the_query_and_reads_a_number() {
    let (reader, reference) = reference(ok(json!(7)), None).await;
    let query = Query::kind(KIND_AGREEMENT).creator(lb());
    assert_eq!(reader.count(&query).await.expect("count"), 7);
    let seen = reference.seen.lock().expect("seen");
    assert_eq!(seen[0].0, None);
    assert_eq!(seen[0].1["method"], "arkiv_getEntityCount");
    assert_eq!(seen[0].1["params"][0]["query"], query.text());
}

#[tokio::test]
async fn the_head_height_is_a_hex_quantity() {
    let (reader, reference) = reference(ok(json!("0x1b4")), None).await;
    assert_eq!(reader.block_number().await.expect("head"), 436);
    let seen = reference.seen.lock().expect("seen");
    assert_eq!(seen[0].1["method"], "eth_blockNumber");
}

#[tokio::test]
async fn a_balance_is_asked_at_the_head_and_read_as_wei() {
    // 0.02 GLM, the gas warning's default threshold.
    let (reader, reference) = reference(ok(json!("0x470de4df820000")), None).await;
    let balance = reader.balance(lb()).await.expect("balance");
    assert_eq!(
        balance,
        alloy_primitives::U256::from(20_000_000_000_000_000u64)
    );
    let seen = reference.seen.lock().expect("seen");
    assert_eq!(seen[0].1["method"], "eth_getBalance");
    assert_eq!(seen[0].1["params"], json!([LB, "latest"]));
}

#[tokio::test]
async fn a_json_rpc_error_is_reported_with_its_code() {
    let (reader, _) = reference(
        json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32002, "message": "limit above the maximum" } }),
        None,
    )
    .await;
    let error = reader
        .count(&Query::kind(KIND_AGREEMENT))
        .await
        .expect_err("error");
    assert!(matches!(error, ReadError::Rpc { code: -32002, .. }));
    assert!(error.to_string().contains("limit above the maximum"));
}

#[tokio::test]
async fn an_unexpected_answer_is_reported_as_such() {
    let (reader, _) = reference(ok(json!("seven")), None).await;
    let error = reader
        .count(&Query::kind(KIND_AGREEMENT))
        .await
        .expect_err("error");
    assert!(matches!(error, ReadError::Unexpected(_)));
}

#[tokio::test]
async fn an_unreachable_reference_is_a_transport_error() {
    let reader = Reader::new(
        reqwest::Client::new(),
        "http://127.0.0.1:1/".parse().expect("url"),
        None,
        Duration::from_secs(2),
    );
    let error = reader.block_number().await.expect_err("unreachable");
    assert!(matches!(error, ReadError::Transport(_)));
}
