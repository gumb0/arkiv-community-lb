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

/// The reference's last known height, and when it was last asked for.
#[derive(Default)]
struct ReferenceHeight {
    height: Option<u64>,
    asked: Option<std::time::Instant>,
}

pub struct Monitor {
    pool: Arc<Pool>,
    client: reqwest::Client,
    config: Health,
    reference: Option<reqwest::Url>,
}

impl Monitor {
    pub fn new(
        pool: Arc<Pool>,
        client: reqwest::Client,
        config: Health,
        reference: Option<reqwest::Url>,
    ) -> Self {
        Self {
            pool,
            client,
            config,
            reference,
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
        let mut reference_height = ReferenceHeight::default();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // Reference height is asked serially before providers,
                    // so a timing-out reference delays the round by up to probe_timeout.
                    // Asking it concurrently with the probes would avoid that,
                    // at the cost of judging this round by the previous sample.
                    self.refresh_reference_height(&mut reference_height).await;
                    self.probe_all(reference_height.height).await;
                }
                _ = shutdown.changed() => return,
            }
        }
    }

    /// Asks the reference for its height, at most once per
    /// `ref_height_interval`. A failed ask clears `reference_height.height`:
    /// no reference height means lag will not be judged.
    async fn refresh_reference_height(&self, reference_height: &mut ReferenceHeight) {
        let Some(url) = &self.reference else { return };
        let due = match reference_height.asked {
            None => true,
            Some(at) => at.elapsed() >= self.config.ref_height_interval,
        };
        if !due {
            return;
        }
        reference_height.asked = Some(std::time::Instant::now());
        reference_height.height = self.block_number("reference", url).await;
    }

    async fn probe_all(&self, reference_height: Option<u64>) {
        // Probing one by one would make a round as slow as the sum of
        // all timeouts; probing everyone at once would spike load with a
        // large pool. So: up to CONCURRENT_PROBES probes at once, each
        // next one starting as a slot frees up, returning when the last
        // finishes. Interleaved waiting on the Monitor's own task, not
        // threads.
        futures::stream::iter(self.pool.providers())
            .for_each_concurrent(CONCURRENT_PROBES, |provider| {
                self.probe(provider, reference_height)
            })
            .await;
    }

    async fn probe(&self, provider: &Provider, reference_height: Option<u64>) {
        let Some(height) = self.block_number(&provider.id, &provider.url).await else {
            provider.record_health(false, self.config.flip_after, HealthSignal::Probe);
            return;
        };
        provider.height.store(height, Ordering::Relaxed);

        // Ahead of the reference, or behind it within the tolerance, is
        // healthy; further behind is one failure. No reference height
        // means no lag verdict either way.
        match reference_height {
            Some(reference)
                if height.saturating_add(self.config.lag_tolerance_blocks) < reference =>
            {
                tracing::debug!(provider = %provider.id, height, reference, "behind the reference");
                provider.record_health(false, self.config.flip_after, HealthSignal::Lag);
            }
            _ => provider.record_health(true, self.config.flip_after, HealthSignal::Probe),
        }
    }

    /// One `eth_blockNumber` round trip; any shortfall — transport,
    /// status, or an answer that is not a hex height — is one failure.
    async fn block_number(&self, id: &str, url: &reqwest::Url) -> Option<u64> {
        let sent = self
            .client
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"jsonrpc":"2.0","id":0,"method":"eth_blockNumber","params":[]}"#)
            .timeout(self.config.probe_timeout)
            .send()
            .await;
        let response = match sent {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(provider = %id, status = %response.status(), "probe failed");
                return None;
            }
            Err(error) => {
                tracing::debug!(provider = %id, %error, "probe failed");
                return None;
            }
        };
        let body: Value = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
        let height = hex_quantity(body.get("result")?);
        if height.is_none() {
            tracing::debug!(provider = %id, "probe answered without a height");
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
