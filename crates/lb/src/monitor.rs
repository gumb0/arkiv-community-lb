//! The Monitor: the probe loop. One task, one sweep per probe interval.
//! Probe outcomes feed the same health streak that traffic outcomes do,
//! and an ineligible provider receives no traffic — so probes and
//! traffic can both take a provider out of rotation, but only probes
//! bring one in.

use std::sync::{Arc, atomic::Ordering};

use futures::StreamExt;
use reqwest::header;
use serde_json::Value;
use tokio::sync::watch;

use crate::{
    config::Health,
    pool::{HealthSignal, Pool, Provider},
};

/// Probes at most this many providers at once.
const CONCURRENT_PROBES: usize = 16;

pub struct Monitor {
    pool: Arc<Pool>,
    client: reqwest::Client,
    config: Health,
}

impl Monitor {
    pub fn new(pool: Arc<Pool>, client: reqwest::Client, config: Health) -> Self {
        Self {
            pool,
            client,
            config,
        }
    }

    /// Probes until shutdown. The first round runs immediately, so
    /// boot-to-serving is about `flip_after` x `probe_interval`.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(self.config.probe_interval);
        // A round that takes longer than the interval would miss ticks.
        // `Delay` behavior waits a full interval after the late round,
        // instead of bursting missed rounds right away.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.probe_all().await,
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn probe_all(&self) {
        // Probing one by one would make a round as slow as the sum of
        // all timeouts; probing everyone at once would spike load with a
        // large pool. So: up to CONCURRENT_PROBES probes at once, each
        // next one starting as a slot frees up, returning when the last
        // finishes. Interleaved waiting on the Monitor's own task, not
        // threads.
        futures::stream::iter(self.pool.providers())
            .for_each_concurrent(CONCURRENT_PROBES, |provider| self.probe(provider))
            .await;
    }

    async fn probe(&self, provider: &Provider) {
        let success = match self.block_number(provider).await {
            Some(height) => {
                provider.height.store(height, Ordering::Relaxed);
                true
            }
            None => false,
        };
        provider.record_health(success, self.config.flip_after, HealthSignal::Probe);
    }

    /// One `eth_blockNumber` round trip; any shortfall — transport,
    /// status, or an answer that is not a hex height — is one failure.
    async fn block_number(&self, provider: &Provider) -> Option<u64> {
        let sent = self
            .client
            .post(provider.url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"jsonrpc":"2.0","id":0,"method":"eth_blockNumber","params":[]}"#)
            .timeout(self.config.probe_timeout)
            .send()
            .await;
        let response = match sent {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(provider = %provider.id, status = %response.status(), "probe failed");
                return None;
            }
            Err(error) => {
                tracing::debug!(provider = %provider.id, %error, "probe failed");
                return None;
            }
        };
        let body: Value = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
        let height = hex_quantity(body.get("result")?);
        if height.is_none() {
            tracing::debug!(provider = %provider.id, "probe answered without a height");
        }
        height
    }
}

/// A JSON-RPC quantity: a JSON string holding "0x"-prefixed hex.
fn hex_quantity(value: &Value) -> Option<u64> {
    let hex = value.as_str()?.strip_prefix("0x")?;
    u64::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hex_quantities_parse_and_junk_does_not() {
        assert_eq!(hex_quantity(&json!("0x0")), Some(0));
        assert_eq!(hex_quantity(&json!("0x2a")), Some(42));
        assert_eq!(
            hex_quantity(&json!("2a")),
            None,
            "the 0x prefix is required"
        );
        assert_eq!(hex_quantity(&json!("0xzz")), None);
        assert_eq!(
            hex_quantity(&json!(42)),
            None,
            "a JSON number is not a quantity"
        );
        assert_eq!(hex_quantity(&json!(null)), None);
    }
}
