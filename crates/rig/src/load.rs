//! The load generator: concurrent workers sending read requests of one
//! kind at one target URL, counting outcomes. Target-agnostic on
//! purpose — the scenarios point it at the rig's LB, an operator can
//! point the same command at a deployed one.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// Cheap reads every node answers, cycled through in order.
const METHODS: [&str; 4] = [
    "eth_blockNumber",
    "eth_chainId",
    "eth_gasPrice",
    "net_version",
];

/// What the workers ask.
#[derive(Clone, Copy, Debug)]
pub enum Requests {
    /// The cheap reads, in turn: what a node answers fastest, so the
    /// load finds the ceiling of the path rather than of the node.
    Cheap,
    /// One `arkiv_query` for a full page of live entities with their
    /// payloads, the heaviest read a client makes: what fills the LB's
    /// memory and the tunnel.
    Query,
}

impl Requests {
    /// The request body for the worker's `turn`th request.
    fn body(self, turn: usize) -> Value {
        match self {
            Self::Cheap => {
                let method = METHODS[turn % METHODS.len()];
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": []})
            }
            Self::Query => json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "arkiv_query",
                "params": [
                    "$expiresAt > u64(0)",
                    // The node's page maximum, 200 rows.
                    {"select": {"key": true, "payload": true, "attributes": true}, "limit": "0xc8"},
                ],
            }),
        }
    }
}

/// A request is `ok` only when it comes back HTTP 200 with a JSON-RPC
/// `result`; everything else — transport errors, other statuses, error
/// envelopes — is `failed`, counted by kind, with the first reason of
/// each kind kept for diagnosis. Latencies are the round-trip times of
/// the `ok` requests.
#[derive(Default)]
pub struct Stats {
    pub sent: u64,
    pub ok: u64,
    pub failed: u64,
    /// By kind: `timeout`, `connect`, `transport`, `HTTP 503`, an LB
    /// error's message (`lb: no healthy provider`), a provider error's
    /// code (`rpc error -32601`), `bad body`. The count, and the first
    /// reason seen.
    pub failures: BTreeMap<String, (u64, String)>,
    pub latencies: Vec<Duration>,
}

/// Why a request failed: the kind it is counted under, and the reason
/// in full.
pub struct Failure {
    kind: String,
    reason: String,
}

impl Failure {
    fn new(kind: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            reason: reason.into(),
        }
    }
}

impl Stats {
    fn record(&mut self, outcome: Result<Duration, Failure>) {
        self.sent += 1;
        match outcome {
            Ok(latency) => {
                self.ok += 1;
                self.latencies.push(latency);
            }
            Err(failure) => {
                self.failed += 1;
                self.failures
                    .entry(failure.kind)
                    .or_insert((0, failure.reason))
                    .0 += 1;
            }
        }
    }

    fn merge(mut self, other: Stats) -> Stats {
        self.sent += other.sent;
        self.ok += other.ok;
        self.failed += other.failed;
        for (kind, (count, reason)) in other.failures {
            self.failures.entry(kind).or_insert((0, reason)).0 += count;
        }
        self.latencies.extend(other.latencies);
        self
    }
}

/// Runs `concurrency` workers against `target` until `duration` is up
/// or `stop` turns true; every worker loops as fast as its answers
/// come back, and finishes its request in flight before stopping.
pub async fn run(
    target: &str,
    requests: Requests,
    concurrency: usize,
    duration: Duration,
    stop: Arc<AtomicBool>,
) -> Stats {
    let client = reqwest::Client::builder()
        // A hung target must not stall a worker past the run.
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let deadline = Instant::now() + duration;

    let workers = (0..concurrency).map(|_| {
        let client = client.clone();
        let target = target.to_string();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut stats = Stats::default();
            let mut turn = 0usize;
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                let body = requests.body(turn);
                turn += 1;
                stats.record(one_request(&client, &target, &body).await);
            }
            stats
        })
    });

    let mut total = Stats::default();
    for worker in workers.collect::<Vec<_>>() {
        total = total.merge(worker.await.expect("worker panicked"));
    }
    total
}

async fn one_request(
    client: &reqwest::Client,
    target: &str,
    body: &Value,
) -> Result<Duration, Failure> {
    // Named in the failure reasons.
    let method = body["method"].as_str().unwrap_or("?");
    let started = Instant::now();
    let response = client
        .post(target)
        .json(body)
        .send()
        .await
        .map_err(|error| {
            let kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "transport"
            };
            Failure::new(kind, format!("{method}: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body = body.get(..200).unwrap_or(&body);
        return Err(Failure::new(
            format!("HTTP {}", status.as_u16()),
            format!("{method}: HTTP {status}: {body}"),
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| Failure::new("bad body", format!("{method}: bad body: {error}")))?;
    if body.get("result").is_none() {
        // The LB's own errors carry a fixed message, which names the
        // kind better than the code; a provider's error is its code.
        let kind = match body["error"]["message"].as_str() {
            Some(message) if message.starts_with("lb: ") => message.to_string(),
            _ => format!("rpc error {}", body["error"]["code"]),
        };
        return Err(Failure::new(kind, format!("{method}: {body}")));
    }
    Ok(started.elapsed())
}
