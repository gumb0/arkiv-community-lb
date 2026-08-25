//! JSON-RPC error envelopes the LB generates itself. Standard codes
//! (−32700, −32601, …) are always a provider's answer, passed through —
//! never fabricated here. LB codes live in −3205x, and every message
//! carries the `lb: ` prefix so a client can tell whose error it reads;
//! the prefix is applied here, in one place.

use serde_json::{Value, json};

pub const NO_HEALTHY_PROVIDER: i32 = -32051;

pub fn error_response(code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": code, "message": format!("lb: {message}") },
    })
}
