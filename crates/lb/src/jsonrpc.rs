//! JSON-RPC error envelopes the LB generates itself. Standard codes
//! (−32700, −32601, …) are always a provider's answer, passed through —
//! never fabricated here. LB codes live in −3205x, and every message
//! carries the `lb: ` prefix so a client can tell whose error it reads;
//! the prefix is applied here, in one place.

use serde_json::{Value, json};

pub const METHOD_DENIED: i32 = -32050;
pub const NO_HEALTHY_PROVIDER: i32 = -32051;
pub const REQUEST_TIMED_OUT: i32 = -32052;
pub const RESPONSE_TOO_LARGE: i32 = -32053;
pub const REQUEST_TOO_LARGE: i32 = -32054;

pub fn error_response(code: i32, message: &str, id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": format!("lb: {message}") },
    })
}

/// The request `id` for an error envelope. Parsing happens on error paths
/// only — the served path forwards the body untouched.
pub fn extract_id(body: &[u8]) -> Value {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|request| request.get("id").cloned())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_id_never_fails() {
        assert_eq!(extract_id(br#"{"id": 7}"#), json!(7));
        assert_eq!(extract_id(br#"{"id": "abc"}"#), json!("abc"));
        assert_eq!(extract_id(b"not json at all"), Value::Null);
        assert_eq!(
            extract_id(br#"[{"id": 1}, {"id": 2}]"#),
            Value::Null,
            "a batch has no single id"
        );
        assert_eq!(extract_id(br#"{"jsonrpc": "2.0"}"#), Value::Null);
    }
}
