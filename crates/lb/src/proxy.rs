//! The public listener: the served JSON-RPC endpoint. With no providers
//! in rotation every request gets the no-healthy-provider answer — the
//! truthful state of an empty pool.

use axum::{Json, Router, http::StatusCode, response::IntoResponse};
use tower_http::cors::CorsLayer;

use crate::jsonrpc;

pub fn router() -> Router {
    // Catch-all: JSON-RPC clients POST to /, but nothing else lives on
    // this listener either.
    Router::new()
        .fallback(no_healthy_provider)
        // Needed to make requests from inside the browser work.
        .layer(CorsLayer::permissive())
}

async fn no_healthy_provider() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(jsonrpc::error_response(
            jsonrpc::NO_HEALTHY_PROVIDER,
            "no healthy provider",
        )),
    )
}
