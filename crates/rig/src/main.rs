//! The rig: drives the shipped LB binary against real node containers.
//!
//! Every scenario starts a fleet of dev-node containers through
//! `scripts/dev-node.sh`, renders a config for them, boots `arkiv-lb`,
//! waits until it reports ready, and tears everything down again.
//! `rig boot` is that and nothing more — the smallest scenario.
//! `rig relay` is not a scenario: it runs the relay alone, in front of
//! any node.

use std::{
    future::Future,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

mod fleet;
mod load;
mod provider;
mod relay;
mod writer;

use fleet::Fleet;
use writer::Writer;

/// Dev-node host ports start here: away from the script's own 8645
/// default, so a leftover manual dev node never collides with the rig.
const BASE_PORT: u16 = 18650;
const NODES: usize = 3;

// Not 18545/18546: those are the documented tunnel-port examples, so
// on a dev machine they may be real forwarded ports.
const LB_PUBLIC: &str = "127.0.0.1:18700";
const LB_ADMIN: &str = "127.0.0.1:18701";
/// The writer sidecar's port in the scenarios that run it: its own, so
/// a sidecar left running on the default port does not take the writes.
const WRITER_PORT: u16 = 18702;

/// Boot cover: image pulls are done by then (the script waits out its
/// own 60 s), and admission needs flip_after probe rounds on top.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Scenario order for `rig all`: quick smokes first, the acceptance
/// scenario last.
const SCENARIOS: [&str; 10] = [
    "boot",
    "distribution",
    "denylist",
    "forward-to-node",
    "kill-recover",
    "wrong-entity",
    "wrong-block",
    "frozen-head",
    "reference-down",
    "offer-accepted",
];

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("all") => cancel_on_ctrl_c(all()).await,
        // The load command owns its Ctrl-C instead: the workers stop,
        // and the report still covers everything sent so far.
        Some("load") => load_command(std::env::args().skip(2)).await,
        Some("relay") => relay_command(std::env::args().skip(2)).await,
        Some(name) if SCENARIOS.contains(&name) => cancel_on_ctrl_c(scenario(name)).await,
        _ => usage(),
    }
}

/// Ctrl-C cancels the run at its current await point; the cancelled
/// future drops whatever it holds, so the stack tears down through
/// the same drops as any other ending. The exit must come after the
/// select! expression — that is where the losing future is dropped.
async fn cancel_on_ctrl_c(run: impl Future<Output = ()>) {
    let interrupted = tokio::select! {
        _ = run => false,
        _ = tokio::signal::ctrl_c() => true,
    };
    if interrupted {
        println!("rig: interrupted");
        std::process::exit(130);
    }
}

async fn scenario(name: &str) {
    match name {
        "boot" => boot().await,
        "denylist" => denylist().await,
        "distribution" => distribution().await,
        "forward-to-node" => forward_to_node().await,
        "kill-recover" => kill_recover().await,
        "wrong-entity" => liar(relay::Lie::Entity, "entity").await,
        "wrong-block" => liar(relay::Lie::Block, "block").await,
        "frozen-head" => frozen_head().await,
        "reference-down" => reference_down().await,
        "offer-accepted" => offer_accepted().await,
        _ => unreachable!("scenario {name} is listed but not dispatched"),
    }
}

/// Every scenario in sequence, each on its own fresh stack; the first
/// failure ends the run.
async fn all() {
    for name in SCENARIOS {
        println!("rig: === {name} ===");
        scenario(name).await;
    }
    println!("rig: all scenarios passed");
}

fn usage() -> ! {
    eprintln!(
        "usage: rig all\n       rig <scenario>   ({})\n       rig load --target <url> [--concurrency N] [--duration SECONDS]\n       rig relay --listen <host:port> --upstream <url> [--lie entity|block|frozen-head]",
        SCENARIOS.join(" | ")
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
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            flag.store(true, Ordering::Relaxed);
        }
    });
    let started = Instant::now();
    let stats = load::run(&target, concurrency, duration, stop.clone()).await;
    report(&stats, started.elapsed());
    if stop.load(Ordering::Relaxed) {
        println!("rig: interrupted");
        std::process::exit(130);
    }
    if stats.failed > 0 {
        std::process::exit(1);
    }
}

async fn relay_command(mut args: impl Iterator<Item = String>) {
    let (mut listen, mut upstream, mut lie) = (None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| usage());
        match (flag.as_str(), value.as_str()) {
            ("--listen", _) => {
                listen = Some(
                    value
                        .parse::<std::net::SocketAddr>()
                        .unwrap_or_else(|_| usage()),
                )
            }
            ("--upstream", _) => upstream = Some(value.parse().unwrap_or_else(|_| usage())),
            ("--lie", "entity") => lie = Some(relay::Lie::Entity),
            ("--lie", "block") => lie = Some(relay::Lie::Block),
            ("--lie", "frozen-head") => lie = Some(relay::Lie::FrozenHead),
            _ => usage(),
        }
    }
    let (Some(listen), Some(upstream)) = (listen, upstream) else {
        usage()
    };
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .unwrap_or_else(|error| panic!("cannot listen on {listen}: {error}"));
    match lie {
        Some(lie) => println!("rig: relay {listen} -> {upstream}, lying: {lie:?}"),
        None => println!("rig: relay {listen} -> {upstream}, honest"),
    }
    relay::serve(listener, upstream, lie).await;
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
    let mut latencies = stats.latencies.clone();
    if !latencies.is_empty() {
        latencies.sort_unstable();
        let avg: Duration = latencies.iter().sum::<Duration>() / latencies.len() as u32;
        println!(
            "rig: latency: avg {} ms, p50 {} ms, p99 {} ms",
            avg.as_millis(),
            latencies[latencies.len() / 2].as_millis(),
            latencies[latencies.len() * 99 / 100].as_millis(),
        );
    }
    if let Some(reason) = &stats.first_failure {
        println!("rig: first failure: {reason}");
    }
}

/// One HTTP client for everything but the load loop (which has its own
/// timeout policy): every scenario call rides one connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("client")
    })
}

/// The workspace root, from the rig crate's own location — correct
/// regardless of the directory the rig is invoked from.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

/// A running stack: the LB over its fleet. Dropping it tears both
/// down — fields drop in declaration order, so the LB stops before
/// the containers it probes.
struct Stack {
    // Never read, held for its Drop: the process dies with the stack.
    #[allow(dead_code)]
    lb: Lb,
    fleet: Fleet,
}

/// Fleet up, LB booted over it and ready — where every scenario
/// starts.
async fn start_stack() -> Stack {
    let root = workspace_root();
    let fleet = Fleet::start(&root, NODES, BASE_PORT);
    let chain_id = fleet.chain_id();
    println!("rig: fleet of {NODES} up, chain id {chain_id}");

    let config = render_config(&root, &fleet, chain_id);
    let mut lb = Lb::spawn(&root, &config, None);
    wait_ready(&mut lb).await;
    Stack { lb, fleet }
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
    let victim = &stack.fleet.nodes()[0];

    println!("rig: load on http://{LB_PUBLIC}: 4 workers for 15s");
    let load_started = Instant::now();
    let load = tokio::spawn(async {
        load::run(
            &format!("http://{LB_PUBLIC}"),
            4,
            Duration::from_secs(15),
            Arc::new(AtomicBool::new(false)),
        )
        .await
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
    let started = Instant::now();
    let stats = load::run(
        &format!("http://{LB_PUBLIC}"),
        4,
        Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    report(&stats, started.elapsed());
    assert_eq!(stats.failed, 0, "a healthy fleet must serve everything");

    let served = served_counts(&stack.fleet).await;
    for (node, count) in stack.fleet.nodes().iter().zip(&served) {
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
    let client = client();
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
        served_counts(&stack.fleet).await.iter().sum::<u64>(),
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
    let victim = &stack.fleet.nodes()[0];
    let client = client();
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
        served_counts(&stack.fleet).await.iter().sum::<u64>(),
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
    client()
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
    /// readable. `reference` is the Arkiv endpoint it reads, if any.
    fn spawn(root: &Path, config: &Path, reference: Option<&str>) -> Self {
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
        let mut command = Command::new(&binary);
        command.arg(config);
        // The machine's own Arkiv endpoint must not leak into the run:
        // the rig decides the reference.
        command
            .env_remove("ARKIV_RPC_URL")
            .env_remove("ARKIV_API_KEY");
        // The log goes to a file the scenarios read: no colour codes
        // between a field's name and its value.
        command.env("NO_COLOR", "1");
        if let Some(reference) = reference {
            command.env("ARKIV_RPC_URL", reference);
        }
        let child = command
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn arkiv-lb");
        println!("rig: arkiv-lb started (logs: {})", log.display());
        Self { child, log }
    }
}

impl Drop for Lb {
    /// SIGTERM first — the stop the deployment sends — so every run
    /// exercises the graceful path; SIGKILL only if the drain hangs.
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("rig: arkiv-lb ignored SIGTERM for 10s, killing it");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls the admin `/health` until `ready` — the boot window is closed
/// and every healthy provider is admitted.
async fn wait_ready(lb: &mut Lb) {
    let started = Instant::now();
    let client = client();
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

/// Servers the rig runs as its own tasks, the relays and the beacon
/// stand-in; aborted on drop, so a scenario that ends, however it
/// ends, leaves no listener behind.
struct Servers(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Servers {
    fn drop(&mut self) {
        for server in &self.0 {
            server.abort();
        }
    }
}

/// A relay in front of `upstream` on a free loopback port, among the
/// scenario's servers; its URL.
async fn start_relay(servers: &mut Servers, upstream: &str, lie: Option<relay::Lie>) -> String {
    let (url, task) = bind_relay("127.0.0.1:0", upstream, lie).await;
    servers.0.push(task);
    url
}

/// One relay on its own, for a scenario that stops or replaces it
/// while the LB runs: it keeps its address, so what the LB's config
/// names stays reachable there. Aborted on drop.
struct Relay {
    url: String,
    upstream: String,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    /// A relay on a free loopback port in front of `upstream`.
    async fn start(upstream: &str, lie: Option<relay::Lie>) -> Self {
        let (url, task) = bind_relay("127.0.0.1:0", upstream, lie).await;
        Self {
            url,
            upstream: upstream.to_owned(),
            task,
        }
    }

    /// Stops it and waits until it is gone, so its port is free: a
    /// drop only asks. Nothing answers at its address until `restart`.
    /// Stopping a stopped one is nothing: a handle awaited once must
    /// not be awaited again.
    async fn stop(&mut self) {
        if self.task.is_finished() {
            return;
        }
        self.task.abort();
        let _ = (&mut self.task).await;
    }

    /// Stopped, and started again at the same address, with `lie`.
    async fn restart(&mut self, lie: Option<relay::Lie>) {
        self.stop().await;
        let listen = self.url.trim_start_matches("http://");
        let (url, task) = bind_relay(listen, &self.upstream, lie).await;
        assert_eq!(url, self.url, "the same address again");
        self.task = task;
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The relay task on the given address, port 0 for any free one, and
/// its URL.
async fn bind_relay(
    listen: &str,
    upstream: &str,
    lie: Option<relay::Lie>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("bind a relay");
    let url = format!("http://{}", listener.local_addr().expect("relay address"));
    let upstream = upstream.parse().expect("the node's url parses");
    (url, tokio::spawn(relay::serve(listener, upstream, lie)))
}

/// The integrity scenarios: three providers serving one dev chain
/// through relays, one of them telling `lie`. The liar is taken out
/// with the evidence in the log naming `check` as the read that
/// differed, the honest two are matched and carry the traffic.
async fn liar(lie: relay::Lie, check: &str) {
    let root = workspace_root();
    // One chain behind every provider: they agree on data, so a
    // difference is one the LB has to explain.
    let fleet = Fleet::start(&root, 1, BASE_PORT);
    let chain_id = fleet.chain_id();
    let node = &fleet.nodes()[0].url;
    // The marketplace writes the LB's listing at start through the
    // sidecar: the live entity the integrity round samples, on a chain
    // that has no other.
    let writer = Writer::start(&root, node, WRITER_PORT).await;
    let mut relays = Servers(Vec::new());
    let providers = [
        ("honest-0", start_relay(&mut relays, node, None).await),
        ("honest-1", start_relay(&mut relays, node, None).await),
        ("liar", start_relay(&mut relays, node, Some(lie)).await),
    ];
    println!("rig: one dev chain behind 3 relays, one lying: {lie:?}");

    let config = render_marketplace_config(
        &root,
        &providers,
        chain_id,
        &writer.url,
        "",
        "",
        "[integrity]\n\
         # Blocks are 250 ms on the dev node, so the lag tolerance of 30\n\
         # blocks is ~7 s: a 2 s confirm stays well inside it.\n\
         interval = \"5s\"\nconfirm_after = \"2s\"\n",
    );
    let mut lb = Lb::spawn(&root, &config, Some(node));
    wait_ready(&mut lb).await;

    let started = Instant::now();
    wait_nodes(
        "the liar's divergence is confirmed",
        Duration::from_secs(90),
        |nodes| provider(nodes, "liar")["integrity_verdict"] == "divergence",
    )
    .await;
    println!(
        "rig: divergence confirmed {:.1}s after ready",
        started.elapsed().as_secs_f32()
    );
    let nodes = fetch_nodes().await;
    let liar = provider(&nodes, "liar");
    assert_eq!(liar["eligible"], false, "out of rotation: {liar}");
    assert_eq!(liar["ineligibility_reason"], "integrity", "{liar}");
    for id in ["honest-0", "honest-1"] {
        let honest = provider(&nodes, id);
        assert_eq!(honest["integrity_verdict"], "match", "{honest}");
        assert_eq!(honest["eligible"], true, "{honest}");
    }
    let log = std::fs::read_to_string(&lb.log).expect("read lb.log");
    assert!(
        log.lines().any(|line| line.contains("divergence confirmed")
            && line.contains("provider=liar")
            && line.contains(&format!("check=\"{check}\""))),
        "the evidence event naming the {check} read is in {}",
        lb.log.display()
    );

    // The public endpoint serves through the honest two only.
    let served_before = provider(&nodes, "liar")["served"].clone();
    let stats = load::run(
        &format!("http://{LB_PUBLIC}"),
        4,
        Duration::from_secs(3),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert_eq!(stats.failed, 0, "clients see nothing of the liar");
    assert!(stats.ok > 0, "the load ran");
    let nodes = fetch_nodes().await;
    assert_eq!(
        provider(&nodes, "liar")["served"],
        served_before,
        "no traffic to the liar"
    );

    // Torn down in this order: the LB before the providers and the
    // sidecar it talks to, the chain last.
    drop(lb);
    drop(relays);
    drop(writer);
    drop(fleet);
    println!("rig: ok");
}

/// A config with the marketplace on, pointed at the sidecar: the
/// providers as static entries, `health` added to its section,
/// `marketplace` to its, and `rest` after them.
fn render_marketplace_config(
    root: &Path,
    providers: &[(&str, String)],
    chain_id: u64,
    writer_url: &str,
    health: &str,
    marketplace: &str,
    rest: &str,
) -> PathBuf {
    let dir = root.join("target/rig");
    std::fs::create_dir_all(&dir).expect("create target/rig");
    let mut config = format!(
        "# Rendered by the rig — do not edit.\n\
         [listen]\npublic = \"{LB_PUBLIC}\"\nadmin = \"{LB_ADMIN}\"\n\n\
         [health]\nchain_id = {chain_id}\n{health}\n\
         [marketplace]\nwriter_url = \"{writer_url}\"\n\
         wei_per_call = \"1000000000000000\"\ntunnel_server = \"127.0.0.1:7000\"\n\
         {marketplace}\n{rest}"
    );
    for (id, url) in providers {
        config.push_str(&format!(
            "\n[[providers]]\nid = \"{id}\"\nurl = \"{url}\"\n"
        ));
    }
    let path = dir.join("config.toml");
    std::fs::write(&path, config).expect("write rig config");
    path
}

/// The provider tooling from arkiv-community-node posts an offer, the
/// LB parses it and accepts it, the tooling parses the agreement the LB
/// wrote and signs its tunnel token, and the LB admits that token. The
/// two codebases share no code, only the record specs and the token's
/// message, so this is where drift between them shows.
async fn offer_accepted() {
    let root = workspace_root();
    let fleet = Fleet::start(&root, 1, BASE_PORT);
    let chain_id = fleet.chain_id();
    let node = &fleet.nodes()[0].url;
    let writer = Writer::start(&root, node, WRITER_PORT).await;
    let lb_address = writer.address().await;

    // Discovery every 2 s rather than 5 min, so the acceptance follows
    // the offer at once.
    let config = render_marketplace_config(
        &root,
        &[],
        chain_id,
        &writer.url,
        "",
        "discovery_interval = \"2s\"\n",
        "",
    );
    let mut lb = Lb::spawn(&root, &config, Some(node));
    wait_ready(&mut lb).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the beacon");
    let beacon_url = format!("http://{}", listener.local_addr().expect("beacon address"));
    let beacon = Servers(vec![tokio::spawn(provider::serve_beacon(listener))]);
    let tooling = provider::Tooling::prepare(&root, node, &beacon_url, &lb_address);

    // The offer: the tooling reads the LB's listing and posts against it.
    let posted = tooling.run("post-offer");
    let printed = provider::text(&posted);
    println!("{printed}");
    assert!(posted.status.success(), "post-offer failed");
    assert!(printed.contains("Posted: offer"), "{printed}");

    // The LB parses the offer and writes an agreement.
    wait_nodes("the offer is accepted", Duration::from_secs(60), |nodes| {
        nodes.as_array().is_some_and(|nodes| {
            nodes
                .iter()
                .any(|node| node["id"] == provider::PROVIDER_ADDRESS)
        })
    })
    .await;
    let nodes = fetch_nodes().await;
    let accepted = provider(&nodes, provider::PROVIDER_ADDRESS);
    assert_eq!(accepted["source"], "marketplace", "{accepted}");
    let agreement = accepted["agreement_id"]
        .as_str()
        .expect("an agreement id")
        .to_lowercase();
    let port = accepted["url"]
        .as_str()
        .and_then(|url| url.trim_end_matches('/').rsplit(':').next())
        .expect("a tunnel port")
        .to_owned();
    println!("rig: accepted, agreement {agreement} on port {port}");

    // The tooling parses the agreement the LB wrote, and the counter
    // record it opens for it, which is a second write after the
    // agreement and can land a moment later.
    let started = Instant::now();
    let printed = loop {
        let status = tooling.run("status");
        let printed = provider::text(&status);
        assert!(status.status.success(), "status failed: {printed}");
        if printed.contains("Counting: 0 requests since block") {
            break printed;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the tooling never read the counter record the LB wrote: {printed}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    println!("{printed}");
    assert!(
        printed
            .to_lowercase()
            .contains(&format!("agreement: {agreement}, tunnel port {port},")),
        "the tooling reads the agreement the LB wrote: {printed}"
    );

    // The tooling signs the tunnel token over the agreement it read.
    let started = tooling.run("start-tunnel");
    let printed = provider::text(&started);
    println!("{printed}");
    assert!(started.status.success(), "start-tunnel failed");
    let settings = tooling.tunnel_settings();
    assert_eq!(settings["TUNNEL_AGREEMENT"].to_lowercase(), agreement);
    assert_eq!(settings["TUNNEL_REMOTE_PORT"], port);
    let token = settings["TUNNEL_TOKEN"].clone();

    // The tunnel server would carry the token to the LB's admission
    // route in its callbacks; the rig posts them in its place, the way
    // frps does, and the LB admits the token the tooling signed.
    let port: u16 = port.parse().expect("a port number");
    let login = admission("Login", &login_body(&agreement, &token)).await;
    assert_eq!(login, serde_json::json!({ "unchange": true }), "login");
    let proxy = admission("NewProxy", &new_proxy_body(&agreement, &token, port)).await;
    assert_eq!(proxy, serde_json::json!({ "unchange": true }), "proxy");
    println!("rig: the token the tooling signed is admitted");

    // The same body with the token's last byte changed is another
    // signer's; the same token for another port is not the agreement.
    let mut wrong = token.clone();
    wrong.replace_range(wrong.len() - 2.., "00");
    let refused = admission("NewProxy", &new_proxy_body(&agreement, &wrong, port)).await;
    assert_eq!(refused["reject"], true, "{refused}");
    assert!(
        refused["reject_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not made by the agreement's provider")),
        "{refused}"
    );
    let refused = admission("NewProxy", &new_proxy_body(&agreement, &token, port + 1)).await;
    assert_eq!(refused["reject"], true, "{refused}");
    assert!(
        refused["reject_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("requested, agreement assigns")),
        "{refused}"
    );

    drop(lb);
    drop(beacon);
    drop(writer);
    drop(fleet);
    println!("rig: ok");
}

/// One admission callback to the LB, as frps posts it: the op and
/// version in the query and the body alike; the LB's answer.
async fn admission(op: &str, content: &serde_json::Value) -> serde_json::Value {
    client()
        .post(format!("http://{LB_ADMIN}/admission?op={op}&version=0.1.0"))
        .json(&serde_json::json!({ "version": "0.1.0", "op": op, "content": content }))
        .send()
        .await
        .expect("the admission route answers")
        .json()
        .await
        .expect("the answer is json")
}

/// The client's metas, as frpc sends what the tooling wrote.
fn metas(agreement: &str, token: &str) -> serde_json::Value {
    serde_json::json!({ "agreement": agreement, "token": token })
}

/// A login's content, the fields frps v0.61.1 sends; the route decides
/// on the metas.
fn login_body(agreement: &str, token: &str) -> serde_json::Value {
    serde_json::json!({
        "version": "0.61.1", "os": "linux", "arch": "amd64",
        "metas": metas(agreement, token), "client_spec": {}, "pool_count": 1,
        // Where frpc connected from. The route only names the client
        // by it in the log, so a documentation address stands in.
        "client_address": "203.0.113.5:53502",
    })
}

/// A proxy registration's content, the same way.
fn new_proxy_body(agreement: &str, token: &str, port: u16) -> serde_json::Value {
    serde_json::json!({
        "user": { "user": "", "metas": metas(agreement, token), "run_id": "rig" },
        "proxy_name": format!("node-rpc-{port}"), "proxy_type": "tcp", "remote_port": port,
    })
}

/// A provider that falls behind the chain is lag, not lying: it leaves
/// rotation on the lag path with no integrity verdict against it, and
/// comes back when it catches up.
async fn frozen_head() {
    let root = workspace_root();
    let fleet = Fleet::start(&root, 1, BASE_PORT);
    let chain_id = fleet.chain_id();
    let node = &fleet.nodes()[0].url;
    let writer = Writer::start(&root, node, WRITER_PORT).await;
    let mut relays = Servers(Vec::new());
    // The one that will fall behind keeps its address through its
    // restarts, so the provider's URL stays what the config says.
    // Honest first: it is in rotation when it gets stuck.
    let mut stuck = Relay::start(node, None).await;
    let providers = [
        ("honest-0", start_relay(&mut relays, node, None).await),
        ("honest-1", start_relay(&mut relays, node, None).await),
        ("stuck", stuck.url.clone()),
    ];
    println!("rig: one dev chain behind 3 relays");

    // The reference head sampled at every probe round, not every
    // minute: lag is the distance to that sample, and the chain moves
    // four blocks a second here.
    let config = render_marketplace_config(
        &root,
        &providers,
        chain_id,
        &writer.url,
        "ref_height_interval = \"0s\"\n",
        "",
        "[integrity]\ninterval = \"5s\"\nconfirm_after = \"2s\"\n",
    );
    let mut lb = Lb::spawn(&root, &config, Some(node));
    wait_ready(&mut lb).await;
    assert_eq!(provider(&fetch_nodes().await, "stuck")["eligible"], true);

    // Stuck: the relay replaced by one that keeps answering the head
    // of this moment.
    stuck.restart(Some(relay::Lie::FrozenHead)).await;
    println!("rig: one provider stuck at the head of this moment");
    let frozen = Instant::now();
    wait_nodes(
        "the stuck provider is out for lag",
        Duration::from_secs(90),
        |nodes| provider(nodes, "stuck")["ineligibility_reason"] == "lag",
    )
    .await;
    println!(
        "rig: out for lag {:.1}s after it got stuck",
        frozen.elapsed().as_secs_f32()
    );
    let nodes = fetch_nodes().await;
    assert_ne!(
        provider(&nodes, "stuck")["integrity_verdict"],
        "divergence",
        "behind is not lying: {}",
        provider(&nodes, "stuck")
    );
    for id in ["honest-0", "honest-1"] {
        assert_eq!(
            provider(&nodes, id)["eligible"],
            true,
            "{}",
            provider(&nodes, id)
        );
    }

    // Caught up: the relay replaced by an honest one again.
    stuck.restart(None).await;
    let replaced = Instant::now();
    wait_nodes(
        "the provider is readmitted once it catches up",
        Duration::from_secs(90),
        |nodes| provider(nodes, "stuck")["eligible"] == true,
    )
    .await;
    println!(
        "rig: readmitted {:.1}s after it caught up",
        replaced.elapsed().as_secs_f32()
    );
    let nodes = fetch_nodes().await;
    assert_ne!(provider(&nodes, "stuck")["integrity_verdict"], "divergence");

    drop(lb);
    drop(stuck);
    drop(relays);
    drop(writer);
    drop(fleet);
    println!("rig: ok");
}

/// The reference taken away judges nobody: no verdict changes, every
/// provider stays in rotation, and the rounds resume when it is back.
async fn reference_down() {
    let root = workspace_root();
    let fleet = Fleet::start(&root, 1, BASE_PORT);
    let chain_id = fleet.chain_id();
    let node = &fleet.nodes()[0].url;
    let writer = Writer::start(&root, node, WRITER_PORT).await;
    let mut relays = Servers(Vec::new());
    let providers = [
        ("honest-0", start_relay(&mut relays, node, None).await),
        ("honest-1", start_relay(&mut relays, node, None).await),
    ];
    // The reference through a relay of its own, so it can go away
    // while the providers, on the same node, stay.
    let mut reference = Relay::start(node, None).await;
    println!("rig: one dev chain behind 2 relays, and a third as the reference");

    let config = render_marketplace_config(
        &root,
        &providers,
        chain_id,
        &writer.url,
        "ref_height_interval = \"0s\"\n",
        "",
        "[integrity]\ninterval = \"5s\"\nconfirm_after = \"2s\"\n",
    );
    let mut lb = Lb::spawn(&root, &config, Some(&reference.url));
    wait_ready(&mut lb).await;
    wait_nodes(
        "the first round matched everyone",
        Duration::from_secs(60),
        |nodes| {
            ["honest-0", "honest-1"]
                .iter()
                .all(|id| provider(nodes, id)["integrity_verdict"] == "match")
        },
    )
    .await;
    let judged_at =
        |nodes: &serde_json::Value| provider(nodes, "honest-0")["integrity_height"].as_u64();
    let before = judged_at(&fetch_nodes().await);

    // Gone: nothing answers on the reference's port.
    reference.stop().await;
    println!("rig: the reference taken away");
    let gone = Instant::now();
    // Three rounds' worth without it: each says it judged nobody.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let nodes = fetch_nodes().await;
    for id in ["honest-0", "honest-1"] {
        let node = provider(&nodes, id);
        assert_eq!(node["eligible"], true, "{node}");
        assert_eq!(
            node["integrity_verdict"], "match",
            "the last verdict stands: {node}"
        );
    }
    assert_eq!(
        judged_at(&nodes),
        before,
        "no round judged while the reference was gone"
    );
    let log = std::fs::read_to_string(&lb.log).expect("read lb.log");
    assert!(
        log.contains("the reference could not be read, nobody is judged"),
        "the rounds said so, in {}",
        lb.log.display()
    );
    println!(
        "rig: {:.0}s without the reference, nobody judged, everyone serving",
        gone.elapsed().as_secs_f32()
    );

    // Back: the next round judges again, at a later height.
    reference.restart(None).await;
    wait_nodes(
        "a round judged again once the reference is back",
        Duration::from_secs(60),
        |nodes| judged_at(nodes) > before,
    )
    .await;
    let nodes = fetch_nodes().await;
    for id in ["honest-0", "honest-1"] {
        assert_eq!(provider(&nodes, id)["integrity_verdict"], "match");
    }

    drop(lb);
    drop(reference);
    drop(relays);
    drop(writer);
    drop(fleet);
    println!("rig: ok");
}
