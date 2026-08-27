//! One forwarding attempt: one client request to one provider. The
//! request body crosses untouched; headers do not — fresh ones toward
//! the provider, fresh ones back to the client (upstream status and
//! body, nothing else). Failover across providers is the caller's job.
//!
//! An answer is what a node's RPC layer produced — always an HTTP 2xx,
//! even when it carries a JSON-RPC error. Any other status means the
//! node did not answer (overloaded, misconfigured, or the tunnel
//! speaking in its place) and counts as a failed attempt.

use axum::{
    body::Bytes,
    http::header,
    response::{IntoResponse, Response},
};

use crate::{config::Proxy, pool::Entry};

pub struct Forwarder {
    client: reqwest::Client,
    max_response_size: usize,
}

/// What one attempt produced.
pub enum Outcome {
    /// The provider answered — success or its own JSON-RPC error alike.
    Answer(Response),
    /// No answer: transport failure, or a non-2xx status.
    NoAnswer,
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
        entry: &Entry,
        body: &Bytes,
        timeout: std::time::Duration,
    ) -> Outcome {
        let sent = self
            .client
            .post(entry.url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .timeout(timeout)
            .send()
            .await;
        let mut response = match sent {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(provider = %entry.id, %error, "attempt failed");
                return Outcome::NoAnswer;
            }
        };

        let status = response.status();
        if !status.is_success() {
            tracing::debug!(provider = %entry.id, %status, "non-2xx from provider");
            return Outcome::NoAnswer;
        }
        let cap = self.max_response_size;
        if response
            .content_length()
            .is_some_and(|length| length > cap as u64)
        {
            return Outcome::TooLarge;
        }

        let mut collected = Vec::new();
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
                    tracing::debug!(provider = %entry.id, %error, "body read failed");
                    return Outcome::NoAnswer;
                }
            }
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
