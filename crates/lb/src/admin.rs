//! The admin listener: the operator's and the rig's surface.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{Json, Router, extract::State, routing::get};
use serde_json::json;

pub fn router(ready: Arc<AtomicBool>) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(ready)
}

/// Liveness is answering at all; `ready` says the boot window is
/// closed — every healthy provider has been admitted — so tests and
/// the rig can wait on it instead of sleeping.
async fn health(State(ready): State<Arc<AtomicBool>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "ready": ready.load(Ordering::Relaxed),
    }))
}
