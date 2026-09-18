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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config,
    forwarder::{Forwarder, Outcome},
    jsonrpc,
    marketplace::admission::{Agreements, Op, Token, decide_admission},
    pool::{Pool, Provider, Source, marketplace_id},
    proxy,
};

#[derive(Clone)]
struct AdminState {
    pool: Arc<Pool>,
    ready: Arc<AtomicBool>,
    forwarder: Forwarder,
    attempt_timeout: Duration,
    /// The live agreements, when the marketplace is configured.
    agreements: Option<Arc<dyn Agreements>>,
}

pub fn router(
    pool: Arc<Pool>,
    ready: Arc<AtomicBool>,
    forwarder: Forwarder,
    proxy: &config::Proxy,
    agreements: Option<Arc<dyn Agreements>>,
) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/nodes", get(nodes))
        .route("/node/{id}", post(forward_to_node));
    // The admission route exists only with the marketplace: without it
    // there are no agreements to admit against, and the tunnel server's
    // config does not name the route.
    if agreements.is_some() {
        router = router.route("/admission", post(admission));
    }
    router
        .layer(DefaultBodyLimit::max(
            proxy.max_request_size.as_u64() as usize
        ))
        .with_state(AdminState {
            pool,
            ready,
            forwarder,
            attempt_timeout: proxy.attempt_timeout,
            agreements,
        })
}

/// What the tunnel server posts before it lets a client in: the op, and
/// a content whose shape depends on it (`docs/TUNNELING.md`).
#[derive(Deserialize)]
struct FrpsRequest {
    op: String,
    #[serde(default)]
    content: Value,
}

/// The client's metadata, as its config sets it.
#[derive(Deserialize, Default)]
struct Metas {
    agreement: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize)]
struct LoginContent {
    #[serde(default)]
    metas: Metas,
    #[serde(default)]
    client_address: String,
}

/// A proxy registration: the client's metas ride under `user`, the
/// proxy's own fields beside it.
#[derive(Deserialize)]
struct NewProxyContent {
    #[serde(default)]
    user: UserInfo,
    #[serde(default)]
    proxy_name: String,
    #[serde(default)]
    remote_port: u16,
}

#[derive(Deserialize, Default)]
struct UserInfo {
    #[serde(default)]
    metas: Metas,
}

/// The tunnel server's callback. Every answer is a 200: `unchange` lets
/// the client in as it is, `reject` turns it away with a reason its
/// operator reads in the client's own log. A body that is not the
/// callback's shape is a 400, which the tunnel server treats as a
/// plugin error and refuses the client for.
async fn admission(
    State(state): State<AdminState>,
    body: Result<Json<FrpsRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let agreements = state
        .agreements
        .as_ref()
        .expect("the route is mounted with the agreements");
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            tracing::warn!(%rejection, "admission: the body does not parse as a callback");
            return (StatusCode::BAD_REQUEST, rejection.body_text()).into_response();
        }
    };
    // The content is typed per op. `who` is what the log names the
    // client by: its address at login, its proxy's name at registration.
    // An op this route was not configured for passes through.
    let parsed = match request.op.as_str() {
        "Login" => serde_json::from_value::<LoginContent>(request.content)
            .map(|login| (Op::Login, login.metas, login.client_address)),
        "NewProxy" => serde_json::from_value::<NewProxyContent>(request.content).map(|proxy| {
            (
                Op::NewProxy {
                    remote_port: proxy.remote_port,
                },
                proxy.user.metas,
                proxy.proxy_name,
            )
        }),
        _ => return Json(json!({ "unchange": true })).into_response(),
    };
    let (op, metas, who) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(op = request.op, %error, "admission: the content does not parse");
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    };
    let op_name = request.op.as_str();
    let decision =
        Token::parse(metas.agreement.as_deref(), metas.token.as_deref()).and_then(|token| {
            let stored = agreements.agreement(token.agreement);
            decide_admission(op, &token, stored.as_ref())
                .map(|()| stored.expect("admitted, so the agreement was found"))
        });
    match decision {
        Ok(stored) => {
            tracing::info!(
                op = op_name,
                provider = %stored.record.provider,
                agreement = %stored.key,
                port = stored.record.remote_port,
                client = who,
                "tunnel admitted"
            );
            // The proxy is what carries traffic, so the probe is asked
            // for once it is registered, not at login.
            if let Op::NewProxy { .. } = op
                && let Some(provider) = state
                    .pool
                    .snapshot()
                    .iter()
                    .find(|provider| provider.id == marketplace_id(stored.record.provider))
            {
                provider.schedule_probe_now();
            }
            Json(json!({ "unchange": true })).into_response()
        }
        Err(rejection) => {
            tracing::warn!(
                op = op_name,
                reason = %rejection,
                client = who,
                "tunnel rejected"
            );
            Json(json!({ "reject": true, "reject_reason": rejection.to_string() })).into_response()
        }
    }
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
    let Some(provider) = state.pool.snapshot().iter().find(|p| p.id == id).cloned() else {
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
        .attempt(&provider, &body, state.attempt_timeout)
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
    source: &'static str,
    url: String,
    agreement_id: Option<String>,
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
        let (source, agreement_id) = match &provider.source {
            Source::Static => ("static", None),
            Source::Marketplace { agreement_id, .. } => {
                ("marketplace", Some(format!("{agreement_id:#x}")))
            }
        };
        Self {
            id: provider.id.clone(),
            source,
            url: provider.url.to_string(),
            agreement_id,
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
    Json(
        state
            .pool
            .snapshot()
            .iter()
            .map(|provider| NodeView::from(provider.as_ref()))
            .collect(),
    )
}
