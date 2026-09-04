//! The stop signal, delivered to this very process. Alone in its own
//! test binary on purpose: every integration-test file is its own
//! process, and a signal reaches the whole process.

use std::{process::Command, time::Duration};

#[tokio::test]
async fn sigterm_resolves_the_shutdown_signal() {
    let waiting = tokio::spawn(lb::service::shutdown_signal());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let status = Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());

    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("SIGTERM must resolve the shutdown signal")
        .expect("the waiting task");
}
