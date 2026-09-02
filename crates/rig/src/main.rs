//! The rig: drives the shipped LB binary against real node containers.
//!
//! Every scenario starts a fleet of dev-node containers through the
//! same `scripts/dev-node.sh` operators use, renders a config for
//! them, boots `arkiv-lb`, waits until it reports ready, and tears
//! everything down again. `rig boot` is that and nothing more — the
//! smallest scenario.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

mod fleet;
mod load;

use fleet::Fleet;

/// Dev-node host ports start here: away from the script's own 8645
/// default, so a leftover manual dev node never collides with the rig.
const BASE_PORT: u16 = 18650;
const NODES: usize = 3;

// Not 18545/18546: those are the documented tunnel-port examples, so
// on a dev machine they may be real forwarded ports.
const LB_PUBLIC: &str = "127.0.0.1:18700";
const LB_ADMIN: &str = "127.0.0.1:18701";

/// Boot cover: image pulls are done by then (the script waits out its
/// own 60 s), and admission needs flip_after probe rounds on top.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("boot") => boot().await,
        Some("denylist") => denylist().await,
        Some("distribution") => distribution().await,
        Some("forward-to-node") => forward_to_node().await,
        Some("kill-recover") => kill_recover().await,
        Some("load") => load_command(std::env::args().skip(2)).await,
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: rig boot\n       rig denylist\n       rig distribution\n       rig forward-to-node\n       rig kill-recover\n       rig load --target <url> [--concurrency N] [--duration SECONDS]"
    );
    std::process::exit(2);
}

async fn load_command(mut args: impl Iterator<Item = String>) {
    let mut target = None;
    let mut concurrency = 4;
    let mut duration = Duration::from_secs(10);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--target" => target = Some(value),
            "--concurrency" => concurrency = value.parse().unwrap_or_else(|_| usage()),
            "--duration" => {
                duration = Duration::from_secs(value.parse().unwrap_or_else(|_| usage()))
            }
            _ => usage(),
        }
    }
    let Some(target) = target else { usage() };

    println!("rig: load on {target}: {concurrency} workers for {duration:?}");
    let started = Instant::now();
    let stats = load::run(&target, concurrency, duration).await;
    report(&stats, started.elapsed());
    if stats.failed > 0 {
        std::process::exit(1);
    }
}

fn report(stats: &load::Stats, elapsed: Duration) {
    println!(
        "rig: {} requests in {:.1}s ({:.0}/s), {} ok, {} failed",
        stats.sent,
        elapsed.as_secs_f64(),
        stats.sent as f64 / elapsed.as_secs_f64(),
        stats.ok,
        stats.failed
    );
    if let Some(reason) = &stats.first_failure {
        println!("rig: first failure: {reason}");
    }
}

/// The workspace root, from the rig crate's own location — correct
/// regardless of the directory the rig is invoked from.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

/// Fleet up, LB booted over it and ready — where every scenario
/// starts. Dropping the pair tears it down, LB first.
async fn start_stack() -> (Fleet, Lb) {
    let root = workspace_root();
    let fleet = Fleet::start(&root, NODES, BASE_PORT);
    let chain_id = fleet.chain_id();
    println!("rig: fleet of {NODES} up, chain id {chain_id}");

    let config = render_config(&root, &fleet, chain_id);
    let mut lb = Lb::spawn(&root, &config);
    wait_ready(&mut lb).await;
    (fleet, lb)
}

async fn boot() {
    let stack = start_stack().await;
    drop(stack);
    println!("rig: ok");
}

/// The acceptance scenario: kill a provider mid-load — the clients
/// notice nothing, the pool notices fast; restart it — it returns.
async fn kill_recover() {
    let stack = start_stack().await;
    let victim = &stack.0.nodes()[0];

    println!("rig: load on http://{LB_PUBLIC}: 4 workers for 15s");
    let load_started = Instant::now();
    let load = tokio::spawn(async {
        load::run(&format!("http://{LB_PUBLIC}"), 4, Duration::from_secs(15)).await
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    victim.stop();
    println!("rig: killed {} under load", victim.id);

    let killed = Instant::now();
    wait_nodes(
        "the kill shows in /nodes",
        Duration::from_secs(30),
        |nodes| provider(nodes, &victim.id)["eligible"] == false,
    )
    .await;
    println!(
        "rig: quarantined {:.1}s after the kill",
        killed.elapsed().as_secs_f32()
    );

    let stats = load.await.expect("load task");
    report(&stats, load_started.elapsed());
    assert_eq!(stats.failed, 0, "clients must not see the kill");
    assert!(stats.sent > 0, "the load must actually have run");

    victim.start();
    println!("rig: restarted {}", victim.id);
    let restarted = Instant::now();
    wait_nodes(
        "readmission shows in /nodes",
        Duration::from_secs(120),
        |nodes| provider(nodes, &victim.id)["eligible"] == true,
    )
    .await;
    println!(
        "rig: readmitted {:.1}s after the restart",
        restarted.elapsed().as_secs_f32()
    );

    drop(stack);
    println!("rig: ok");
}

/// Load over a healthy fleet lands on every provider, and the /nodes
/// served counters account for every answered request.
async fn distribution() {
    let stack = start_stack().await;

    println!("rig: load on http://{LB_PUBLIC}: 4 workers for 5s");
    let stats = load::run(&format!("http://{LB_PUBLIC}"), 4, Duration::from_secs(5)).await;
    report(&stats, Duration::from_secs(5));
    assert_eq!(stats.failed, 0, "a healthy fleet must serve everything");

    let served = served_counts(&stack.0).await;
    for (node, count) in stack.0.nodes().iter().zip(&served) {
        println!("rig: {} served {count}", node.id);
    }

    assert_eq!(
        served.iter().sum::<u64>(),
        stats.ok,
        "every answered request is billed to exactly one provider"
    );
    let (min, max) = (
        *served.iter().min().expect("nodes"),
        *served.iter().max().expect("nodes"),
    );
    assert!(min > 0, "round robin must reach every provider");
    // Not a statistical test — only a guard against a broken cursor
    // pinning the traffic to one provider.
    assert!(max <= 2 * min, "share spread too wide: {served:?}");

    drop(stack);
    println!("rig: ok");
}

/// A refused method is answered by the LB itself: the error envelope
/// comes back, no provider is involved, nothing is billed — and the
/// endpoint keeps serving allowed methods.
async fn denylist() {
    let stack = start_stack().await;
    let client = reqwest::Client::new();
    let url = format!("http://{LB_PUBLIC}");

    let denied = serde_json::json!(
        {"jsonrpc": "2.0", "id": 7, "method": "admin_peers", "params": []}
    );
    let response = client
        .post(&url)
        .json(&denied)
        .send()
        .await
        .expect("lb answers");
    assert_eq!(response.status(), 200, "a denial is an answered request");
    let body: serde_json::Value = response.json().await.expect("json");
    // -32050: the method-denied code from the client contract
    // (docs/ENDPOINT.md).
    assert_eq!(body["error"]["code"], -32050, "{body}");
    assert_eq!(body["id"], 7, "the request id is echoed");
    println!("rig: admin_peers refused: {}", body["error"]["message"]);

    let allowed = serde_json::json!(
        {"jsonrpc": "2.0", "id": 8, "method": "eth_blockNumber", "params": []}
    );
    let response = client
        .post(&url)
        .json(&allowed)
        .send()
        .await
        .expect("lb answers");
    let body: serde_json::Value = response.json().await.expect("json");
    assert!(
        body.get("result").is_some(),
        "allowed methods still serve: {body}"
    );

    assert_eq!(
        served_counts(&stack.0).await.iter().sum::<u64>(),
        1,
        "the refusal reached no provider; the allowed request reached one"
    );

    drop(stack);
    println!("rig: ok");
}

/// The operator's side door: a quarantined provider is out of rotation
/// but still reachable one-off through the admin listener — dead it
/// answers 502, alive it answers as itself, and neither touches
/// billing.
async fn forward_to_node() {
    let stack = start_stack().await;
    let victim = &stack.0.nodes()[0];
    let client = reqwest::Client::new();
    let node_url = format!("http://{LB_ADMIN}/node/{}", victim.id);
    let ask = serde_json::json!(
        {"jsonrpc": "2.0", "id": 9, "method": "eth_blockNumber", "params": []}
    );

    victim.stop();
    wait_nodes(
        "the kill shows in /nodes",
        Duration::from_secs(30),
        |nodes| provider(nodes, &victim.id)["eligible"] == false,
    )
    .await;
    println!("rig: {} killed and quarantined", victim.id);

    let response = client
        .post(&node_url)
        .json(&ask)
        .send()
        .await
        .expect("admin answers");
    assert_eq!(response.status(), 502, "a dead node has no answer to relay");
    println!("rig: admin forward to the dead node: 502");

    // Started again, the node answers admin forwards long before the
    // probes readmit it — that is the whole point of the side door.
    victim.start();
    let response = client
        .post(&node_url)
        .json(&ask)
        .send()
        .await
        .expect("admin answers");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("json");
    assert!(body.get("result").is_some(), "{body}");
    assert_eq!(body["id"], 9, "the node's own answer, id included");
    // Readmission needs flip_after probe successes.
    assert_eq!(
        provider(&fetch_nodes().await, &victim.id)["eligible"],
        false,
        "the answer came from a provider still out of rotation"
    );
    println!(
        "rig: admin forward reached the quarantined node: {}",
        body["result"]
    );

    assert_eq!(
        served_counts(&stack.0).await.iter().sum::<u64>(),
        0,
        "admin forwards are not billed"
    );

    drop(stack);
    println!("rig: ok");
}

/// Per-provider served counts from `/nodes`, in fleet order.
async fn served_counts(fleet: &Fleet) -> Vec<u64> {
    let nodes = fetch_nodes().await;
    fleet
        .nodes()
        .iter()
        .map(|node| {
            provider(&nodes, &node.id)["served"]
                .as_u64()
                .expect("served is a number")
        })
        .collect()
}

/// One `/nodes` answer.
async fn fetch_nodes() -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("http://{LB_ADMIN}/nodes"))
        .send()
        .await
        .expect("/nodes answers")
        .json()
        .await
        .expect("/nodes is json")
}

/// The row for one provider id in a `/nodes` answer.
fn provider<'a>(nodes: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    nodes
        .as_array()
        .expect("/nodes is an array")
        .iter()
        .find(|node| node["id"] == id)
        .expect("known provider id")
}

/// Polls `/nodes` until the condition holds; panics after `timeout` —
/// or right away when `/nodes` stops answering, which after a
/// successful boot means the LB is gone.
async fn wait_nodes(what: &str, timeout: Duration, condition: impl Fn(&serde_json::Value) -> bool) {
    let started = Instant::now();
    loop {
        if condition(&fetch_nodes().await) {
            return;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Writes the LB config for this fleet under `target/rig/`. Everything
/// not listed stays the shipped default; no reference endpoint — the
/// fleet is N independent chains, so lag verdicts would be meaningless.
fn render_config(root: &Path, fleet: &Fleet, chain_id: u64) -> PathBuf {
    let dir = root.join("target/rig");
    std::fs::create_dir_all(&dir).expect("create target/rig");
    let mut config = format!(
        "# Rendered by the rig — do not edit.\n\
         [listen]\npublic = \"{LB_PUBLIC}\"\nadmin = \"{LB_ADMIN}\"\n\n\
         [health]\nchain_id = {chain_id}\n"
    );
    for node in fleet.nodes() {
        config.push_str(&format!(
            "\n[[providers]]\nid = \"{}\"\nurl = \"{}\"\n",
            node.id, node.url
        ));
    }
    let path = dir.join("config.toml");
    std::fs::write(&path, config).expect("write rig config");
    path
}

/// The spawned LB process; killed on drop so no failure path leaves it
/// behind.
struct Lb {
    child: Child,
    log: PathBuf,
}

impl Lb {
    /// Runs the binary the workspace build produced — the same one that
    /// ships — with its output to a log file, so scenario output stays
    /// readable.
    fn spawn(root: &Path, config: &Path) -> Self {
        // The LB binary next to the rig's own: whatever profile built
        // the rig built the LB it drives, so a release rig tests the
        // release binary — the artifact that actually ships.
        let binary = std::env::current_exe()
            .expect("own path")
            .with_file_name("arkiv-lb");
        assert!(
            binary.exists(),
            "{} not found — run `cargo build --workspace` (same profile as the rig) first",
            binary.display()
        );
        let log = root.join("target/rig/lb.log");
        let out = std::fs::File::create(&log).expect("create lb.log");
        let err = out.try_clone().expect("clone log handle");
        let child = Command::new(&binary)
            .arg(config)
            // The machine's own Arkiv endpoint must not leak into the
            // run: the rig decides the reference, and it decides none.
            .env_remove("ARKIV_RPC_URL")
            .env_remove("ARKIV_API_KEY")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn arkiv-lb");
        println!("rig: arkiv-lb started (logs: {})", log.display());
        Self { child, log }
    }
}

impl Drop for Lb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls the admin `/health` until `ready` — the boot window is closed
/// and every healthy provider is admitted.
async fn wait_ready(lb: &mut Lb) {
    let started = Instant::now();
    let client = reqwest::Client::new();
    let url = format!("http://{LB_ADMIN}/health");
    loop {
        // A refused start (a taken port, a bad config) fails here and
        // now, not at the timeout.
        if let Ok(Some(status)) = lb.child.try_wait() {
            panic!(
                "arkiv-lb exited during boot ({status}) — see {}",
                lb.log.display()
            );
        }
        if let Ok(response) = client.get(&url).send().await
            && let Ok(body) = response.json::<serde_json::Value>().await
            && body["ready"] == true
        {
            break;
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "arkiv-lb not ready after {READY_TIMEOUT:?} — see {}",
            lb.log.display()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("rig: ready in {:.1}s", started.elapsed().as_secs_f32());
}
