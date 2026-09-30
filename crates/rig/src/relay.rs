//! A JSON-RPC relay in front of a node: honest, or lying about entities.
//! In the rig it stands in front of a dev node as one provider; on a
//! provider's box it stands between the tunnel client and the node, so
//! a live provider can be made to lie on demand. A test counterparty,
//! never a shipped component.

use axum::{
    Router,
    body::Bytes,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use tokio::net::TcpListener;

/// The lie a relay tells, chosen at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lie {
    /// Every entity answered to a query by key comes back with one
    /// byte added to its payload. Nothing else changes, so the probes
    /// see an honest node and only the integrity round can notice.
    Entity,
}

/// Relays every request on `listener` to `upstream` until the process
/// ends.
pub async fn serve(listener: TcpListener, upstream: reqwest::Url, lie: Option<Lie>) {
    let client = reqwest::Client::new();
    let app = Router::new().fallback(move |body: Bytes| {
        let (client, upstream) = (client.clone(), upstream.clone());
        async move { relay(&client, upstream, body, lie).await }
    });
    axum::serve(listener, app).await.expect("relay serves");
}

async fn relay(
    client: &reqwest::Client,
    upstream: reqwest::Url,
    body: Bytes,
    lie: Option<Lie>,
) -> Response {
    // Only the body goes on: the node needs no header of the caller's.
    let answer = match client
        .post(upstream)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
    {
        Ok(answer) => answer,
        // The LB reads a non-2xx status as the node not answering,
        // which is what happened.
        Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    };
    let status = answer.status();
    let Ok(bytes) = answer.bytes().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let bytes = match lie {
        Some(Lie::Entity) if status.is_success() => lie_about_entities(&body, bytes),
        _ => bytes,
    };
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

/// The answer as relayed with the entity lie on: altered when the
/// request is a query by key, as it came otherwise. A batch, a body
/// that is not JSON, and every other method pass through untouched.
fn lie_about_entities(request: &Bytes, answer: Bytes) -> Bytes {
    let Ok(request) = serde_json::from_slice::<Value>(request) else {
        return answer;
    };
    let Ok(mut parsed) = serde_json::from_slice::<Value>(&answer) else {
        return answer;
    };
    if !alter(&request, &mut parsed) {
        return answer;
    }
    serde_json::to_vec(&parsed)
        .map(Bytes::from)
        .unwrap_or(answer)
}

/// Adds a byte to the payload of every row in the answer to a query by
/// key; returns whether the request was one.
fn alter(request: &Value, answer: &mut Value) -> bool {
    let by_key = request["method"] == "arkiv_query"
        && request["params"][0]
            .as_str()
            .is_some_and(|text| text.trim_start().starts_with("$key = key("));
    if !by_key {
        return false;
    }
    if let Some(rows) = answer["result"]["data"].as_array_mut() {
        for row in rows {
            let payload = row["payload"].as_str().unwrap_or("0x");
            row["payload"] = Value::String(format!("{payload}00"));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "data": [{"key": "0x01", "payload": "0xabcd"}],
            "blockNumber": "0x10",
        }})
    }

    #[test]
    fn a_query_by_key_gets_a_different_payload() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "arkiv_query",
            "params": ["$key = key(0x01)", {}]});
        let mut answer = answer();
        assert!(alter(&request, &mut answer));
        assert_eq!(answer["result"]["data"][0]["payload"], "0xabcd00");
        assert_eq!(answer["result"]["blockNumber"], "0x10", "only the payload");
    }

    #[test]
    fn other_queries_and_methods_pass_untouched() {
        for request in [
            json!({"method": "arkiv_query", "params": ["kind = \"rpc.offer\"", {}]}),
            json!({"method": "eth_blockNumber", "params": []}),
            // A batch holding a query by key: only single requests are
            // altered, since the integrity round never batches.
            json!([{"method": "arkiv_query", "params": ["$key = key(0x01)", {}]}]),
        ] {
            let mut answer = answer();
            assert!(!alter(&request, &mut answer), "{request}");
            assert_eq!(answer, self::tests::answer());
        }
    }
}
