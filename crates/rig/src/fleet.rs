//! The dev-node fleet, driven through `scripts/dev-node.sh` — the same
//! script operators and CI use, never the Docker API directly.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// One running dev node. Stops its own container on drop, so however a
/// startup or scenario fails, whatever was started gets stopped.
pub struct Node {
    /// Provider id in the rendered config and container name alike.
    pub id: String,
    pub url: String,
    script: PathBuf,
    port: u16,
}

impl Node {
    /// Kills the container. The handle stays alive so the node can be
    /// started again; dropping a stopped node is a harmless no-op.
    pub fn stop(&self) {
        let status = Command::new(&self.script)
            .arg("stop")
            .env("DEV_NODE_NAME", &self.id)
            .status()
            .expect("run dev-node.sh stop");
        assert!(status.success(), "dev-node.sh stop failed for {}", self.id);
    }

    /// Starts the container, waiting until it answers. Started again
    /// after a `stop`, the chain state is fresh — stop discards it —
    /// which no scenario minds: same image, same chain id.
    pub fn start(&self) {
        let status = Command::new(&self.script)
            .arg("start")
            .env("DEV_NODE_NAME", &self.id)
            .env("DEV_NODE_PORT", self.port.to_string())
            .status()
            .expect("run dev-node.sh start");
        assert!(status.success(), "dev-node.sh start failed for {}", self.id);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new(&self.script)
            .arg("stop")
            .env("DEV_NODE_NAME", &self.id)
            .status();
    }
}

pub struct Fleet {
    nodes: Vec<Node>,
}

impl Fleet {
    /// Starts `count` nodes on consecutive host ports. The script waits
    /// for each node to answer before returning, so a returned fleet is
    /// a live one.
    pub fn start(root: &Path, count: usize, base_port: u16) -> Self {
        let script = root.join("scripts/dev-node.sh");
        let nodes = (0..count)
            .map(|i| {
                let port = base_port + i as u16;
                // The Node exists before it is started: a start that
                // failed at the script's readiness timeout has still
                // launched a container, and the drop must reach it.
                let node = Node {
                    id: format!("rig-node-{i}"),
                    url: format!("http://127.0.0.1:{port}"),
                    script: script.clone(),
                    port,
                };
                node.start();
                node
            })
            .collect();
        Self { nodes }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The fleet's chain id, asked of the first node — every dev node
    /// runs the same image, so one answer speaks for all.
    pub fn chain_id(&self) -> u64 {
        let node = &self.nodes[0];
        let output = Command::new(&node.script)
            .arg("chain-id")
            .env("DEV_NODE_NAME", &node.id)
            .env("DEV_NODE_PORT", node.port.to_string())
            .output()
            .expect("run dev-node.sh chain-id");
        assert!(output.status.success(), "dev-node.sh chain-id failed");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("chain id is a number")
    }
}
