//! One forwarding attempt: one client request to one provider. The
//! request body crosses untouched; headers do not — fresh ones toward
//! the provider, fresh ones back to the client (upstream status and
//! body, nothing else). Failover across providers is the caller's job.
//!
//! An answer is what a node's RPC layer produced: a 2xx with a JSON
//! body, even one carrying a JSON-RPC error, or a 400 carrying a
//! JSON-RPC response, the node refusing the request itself. Any other
//! status means the node did not answer (overloaded, misconfigured, or
//! the tunnel speaking in its place) and counts as a failed attempt.

use axum::{
    body::Bytes,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::{chain::transport_causes, config::Proxy, pool::Provider};

/// Cloning shares the connection pool: the client inside is an `Arc`.
#[derive(Clone)]
pub struct Forwarder {
    client: reqwest::Client,
    max_response_size: usize,
}

/// What one attempt produced.
pub enum Outcome {
    /// The provider answered — success or its own JSON-RPC error alike.
    Answer(Response),
    /// No answer: transport failure, a 2xx whose body is not JSON, a
    /// 400 whose body is not a JSON-RPC response, or any other status.
    NoAnswer,
    /// No answer within the attempt's timeout, on the send or the body
    /// read. Apart from the other failures because a slow provider is
    /// not a dead one.
    TimedOut,
    /// The answer exceeded the response cap.
    TooLarge,
}

impl Forwarder {
    pub fn new(client: reqwest::Client, config: &Proxy) -> Self {
        Self {
            client,
            max_response_size: config.max_response_size.as_u64() as usize,
        }
    }

    pub async fn attempt(
        &self,
        provider: &Provider,
        body: &Bytes,
        timeout: std::time::Duration,
    ) -> Outcome {
        let sent = self
            .client
            .post(provider.url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .timeout(timeout)
            .send()
            .await;
        let mut response = match sent {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(
                    provider = %provider.id,
                    error = %transport_causes(&error),
                    "attempt failed"
                );
                return failure(&error);
            }
        };

        let status = response.status();
        // A 400 can be the node refusing the request, which every node
        // refuses alike, so it is read and judged by its body below. The
        // other 4xx can be this provider's own setup, a 413 from a lower
        // request cap or a 404 from a misrouted proxy, so another
        // provider may answer: they fail over like a 5xx.
        let refused = status == StatusCode::BAD_REQUEST;
        let answered = status.is_success() || refused;
        if !answered {
            tracing::debug!(
                provider = %provider.id,
                %status,
                request = %head(body),
                "non-2xx from provider"
            );
            return Outcome::NoAnswer;
        }
        let cap = self.max_response_size;
        if response
            .content_length()
            .is_some_and(|length| length > cap as u64)
        {
            return Outcome::TooLarge;
        }

        // The declared length is provider-controlled, but it passed the
        // cap check above, so the reservation is bounded by the cap.
        let mut collected = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if collected.len() + chunk.len() > cap {
                        return Outcome::TooLarge;
                    }
                    collected.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(
                        provider = %provider.id,
                        error = %transport_causes(&error),
                        "body read failed"
                    );
                    return failure(&error);
                }
            }
        }

        // A JSON-RPC answer is an object or, for a batch, an array. A
        // 2xx with nothing or something else in it, a 204 or an HTML
        // page from something in front of the node, is no more the
        // node's answer than a non-2xx is, and fails over the same way.
        // The first byte, not a parse: the body can be large, and what
        // a well-formed body says is the integrity check's business.
        if !looks_like_json(&collected) {
            tracing::debug!(
                provider = %provider.id,
                bytes = collected.len(),
                body = %head(&collected),
                request = %head(body),
                "2xx without a JSON body"
            );
            return Outcome::NoAnswer;
        }
        // A refusal is the node's answer only when its body is a
        // JSON-RPC response; anything else is something in front of the
        // node refusing in its place, and fails over like a 5xx. A
        // parse, unlike the 2xx check: a refusal is a short error.
        if refused {
            if !is_json_rpc_response(&collected) {
                tracing::debug!(
                    provider = %provider.id,
                    %status,
                    bytes = collected.len(),
                    body = %head(&collected),
                    request = %head(body),
                    "400 without a JSON-RPC body"
                );
                return Outcome::NoAnswer;
            }
            tracing::debug!(provider = %provider.id, %status, "the node refused the request");
        }

        Outcome::Answer(
            (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                collected,
            )
                .into_response(),
        )
    }
}

/// A failed attempt's outcome: the timeout told apart from the rest.
fn failure(error: &reqwest::Error) -> Outcome {
    if error.is_timeout() {
        Outcome::TimedOut
    } else {
        Outcome::NoAnswer
    }
}

/// The first bytes of a body as text, for a log line that has to say
/// what was sent or answered without carrying a whole body.
fn head(body: &[u8]) -> String {
    const SHOWN: usize = 200;
    let shown = &body[..body.len().min(SHOWN)];
    let mut text = String::from_utf8_lossy(shown).into_owned();
    if body.len() > SHOWN {
        text.push('…');
    }
    text
}

/// The longest body the check below parses. A node's error body is a
/// few dozen bytes; a longer one comes from something else, an error
/// page for example, and is not parsed at all.
const REFUSAL_MAX: usize = 4096;

/// Whether a body is a JSON-RPC response: an object with a `jsonrpc`
/// field, or a batch of them.
fn is_json_rpc_response(body: &[u8]) -> bool {
    if body.len() > REFUSAL_MAX {
        return false;
    }
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(fields)) => fields.contains_key("jsonrpc"),
        Ok(serde_json::Value::Array(batch)) => {
            !batch.is_empty() && batch.iter().all(|item| item.get("jsonrpc").is_some())
        }
        _ => false,
    }
}

/// Whether a body starts, after whitespace, as a JSON object or array.
/// The scan stops at the first non-blank byte, usually the first byte,
/// and a body of nothing but whitespace is bounded by the request or
/// response cap, which was already paid to read it in.
pub(crate) fn looks_like_json(body: &[u8]) -> bool {
    body.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{' || *byte == b'[')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_a_json_rpc_object_or_a_batch_of_them_within_the_ceiling() {
        let error =
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;
        assert!(is_json_rpc_response(error));
        assert!(is_json_rpc_response(
            br#"[{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid request"}}]"#
        ));

        assert!(!is_json_rpc_response(b"[]"), "an empty batch says nothing");
        assert!(
            !is_json_rpc_response(br#"[{"jsonrpc":"2.0","id":1,"result":"ok"},{"error":"x"}]"#),
            "every item carries the envelope"
        );
        assert!(
            !is_json_rpc_response(br#"{"error":"forbidden"}"#),
            "plain JSON"
        );
        assert!(!is_json_rpc_response(b"<html>bad request</html>"));
        assert!(!is_json_rpc_response(b""));

        // Over the ceiling the body is not parsed, whatever it holds.
        let padding = " ".repeat(REFUSAL_MAX);
        let long = format!("{padding}{}", std::str::from_utf8(error).unwrap());
        assert!(!is_json_rpc_response(long.as_bytes()));
    }
}
