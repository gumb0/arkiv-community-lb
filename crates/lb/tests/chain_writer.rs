//! The writer client against a fake sidecar that answers with the bodies
//! docs/CHAIN_WRITER.md shows: every route's request lands in the shape
//! the sidecar decodes, and every status maps to its outcome.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256};
use axum::{
    Router,
    body::Bytes,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use lb::chain::{
    records::{Agreement, Record, Wei},
    writer::{Batch, Create, Delete, Expiry, Extend, Patch, WriteError, Writer},
};
use serde_json::{Value, json};

const KEY: &str = "0x8863000000000000000000000000000000000000000000000000000000009057";
const TX: &str = "0x925d000000000000000000000000000000000000000000000000000000000033c7";
/// The fake sidecar's signing address, checksummed as viem reports it.
const SIDECAR_ADDRESS: &str = "0xCA4B166EE155Cb2816Dc25f94Dc1fD102a26c997";

struct Sidecar {
    /// Every request: route and decoded body.
    seen: Mutex<Vec<(String, Value)>>,
    status: StatusCode,
    response: Value,
}

async fn sidecar(status: StatusCode, response: Value) -> (Writer, Arc<Sidecar>) {
    let state = Arc::new(Sidecar {
        seen: Mutex::new(Vec::new()),
        status,
        response,
    });
    let app = Router::new()
        .route(
            "/identity",
            get(|| async {
                axum::Json(json!({ "address": SIDECAR_ADDRESS, "chainId": 7_733_102 }))
            }),
        )
        .route(
            "/{route}",
            post({
                let state = state.clone();
                move |axum::extract::Path(route): axum::extract::Path<String>, body: Bytes| {
                    let state = state.clone();
                    async move {
                        let body: Value = serde_json::from_slice(&body).expect("json body");
                        state.seen.lock().expect("seen").push((route, body));
                        (state.status, axum::Json(state.response.clone())).into_response()
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let writer = Writer::new(
        reqwest::Client::new(),
        format!("http://{addr}/").parse().expect("url"),
    );
    (writer, state)
}

fn agreement() -> Create {
    Create::new(
        Agreement {
            provider: Address::ZERO,
            wei_per_call: Wei::new(1_000_000_000_000_000),
            remote_port: 20007,
        }
        .encode(),
        Expiry::Seconds(7200),
    )
}

fn key() -> B256 {
    KEY.parse().expect("key")
}

#[tokio::test]
async fn the_identity_is_the_signing_address_and_the_chain() {
    let (writer, sidecar) = sidecar(StatusCode::OK, json!({})).await;
    let identity = writer.identity().await.expect("identity");
    assert_eq!(
        identity.address,
        SIDECAR_ADDRESS.parse::<Address>().expect("address")
    );
    assert_eq!(identity.chain_id, 7_733_102);
    assert!(
        sidecar.seen.lock().expect("seen").is_empty(),
        "a read, not a write"
    );
}

#[tokio::test]
async fn a_create_lands_and_returns_the_key_and_expiry() {
    let (writer, sidecar) = sidecar(
        StatusCode::OK,
        json!({ "entityKey": KEY, "txHash": TX, "expiresAt": "248397" }),
    )
    .await;
    let created = writer.create(&agreement()).await.expect("created");
    assert_eq!(created.entity_key, key());
    assert_eq!(created.tx_hash, TX);
    assert_eq!(created.expires_at, 248_397);

    let seen = sidecar.seen.lock().expect("seen");
    let (route, body) = &seen[0];
    assert_eq!(route, "create");
    assert_eq!(body["contentType"], "application/json");
    assert_eq!(body["expires"], json!({ "seconds": 7200 }));
    assert_eq!(
        body["attributes"]["provider"],
        json!({ "type": "addr", "value": format!("{:#x}", Address::ZERO) })
    );
    assert!(body["payload"].is_string());
}

#[tokio::test]
async fn patch_delete_and_extend_hit_their_routes() {
    let (writer, sidecar) = sidecar(
        StatusCode::OK,
        json!({ "entityKey": KEY, "txHash": TX, "expiresAt": "5678" }),
    )
    .await;
    writer
        .patch(&Patch {
            entity_key: key(),
            set: None,
            payload: Some(b"{}".to_vec()),
        })
        .await
        .expect("patched");
    writer
        .delete(&Delete { entity_key: key() })
        .await
        .expect("deleted");
    let extended = writer
        .extend(&Extend {
            entity_key: key(),
            expires: Expiry::Seconds(259_200),
        })
        .await
        .expect("extended");
    assert_eq!(extended.expires_at, 5678);

    let seen = sidecar.seen.lock().expect("seen");
    let routes: Vec<&str> = seen.iter().map(|(route, _)| route.as_str()).collect();
    assert_eq!(routes, ["patch", "delete", "extend"]);
    assert_eq!(seen[0].1["entityKey"], KEY);
    assert_eq!(seen[0].1["payload"], "e30=");
    assert_eq!(seen[2].1["expires"], json!({ "seconds": 259200 }));
}

#[tokio::test]
async fn a_batch_returns_the_extended_keys() {
    let (writer, sidecar) = sidecar(
        StatusCode::OK,
        json!({
            "txHash": TX,
            "createdEntities": [],
            "patchedEntities": [],
            "deletedEntities": [],
            "extendedEntities": [KEY, KEY],
            "ownershipChanges": [],
        }),
    )
    .await;
    let batch = Batch {
        extensions: vec![
            Extend {
                entity_key: key(),
                expires: Expiry::Seconds(1),
            };
            2
        ],
    };
    let result = writer.execute_batch(&batch).await.expect("batch");
    assert_eq!(result.tx_hash, TX);
    assert_eq!(result.extended_entities, [key(), key()]);

    let seen = sidecar.seen.lock().expect("seen");
    assert_eq!(seen[0].0, "execute-batch");
    assert_eq!(seen[0].1["extensions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_400_is_refused_with_the_sidecar_error() {
    let (writer, _) = sidecar(
        StatusCode::BAD_REQUEST,
        json!({ "error": [{ "name": "Error", "message": "create.expires must be" }] }),
    )
    .await;
    let error = writer.create(&agreement()).await.expect_err("refused");
    assert!(
        matches!(&error, WriteError::Refused(links) if links[0].message.starts_with("create.expires"))
    );
}

#[tokio::test]
async fn a_500_is_a_failed_write_with_the_cause_chain() {
    let (writer, _) = sidecar(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": [
            { "name": "EntityMutationError", "message": "reverted" },
            { "name": "ContractFunctionExecutionError", "message": "TooManyAttributes" },
        ] }),
    )
    .await;
    let error = writer.create(&agreement()).await.expect_err("failed");
    let WriteError::Failed(links) = &error else {
        panic!("not a failure: {error}");
    };
    assert_eq!(links.len(), 2);
    assert!(error.to_string().contains("TooManyAttributes"));
}

#[tokio::test]
async fn a_504_is_unresolved_and_carries_the_hash() {
    let (writer, _) = sidecar(
        StatusCode::GATEWAY_TIMEOUT,
        json!({
            "error": [{ "name": "WaitForTransactionReceiptTimeoutError", "message": "timed out" }],
            "pending": { "txHash": TX },
        }),
    )
    .await;
    let error = writer.create(&agreement()).await.expect_err("unresolved");
    match error {
        WriteError::Unresolved { tx_hash, errors } => {
            assert_eq!(tx_hash, TX);
            assert_eq!(errors.len(), 1);
        }
        other => panic!("not unresolved: {other}"),
    }
}

#[tokio::test]
async fn an_unknown_status_or_body_is_reported_as_such() {
    let (writer, _) = sidecar(StatusCode::NOT_FOUND, json!("no POST route /create")).await;
    let error = writer.create(&agreement()).await.expect_err("unexpected");
    assert!(matches!(error, WriteError::Unexpected { status: 404, .. }));

    let (writer, _) = sidecar(StatusCode::OK, json!({ "surprise": true })).await;
    let error = writer.create(&agreement()).await.expect_err("unexpected");
    assert!(matches!(error, WriteError::Unexpected { status: 200, .. }));
}

#[tokio::test]
async fn an_unreachable_sidecar_is_a_transport_error() {
    let writer = Writer::new(
        reqwest::Client::new(),
        "http://127.0.0.1:1/".parse().expect("url"),
    );
    let error = writer.create(&agreement()).await.expect_err("unreachable");
    assert!(matches!(error, WriteError::Transport(_)));
}
