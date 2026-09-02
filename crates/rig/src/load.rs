//! The load generator: concurrent workers sending a fixed mix of read
//! requests at one target URL, counting outcomes. Target-agnostic on
//! purpose — the scenarios point it at the rig's LB, an operator can
//! point the same command at a deployed one.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Cheap reads every node answers, cycled through in order.
const METHODS: [&str; 4] = [
    "eth_blockNumber",
    "eth_chainId",
    "eth_gasPrice",
    "net_version",
];

/// A request is `ok` only when it comes back HTTP 200 with a JSON-RPC
/// `result`; everything else — transport errors, other statuses, error
/// envelopes — is `failed`, with the first reason kept for diagnosis.
#[derive(Default)]
pub struct Stats {
    pub sent: u64,
    pub ok: u64,
    pub failed: u64,
    pub first_failure: Option<String>,
}

impl Stats {
    fn record(&mut self, outcome: Result<(), String>) {
        self.sent += 1;
        match outcome {
            Ok(()) => self.ok += 1,
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
        self
    }
}

/// Runs `concurrency` workers against `target` until `duration` is up;
/// every worker loops as fast as its answers come back.
pub async fn run(target: &str, concurrency: usize, duration: Duration) -> Stats {
    let client = reqwest::Client::builder()
        // A hung target must not stall a worker past the run.
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let deadline = Instant::now() + duration;

    let workers = (0..concurrency).map(|_| {
        let client = client.clone();
        let target = target.to_string();
        tokio::spawn(async move {
            let mut stats = Stats::default();
            let mut turn = 0usize;
            while Instant::now() < deadline {
                let method = METHODS[turn % METHODS.len()];
                turn += 1;
                stats.record(one_request(&client, &target, method).await);
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

async fn one_request(client: &reqwest::Client, target: &str, method: &str) -> Result<(), String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": []});
    let response = client
        .post(target)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("{method}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{method}: HTTP {status}"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("{method}: bad body: {error}"))?;
    if body.get("result").is_none() {
        return Err(format!("{method}: {body}"));
    }
    Ok(())
}
