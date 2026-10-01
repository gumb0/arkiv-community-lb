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

/// The lie a relay tells, chosen at start. Either one leaves the
/// probes' reads alone, so the probes see an honest node and only the
/// integrity round can notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lie {
    /// Every entity answered to a query by key comes back with one
    /// byte added to its payload.
    Entity,
    /// Every block answered to `eth_getBlockByNumber` comes back with
    /// another hash.
    Block,
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
        Some(lie) if status.is_success() => altered(&body, bytes, lie),
        None | Some(_) => bytes,
    };
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

/// The answer as relayed with a lie on: altered when it is one the lie
/// changes, as it came otherwise. A batch, a body that is not JSON,
/// every other method, and an answer with nothing to alter pass
/// through untouched.
fn altered(request: &Bytes, answer: Bytes, lie: Lie) -> Bytes {
    let Ok(request) = serde_json::from_slice::<Value>(request) else {
        return answer;
    };
    let Ok(mut parsed) = serde_json::from_slice::<Value>(&answer) else {
        return answer;
    };
    let altered = match lie {
        Lie::Entity => alter_entities(&request, &mut parsed),
        Lie::Block => alter_block(&request, &mut parsed),
    };
    if !altered {
        return answer;
    }
    serde_json::to_vec(&parsed)
        .map(Bytes::from)
        .unwrap_or(answer)
}

/// Adds a byte to the payload of every row in the answer to a query by
/// key; returns whether any row was altered.
fn alter_entities(request: &Value, answer: &mut Value) -> bool {
    let by_key = request["method"] == "arkiv_query"
        && request["params"][0]
            .as_str()
            .is_some_and(|text| text.trim_start().starts_with("$key = key("));
    if !by_key {
        return false;
    }
    let Some(rows) = answer["result"]["data"].as_array_mut() else {
        return false;
    };
    for row in rows.iter_mut() {
        let payload = row["payload"].as_str().unwrap_or("0x");
        row["payload"] = Value::String(format!("{payload}00"));
    }
    !rows.is_empty()
}

/// Changes the hash of the block in the answer to `eth_getBlockByNumber`;
/// returns whether it did. A null answer, a block the node does not
/// have, stays null.
fn alter_block(request: &Value, answer: &mut Value) -> bool {
    if request["method"] != "eth_getBlockByNumber" {
        return false;
    }
    let Some(hash) = answer["result"]["hash"].as_str() else {
        return false;
    };
    // The last hex digit flipped: another hash, still well formed.
    let (head, last) = hash.split_at(hash.len() - 1);
    let other = if last == "0" { "1" } else { "0" };
    answer["result"]["hash"] = Value::String(format!("{head}{other}"));
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
        assert!(alter_entities(&request, &mut answer));
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
            assert!(!alter_entities(&request, &mut answer), "{request}");
            assert_eq!(answer, self::tests::answer());
        }
    }

    #[test]
    fn a_block_gets_another_hash_and_nothing_else_changes() {
        let request = json!({"method": "eth_getBlockByNumber", "params": ["0x10", false]});
        let mut answer = json!({"jsonrpc": "2.0", "id": 1, "result": {
            "number": "0x10", "hash": "0xab10", "stateRoot": "0xcd",
        }});
        assert!(alter_block(&request, &mut answer));
        assert_eq!(answer["result"]["hash"], "0xab11");
        assert_eq!(answer["result"]["stateRoot"], "0xcd");
        let mut none = json!({"jsonrpc": "2.0", "id": 1, "result": null});
        assert!(!alter_block(&request, &mut none), "no block stays no block");
        let mut other = answer.clone();
        assert!(!alter_block(
            &json!({"method": "eth_blockNumber"}),
            &mut other
        ));
        assert_eq!(other, answer);
    }
}
