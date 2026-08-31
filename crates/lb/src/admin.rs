//! The admin listener: the operator's and the rig's surface.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use serde_json::json;

use crate::pool::{Pool, Provider};

#[derive(Clone)]
struct AdminState {
    pool: Arc<Pool>,
    ready: Arc<AtomicBool>,
}

pub fn router(pool: Arc<Pool>, ready: Arc<AtomicBool>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/nodes", get(nodes))
        .with_state(AdminState { pool, ready })
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
