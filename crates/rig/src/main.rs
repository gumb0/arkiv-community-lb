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

const LB_PUBLIC: &str = "127.0.0.1:18545";
const LB_ADMIN: &str = "127.0.0.1:19545";

/// Boot cover: image pulls are done by then (the script waits out its
/// own 60 s), and admission needs flip_after probe rounds on top.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("boot") => boot().await,
        Some("load") => load_command(std::env::args().skip(2)).await,
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: rig boot\n       rig load --target <url> [--concurrency N] [--duration SECONDS]"
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

async fn boot() {
    let root = workspace_root();
    let fleet = Fleet::start(&root, NODES, BASE_PORT);
    let chain_id = fleet.chain_id();
    println!("rig: fleet of {NODES} up, chain id {chain_id}");

    let config = render_config(&root, &fleet, chain_id);
    let lb = Lb::spawn(&root, &config);
    wait_ready(&lb).await;

    // Drop order tears the LB down before the fleet.
    drop(lb);
    drop(fleet);
    println!("rig: ok");
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
async fn wait_ready(lb: &Lb) {
    let started = Instant::now();
    let client = reqwest::Client::new();
    let url = format!("http://{LB_ADMIN}/health");
    loop {
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
