//! The load generator: concurrent workers sending read requests of one
//! kind at one target URL, counting outcomes. Target-agnostic on
//! purpose — the scenarios point it at the rig's LB, an operator can
//! point the same command at a deployed one.

use std::{
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
/// envelopes — is `failed`, with the first reason kept for diagnosis.
/// Latencies are the round-trip times of the `ok` requests.
#[derive(Default)]
pub struct Stats {
    pub sent: u64,
    pub ok: u64,
    pub failed: u64,
    pub first_failure: Option<String>,
    pub latencies: Vec<Duration>,
}

impl Stats {
    fn record(&mut self, outcome: Result<Duration, String>) {
        self.sent += 1;
        match outcome {
            Ok(latency) => {
                self.ok += 1;
                self.latencies.push(latency);
            }
            Err(reason) => {
                self.failed += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some(reason);
                }
            }
        }
    }

    fn merge(mut self, other: Stats) -> Stats {
        self.sent += other.sent;
        self.ok += other.ok;
        self.failed += other.failed;
        if self.first_failure.is_none() {
            self.first_failure = other.first_failure;
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
) -> Result<Duration, String> {
    // Named in the failure reasons.
    let method = body["method"].as_str().unwrap_or("?");
    let started = Instant::now();
    let response = client
        .post(target)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("{method}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body = body.get(..200).unwrap_or(&body);
        return Err(format!("{method}: HTTP {status}: {body}"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("{method}: bad body: {error}"))?;
    if body.get("result").is_none() {
        return Err(format!("{method}: {body}"));
    }
    Ok(started.elapsed())
}
