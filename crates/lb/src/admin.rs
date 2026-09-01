//! The admin listener: the operator's and the rig's surface.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State, rejection::BytesRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::json;

use crate::{
    config,
    forwarder::{Forwarder, Outcome},
    jsonrpc,
    pool::{Pool, Provider},
    proxy,
};

#[derive(Clone)]
struct AdminState {
    pool: Arc<Pool>,
    ready: Arc<AtomicBool>,
    forwarder: Forwarder,
    attempt_timeout: Duration,
}

pub fn router(
    pool: Arc<Pool>,
    ready: Arc<AtomicBool>,
    forwarder: Forwarder,
    proxy: &config::Proxy,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/nodes", get(nodes))
        .route("/node/{id}", post(forward_to_node))
        .layer(DefaultBodyLimit::max(
            proxy.max_request_size.as_u64() as usize
        ))
        .with_state(AdminState {
            pool,
            ready,
            forwarder,
            attempt_timeout: proxy.attempt_timeout,
        })
}

/// Liveness is answering at all; `ready` says the boot window is
/// closed — every healthy provider has been admitted — so tests and
/// the rig can wait on it instead of sleeping.
async fn health(State(state): State<AdminState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "ready": state.ready.load(Ordering::Relaxed),
    }))
}

/// One JSON-RPC request to one provider, eligibility ignored — the way
/// an operator reaches a quarantined node. One attempt, no failover,
/// and no state changes: diagnostics touch neither health nor billing.
async fn forward_to_node(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(provider) = state.pool.providers().iter().find(|p| p.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no provider {id:?}") })),
        )
            .into_response();
    };
    // Same refusals, same shapes as the public endpoint — the denylist
    // included: what the operator sees here is what a client would see.
    let body = match proxy::check_request(body) {
        Ok(body) => body,
        Err(refusal) => return refusal,
    };

    match state
        .forwarder
        .attempt(provider, &body, state.attempt_timeout)
        .await
    {
        Outcome::Answer(response) => {
            tracing::info!(provider = %id, outcome = %"answered", "admin forward");
            response
        }
        Outcome::NoAnswer => {
            tracing::info!(provider = %id, outcome = %"no_answer", "admin forward");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "provider did not answer" })),
            )
                .into_response()
        }
        Outcome::TooLarge => {
            tracing::info!(provider = %id, outcome = %"response_too_large", "admin forward");
            proxy::error(
                StatusCode::BAD_GATEWAY,
                jsonrpc::RESPONSE_TOO_LARGE,
                "response too large",
                &body,
            )
        }
    }
}

#[derive(Serialize)]
struct NodeView {
    id: String,
    url: String,
    eligible: bool,
    ineligibility_reason: Option<&'static str>,
    chain_verified: bool,
    health_streak: i64,
    last_height: Option<u64>,
    served: u64,
    transport_failures: u64,
    last_probe_ms: Option<u64>,
}

impl From<&Provider> for NodeView {
    fn from(provider: &Provider) -> Self {
        Self {
            id: provider.id.clone(),
            url: provider.url.to_string(),
            eligible: provider.eligible(),
            ineligibility_reason: provider.ineligibility_reason(),
            chain_verified: provider.chain_verified.load(Ordering::Relaxed),
            health_streak: provider.health_streak.load(Ordering::Relaxed),
            last_height: provider.last_height(),
            served: provider.served.load(Ordering::Relaxed),
            transport_failures: provider.transport_failures.load(Ordering::Relaxed),
            last_probe_ms: provider.last_probe_ms(),
        }
    }
}

/// A fresh, lock-free view over every configured provider entry.
async fn nodes(State(state): State<AdminState>) -> Json<Vec<NodeView>> {
    Json(state.pool.providers().iter().map(NodeView::from).collect())
}
