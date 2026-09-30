//! The chain-writer sidecar, run by the rig the way `chain-smoke.sh`
//! runs it: the TS service from `writer/`, signing with the dev chain's
//! prefunded test key. Scenarios with the marketplace need it, since
//! every chain write of the LB goes through it.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// First account of the standard test mnemonic, funded by the dev
/// node. Public knowledge, and the chain it spends on lasts as long as
/// the scenario.
const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// The running sidecar; killed on drop, and its key file removed.
pub struct Writer {
    pub url: String,
    child: Child,
    key_file: PathBuf,
}

impl Writer {
    /// Starts the service against `rpc_url` on `port`, and waits until
    /// it answers `/identity`.
    pub async fn start(root: &Path, rpc_url: &str, port: u16) -> Self {
        let dir = root.join("target/rig");
        std::fs::create_dir_all(&dir).expect("create target/rig");
        // The service reads its key from a file, as it does deployed.
        let key_file = dir.join("writer.key");
        std::fs::write(&key_file, DEV_KEY).expect("write the dev key");
        let log = dir.join("writer.log");
        let out = std::fs::File::create(&log).expect("create writer.log");
        let err = out.try_clone().expect("clone log handle");
        let child = Command::new("node")
            .arg("src/service.ts")
            .current_dir(root.join("writer"))
            .env("WRITER_PRIVATE_KEY_FILE", &key_file)
            .env("ARKIV_RPC_URL", rpc_url)
            .env("ARKIV_API_KEY", "")
            .env("WRITER_PORT", port.to_string())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn the writer sidecar: is node installed, and `npm ci` run in writer/?");
        let mut writer = Self {
            url: format!("http://127.0.0.1:{port}/"),
            child,
            key_file,
        };
        writer.wait_identity(&log).await;
        println!(
            "rig: writer sidecar up on {} (logs: {})",
            writer.url,
            log.display()
        );
        writer
    }

    async fn wait_identity(&mut self, log: &Path) {
        let started = Instant::now();
        let url = format!("{}identity", self.url);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the writer sidecar exited at start ({status}) — see {}",
                    log.display()
                );
            }
            if let Ok(response) = reqwest::get(&url).await
                && response.status().is_success()
            {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the writer sidecar did not answer in 30s — see {}",
                log.display()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.key_file);
    }
}
