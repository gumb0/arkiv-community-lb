//! The provider tooling from `arkiv-community-node`, run by the rig the
//! way an operator runs it, against the dev chain: the marketplace CLI
//! from a checkout at `../node`, outside its container. What the
//! container would give it, the rig gives instead: the key file, the
//! node's addresses, and the beacon API, which a dev node has none of.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use std::io::Write;

use axum::{Json, Router, routing::get};
use serde_json::json;

/// The second account of the standard test mnemonic, funded by the dev
/// node like the first, which the writer sidecar signs with: the
/// provider needs gas and a key of its own. Public knowledge.
const PROVIDER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
pub const PROVIDER_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
const PASSWORD: &str = "rig-provider";

/// The marketplace CLI, ready to run against one chain and one LB.
pub struct Tooling {
    dir: PathBuf,
    keystore: PathBuf,
    tunnel_env: PathBuf,
    rpc_url: String,
    beacon_url: String,
    lb_address: String,
}

impl Tooling {
    /// Finds the checkout (`RIG_NODE_DIR`, else `../node` beside this
    /// repository) and writes the provider's key file under
    /// `target/rig/`, the encrypted keystore the CLI reads.
    pub fn prepare(root: &Path, rpc_url: &str, beacon_url: &str, lb_address: &str) -> Self {
        let checkout = std::env::var_os("RIG_NODE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("../node"));
        let dir = checkout.join("marketplace");
        assert!(
            dir.join("node_modules").exists(),
            "no provider tooling at {}: check out arkiv-community-node at ../node (or set \
             RIG_NODE_DIR) and run `npm ci` in its marketplace/",
            dir.display()
        );
        let work = root.join("target/rig");
        std::fs::create_dir_all(&work).expect("create target/rig");
        let keystore = work.join("provider-key.json");
        let tunnel_env = work.join("tunnel.env");
        let _ = std::fs::remove_file(&tunnel_env);
        // ethers from the tooling's own dependencies writes the file,
        // so it is the format the tooling reads.
        let output = Command::new("node")
            .args([
                "--input-type=module",
                "-e",
                "import { Wallet } from 'ethers'; \
                 const [key, password] = process.argv.slice(1); \
                 process.stdout.write(await new Wallet(key).encrypt(password))",
                PROVIDER_KEY,
                PASSWORD,
            ])
            .current_dir(&dir)
            .output()
            .expect("run node to write the provider key file");
        assert!(output.status.success(), "the key file: {}", text(&output));
        std::fs::write(&keystore, &output.stdout).expect("write the provider key file");
        Self {
            dir,
            keystore,
            tunnel_env,
            rpc_url: rpc_url.to_owned(),
            beacon_url: beacon_url.to_owned(),
            lb_address: lb_address.to_owned(),
        }
    }

    /// One CLI command, answered its password prompt if it asks; what
    /// it printed, stdout and stderr together.
    pub fn run(&self, command: &str) -> Output {
        let mut child = Command::new("node")
            .args(["src/cli.ts", command])
            .current_dir(&self.dir)
            .env("NODE_RPC_URL", &self.rpc_url)
            .env("NODE_BEACON_URL", &self.beacon_url)
            .env("MARKETPLACE_KEYSTORE", &self.keystore)
            .env("MARKETPLACE_TUNNEL_ENV", &self.tunnel_env)
            .env("LB_ADDRESS", &self.lb_address)
            // Pipes instead of the rig's own terminal: the rig holds
            // the writing end of the CLI's stdin, and the reading ends
            // of its stdout and stderr.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the provider tooling");
        // The password goes into the stdin pipe now, before the CLI
        // asks for it: the pipe holds the line until the prompt reads
        // it. The handle is dropped by the end of the statement, which
        // closes the pipe: a command waiting for the end of its input
        // would otherwise wait forever. A command that never asks
        // exits with the line unread, and the write then fails with a
        // broken pipe, which is no error.
        let _ = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(format!("{PASSWORD}\n").as_bytes());
        // Reads both output pipes to their end, then waits for the
        // exit: what the CLI printed is what the scenario asserts on.
        child.wait_with_output().expect("the tooling ends")
    }
}

impl Tooling {
    /// The tunnel settings `start-tunnel` wrote, name to value: what
    /// the wrapper script merges into the node's `.env`.
    pub fn tunnel_settings(&self) -> std::collections::HashMap<String, String> {
        std::fs::read_to_string(&self.tunnel_env)
            .expect("start-tunnel wrote the tunnel settings")
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }
}

/// A command's output as one text, for assertions and failure messages.
pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The two beacon API paths the tooling reads for an offer's specs: a
/// version, and a node that is not syncing. A dev node has no
/// consensus client to answer them.
pub async fn serve_beacon(listener: tokio::net::TcpListener) {
    let app = Router::new()
        .route(
            "/eth/v1/node/version",
            get(|| async { Json(json!({ "data": { "version": "rig-beacon/v0" } })) }),
        )
        .route(
            "/eth/v1/node/syncing",
            get(|| async {
                Json(json!({ "data": {
                    "is_syncing": false, "is_optimistic": false, "el_offline": false,
                    "head_slot": "0", "sync_distance": "0",
                } }))
            }),
        );
    axum::serve(listener, app).await.expect("beacon serves");
}
