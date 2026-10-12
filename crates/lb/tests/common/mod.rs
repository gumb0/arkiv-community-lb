//! Test support shared by the integration suites. Each suite is its own
//! binary and uses a part of this, so the unused rest is not a warning.
#![allow(dead_code)]

pub mod fake_chain;
pub mod fake_provider;
pub mod signer;

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

/// A stats file of this test's own, under cargo's target directory,
/// so a service started here saves its rings somewhere that exists and
/// no two tests share one. The default path exists only in the image.
pub fn stats_file() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("stats-{}-{n}.json", std::process::id()))
}
