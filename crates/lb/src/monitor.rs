//! The Monitor: the probe loop. One task, one sweep per probe interval.
//! Probe outcomes feed the same health streak that traffic outcomes do,
//! and an ineligible provider receives no traffic — so probes and
//! traffic can both take a provider out of rotation, but only probes
//! bring one in.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
    /// Set once `flip_after` probe rounds have completed — the boot
    /// window is closed and every healthy provider has been admitted.
    /// `/health` shows it.
    ready: Arc<AtomicBool>,
}

impl Monitor {
    pub fn new(
        pool: Arc<Pool>,
        client: reqwest::Client,
        config: Health,
        reference: Option<reqwest::Url>,
        ready: Arc<AtomicBool>,
    ) -> Self {
        Self {
            pool,
            client,
            config,
            reference,
            ready,
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
        let mut last_chain_round: Option<std::time::Instant> = None;
        let mut rounds: u32 = 0;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // Reference height is asked serially before providers,
                    // so a timing-out reference delays the round by up to probe_timeout.
                    // Asking it concurrently with the probes would avoid that,
                    // at the cost of judging this round by the previous sample.
                    self.refresh_reference_height(&mut reference_height).await;
                    let chain_round = if last_chain_round
                        .is_none_or(|at| at.elapsed() >= self.config.chainid_check_interval)
                    {
                        last_chain_round = Some(std::time::Instant::now());
                        true
                    } else {
                        false
                    };
                    self.probe_all(reference_height.height, chain_round).await;
                    // Ready once every healthy provider has had its
                    // flip_after rounds to be admitted: the boot window
                    // is closed.
                    rounds = rounds.saturating_add(1);
                    if rounds == self.config.flip_after {
                        self.ready.store(true, Ordering::Relaxed);
                    }
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
        reference_height.height = self.query_block_number("reference", url).await;
    }

    async fn probe_all(&self, reference_height: Option<u64>, chain_round: bool) {
        let now = std::time::Instant::now();
        // Probing one by one would make a round as slow as the sum of
        // all timeouts; probing everyone at once would spike load with a
        // large pool. So: up to CONCURRENT_PROBES probes at once, each
        // next one starting as a slot frees up, returning when the last
        // finishes. Interleaved waiting on the Monitor's own task, not
        // threads.
        futures::stream::iter(self.pool.providers())
            .for_each_concurrent(CONCURRENT_PROBES, |provider| async move {
                let due = *provider.next_probe() <= now;
                if due && self.chain_cleared(provider, chain_round).await {
                    let answered = self.probe(provider, reference_height).await;
                    if !answered {
                        self.reschedule_unanswered(provider, now);
                    }
                }
            })
            .await;
    }

    /// An unanswered probe pushes the next one out, doubling with the
    /// failure streak's depth up to `max_probe_backoff`. Quarantine is
    /// never slowed: the first `flip_after` failures keep full cadence.
    /// Answered probes change nothing — the streak resets itself, and
    /// `next_probe` already lies in the past.
    fn reschedule_unanswered(&self, provider: &Provider, now: std::time::Instant) {
        // Number of failures is negative `health_streak`, 0 if we're on a healthy streak.
        let failures = provider
            .health_streak
            .load(Ordering::Relaxed)
            .min(0)
            .unsigned_abs();
        let delay = probe_delay(&self.config, failures);
        *provider.next_probe() = now + delay;
    }

    /// Whether this provider may be probed this round: confirmed on the
    /// right chain, re-verifying first on a chain round. With no
    /// `chain_id` configured, everyone is cleared.
    async fn chain_cleared(&self, provider: &Provider, chain_round: bool) -> bool {
        let Some(expected) = self.config.chain_id else {
            return true;
        };
        if chain_round {
            self.verify_chain(provider, expected).await;
        }
        provider.chain_verified.load(Ordering::Relaxed)
    }

    /// One `eth_chainId` round trip, updating `chain_verified`. The
    /// wrong chain quarantines on the spot: misconfiguration is a
    /// certainty, not a failure streak.
    async fn verify_chain(&self, provider: &Provider, expected: u64) {
        match self.query_chain_id(&provider.id, &provider.url).await {
            Some(actual) if actual == expected => {
                provider.chain_verified.store(true, Ordering::Relaxed);
            }
            Some(actual) => {
                tracing::warn!(
                    provider = %provider.id,
                    chain_id = actual,
                    expected,
                    "wrong chain"
                );
                provider.chain_verified.store(false, Ordering::Relaxed);
                provider.quarantine(HealthSignal::Chain);
            }
            None => {}
        }
    }

    /// Returns whether the provider answered — a lagging provider did.
    async fn probe(&self, provider: &Provider, reference_height: Option<u64>) -> bool {
        let Some(height) = self.query_block_number(&provider.id, &provider.url).await else {
            provider.record_health(false, self.config.flip_after, HealthSignal::Probe);
            return false;
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
        true
    }

    async fn query_block_number(&self, id: &str, url: &reqwest::Url) -> Option<u64> {
        self.query(
            id,
            url,
            r#"{"jsonrpc":"2.0","id":0,"method":"eth_blockNumber","params":[]}"#,
        )
        .await
    }

    async fn query_chain_id(&self, id: &str, url: &reqwest::Url) -> Option<u64> {
        self.query(
            id,
            url,
            r#"{"jsonrpc":"2.0","id":0,"method":"eth_chainId","params":[]}"#,
        )
        .await
    }

    /// One probe round trip for a quantity-valued method; any
    /// shortfall — transport, status, or an answer that is not a hex
    /// quantity — is one failure.
    async fn query(&self, id: &str, url: &reqwest::Url, body: &'static str) -> Option<u64> {
        let sent = self
            .client
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
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
        let quantity = hex_quantity(body.get("result")?);
        if quantity.is_none() {
            tracing::debug!(provider = %id, "probe answered without a quantity");
        }
        quantity
    }
}

/// The wait before a failing provider's next probe.
fn probe_delay(config: &Health, failures: u64) -> Duration {
    // First `flip_after` failures don't back off the probes -
    // quarantine is never slowed.
    let excess =
        u32::try_from(failures.saturating_sub(u64::from(config.flip_after))).unwrap_or(u32::MAX);
    // Next probe is after probe_interval * 2^excess, capped at max_probe_backoff.
    config
        .probe_interval
        .saturating_mul(1u32.checked_shl(excess).unwrap_or(u32::MAX))
        .min(config.max_probe_backoff)
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
    fn probe_delay_spares_quarantine_then_doubles_to_the_cap() {
        let config = Health {
            probe_interval: Duration::from_secs(5),
            flip_after: 3,
            max_probe_backoff: Duration::from_secs(60),
            ..Health::default()
        };
        let delay = |failures| probe_delay(&config, failures);

        // Up to and including flip_after: full cadence, quarantine is
        // never slowed.
        assert_eq!(delay(0), Duration::from_secs(5));
        assert_eq!(delay(1), Duration::from_secs(5));
        assert_eq!(delay(3), Duration::from_secs(5));
        // Past it: doubling.
        assert_eq!(delay(4), Duration::from_secs(10));
        assert_eq!(delay(5), Duration::from_secs(20));
        assert_eq!(delay(6), Duration::from_secs(40));
        // The cap, and no overflow however deep the streak goes.
        assert_eq!(delay(7), Duration::from_secs(60));
        assert_eq!(delay(1_000_000), Duration::from_secs(60));
        assert_eq!(delay(u64::MAX), Duration::from_secs(60));
    }

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
