//! The public listener: the served JSON-RPC endpoint, with failover
//! over the provider pool.

use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use tower_http::cors::CorsLayer;

use crate::{
    config::Proxy,
    denylist,
    forwarder::{Forwarder, Outcome},
    jsonrpc,
    pool::{HealthSignal, Pool},
};

pub struct ProxyState {
    pub pool: Arc<Pool>,
    pub forwarder: Forwarder,
    pub config: Proxy,
    /// Health is flipped after this amount of consecutive fails / successes
    pub flip_after: u32,
}

pub fn router(state: Arc<ProxyState>) -> Router {
    let max_request = state.config.max_request_size.as_u64() as usize;
    // Only the root path serves. Any other path — say, an admin route
    // sent to the wrong port — gets a 404 instead of a silently
    // load-balanced answer. Any other method goes to the 405 answer.
    Router::new()
        .route("/", post(handle).fallback(method_not_allowed))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(max_request))
        // Needed to make requests from inside the browser work.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        "Nothing here: the JSON-RPC endpoint is the root path.\n",
    )
        .into_response()
}

/// A non-POST was never a JSON-RPC request: a browser opening the
/// endpoint URL, a health checker, a crawler. The nodes answer these the
/// same way. CORS preflight does not reach here, the layer above answers
/// it, but it is a method the endpoint does accept.
async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, OPTIONS")],
        "This is a JSON-RPC endpoint: send requests with HTTP POST.\n",
    )
        .into_response()
}

async fn handle(
    State(state): State<Arc<ProxyState>>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    match check_request(body) {
        Ok(body) => forward_with_failover(&state, body).await,
        Err(refusal) => refusal,
    }
}

/// The refusals answered before any provider is involved. Shared with
/// the admin pinned forward, so both ports answer them the same way.
// The Err is as large as a Response, but this path moves Responses
// by value everywhere already; boxing would trade a stack copy for
// a heap allocation per refused request.
#[allow(clippy::result_large_err)]
pub(crate) fn check_request(body: Result<Bytes, BytesRejection>) -> Result<Bytes, Response> {
    let body = match body {
        Ok(body) => body,
        // Over the size limit gets the LB's own envelope; any other
        // buffering failure (the client vanished mid-upload, a transfer
        // error) keeps the rejection's answer — there is usually nobody
        // left to read it.
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                jsonrpc::REQUEST_TOO_LARGE,
                "request too large",
                &[],
            ));
        }
        Err(rejection) => return Err(rejection.into_response()),
    };
    // A healthy node answers empty body with 400,
    // which would lead to failover below and return NO_HEALTHY_PROVIDER.
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Empty request body: expected a JSON-RPC request.\n",
        )
            .into_response());
    }
    // Denied before selection: a refused request reaches no provider and
    // ticks nobody's health.
    if let Some(name) = denylist::denied(&body) {
        tracing::info!(method_prefix = %name, outcome = %"denied", "request");
        return Err(error(
            StatusCode::OK,
            jsonrpc::METHOD_DENIED,
            &format!("method not supported: {name}"),
            &body,
        ));
    }
    Ok(body)
}

/// Forwards with failover: attempts across providers within the retry budget, all
/// under one request deadline — each attempt gets
/// `min(attempt_timeout, remaining)`.
async fn forward_with_failover(state: &ProxyState, body: Bytes) -> Response {
    let started = Instant::now();
    let deadline = started + state.config.request_timeout;
    let max_attempts = 1 + state.config.max_retries;
    let mut attempts = 0;

    for _ in 0..max_attempts {
        let Some(provider) = state.pool.next_eligible() else {
            log_outcome(started, attempts, None, "no_healthy_provider");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                jsonrpc::NO_HEALTHY_PROVIDER,
                "no healthy provider",
                &body,
            );
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout = remaining.min(state.config.attempt_timeout);
        attempts += 1;

        match state.forwarder.attempt(provider, &body, timeout).await {
            Outcome::Answer(response) => {
                // No health credit for an answer: successes count only
                // from probes, so served traffic cannot outvote the
                // Monitor's verdicts or readmit a quarantined provider.
                provider.record_served();
                log_outcome(started, attempts, Some(&provider.id), "answered");
                return response;
            }
            Outcome::TooLarge => {
                // No health tick: a cap breach can follow from the query
                // as easily as from the provider, and the same query
                // would tick every provider alike.
                log_outcome(started, attempts, Some(&provider.id), "response_too_large");
                return error(
                    StatusCode::BAD_GATEWAY,
                    jsonrpc::RESPONSE_TOO_LARGE,
                    "response too large",
                    &body,
                );
            }
            Outcome::NoAnswer => {
                provider.record_health(false, state.flip_after, HealthSignal::Traffic);
                provider.record_transport_failure();
                continue;
            }
        }
    }

    // NO_HEALTHY_PROVIDER answers two cases: no eligible provider to try
    // (the return inside the loop), and the retry budget spent on
    // failed attempts. To the client they are the same fact — no
    // provider produced an answer. Running out of time instead gets its
    // own code.
    if Instant::now() >= deadline {
        log_outcome(started, attempts, None, "timed_out");
        error(
            StatusCode::GATEWAY_TIMEOUT,
            jsonrpc::REQUEST_TIMED_OUT,
            "request timed out",
            &body,
        )
    } else {
        log_outcome(started, attempts, None, "no_provider_answered");
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            jsonrpc::NO_HEALTHY_PROVIDER,
            "no provider answered",
            &body,
        )
    }
}

/// How a request ended, for whoever is reading the log.
fn log_outcome(started: Instant, attempts: u32, provider: Option<&str>, outcome: &str) {
    tracing::info!(
        provider = %provider.unwrap_or("-"),
        attempts,
        latency_ms = started.elapsed().as_millis(),
        outcome = %outcome,
        "request"
    );
}

/// An LB error as a response: envelope, `lb: ` prefix, id echoed when the
/// request body yields one.
pub(crate) fn error(status: StatusCode, code: i32, message: &str, request_body: &[u8]) -> Response {
    let id = jsonrpc::extract_id(request_body);
    (
        status,
        Json::<Value>(jsonrpc::error_response(code, message, id)),
    )
        .into_response()
}
