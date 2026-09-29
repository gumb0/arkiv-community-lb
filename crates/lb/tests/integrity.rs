//! The integrity checker over fakes: a fake chain as the reference,
//! fake providers serving it, one verdict per provider per round.
//! Millisecond waits and condition polling, never paused time with
//! real sockets.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Address;
use lb::{
    chain::{
        records::{Agreement, Record, Wei},
        writer::Expiry,
    },
    config,
    integrity::{IntegrityChecker, Verdict},
    pool::{HealthSignal, Pool, Provider},
};
use tokio::sync::watch;

mod common;
use common::{
    fake_chain::{FINALITY_LAG, FakeChain},
    fake_provider::{FakeProvider, rpc_provider_on},
};

const CHAIN_ID: u64 = 1337;
const LAG_TOLERANCE: u64 = 30;
/// Long enough that the timer never fires during a test: the tests
/// call `round()` themselves, and the one test of the timer only
/// watches for the first round.
const INTERVAL: Duration = Duration::from_secs(3600);
const CONFIRM_AFTER: Duration = Duration::from_millis(300);
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);

/// A chain with one entity on it, `n` honest providers at its head, a
/// pool that holds them as healthy, and a checker over the chain.
struct Fleet {
    chain: FakeChain,
    providers: Vec<Arc<FakeProvider>>,
    pool: Arc<Pool>,
    checker: Arc<IntegrityChecker<FakeChain>>,
    ready: Arc<AtomicBool>,
}

async fn fleet(n: usize) -> Fleet {
    let chain = FakeChain::new(Address::ZERO, CHAIN_ID);
    chain.advance(200);
    chain.write_as(
        Address::ZERO,
        Agreement {
            provider: Address::ZERO,
            offer: alloy_primitives::B256::ZERO,
            wei_per_call: Wei::new(5),
            remote_port: 20001,
        }
        .encode(),
        Expiry::Seconds(3600),
    );
    let mut providers = Vec::new();
    let mut entries = Vec::new();
    for i in 0..n {
        let (addr, rpc): (SocketAddr, Arc<FakeProvider>) = rpc_provider_on(&chain).await;
        rpc.height.store(chain.head(), Ordering::Relaxed);
        providers.push(rpc);
        entries.push(config::Provider {
            id: format!("p{i}"),
            url: format!("http://{addr}"),
        });
    }
    let pool = Arc::new(Pool::new(&entries).expect("urls parse"));
    for provider in pool.snapshot().iter() {
        provider.set_health(true);
    }
    let ready = Arc::new(AtomicBool::new(true));
    let checker = Arc::new(checker(&pool, &chain, &ready));
    Fleet {
        chain,
        providers,
        pool,
        checker,
        ready,
    }
}

fn checker(
    pool: &Arc<Pool>,
    chain: &FakeChain,
    ready: &Arc<AtomicBool>,
) -> IntegrityChecker<FakeChain> {
    IntegrityChecker::new(
        pool.clone(),
        reqwest::Client::new(),
        chain.clone(),
        config::Integrity {
            interval: INTERVAL,
            confirm_after: CONFIRM_AFTER,
        },
        LAG_TOLERANCE,
        ATTEMPT_TIMEOUT,
        ready.clone(),
    )
}

fn entry(pool: &Pool, i: usize) -> Arc<Provider> {
    pool.get(&format!("p{i}")).expect("in the pool")
}

#[tokio::test]
async fn an_honest_fleet_matches_and_stays_in_rotation() {
    let fleet = fleet(2).await;
    fleet.checker.round().await;
    let finalized = fleet.chain.head() - FINALITY_LAG;
    for i in 0..2 {
        let provider = entry(&fleet.pool, i);
        assert!(provider.eligible());
        assert_eq!(provider.last_verdict(), Some((Verdict::Match, finalized)));
    }
}

#[tokio::test]
async fn a_wrong_block_is_a_divergence_after_a_second_look() {
    let fleet = fleet(2).await;
    fleet.providers[1].lie_block.store(true, Ordering::Relaxed);
    fleet.checker.round().await;
    let honest = entry(&fleet.pool, 0);
    let liar = entry(&fleet.pool, 1);
    assert!(honest.eligible());
    assert!(!liar.eligible());
    assert_eq!(liar.ineligibility_reason(), Some("integrity"));
    assert_eq!(
        liar.last_verdict().map(|(v, _)| v),
        Some(Verdict::Divergence)
    );
    assert_eq!(
        fleet.providers[1].blocks.load(Ordering::Relaxed),
        2,
        "read once, and once again to confirm"
    );
}

#[tokio::test]
async fn a_wrong_entity_is_a_divergence_after_a_second_look() {
    let fleet = fleet(1).await;
    fleet.providers[0].lie_entity.store(true, Ordering::Relaxed);
    fleet.checker.round().await;
    let liar = entry(&fleet.pool, 0);
    assert!(!liar.eligible());
    assert_eq!(
        liar.last_verdict().map(|(v, _)| v),
        Some(Verdict::Divergence)
    );
    assert_eq!(
        fleet.providers[0].asked_keys.lock().expect("asked").len(),
        2
    );
}

#[tokio::test]
async fn a_mismatch_that_clears_on_the_second_look_is_a_match() {
    let fleet = fleet(1).await;
    let liar = fleet.providers[0].clone();
    liar.lie_block.store(true, Ordering::Relaxed);
    let checker = fleet.checker.clone();
    let round = tokio::spawn(async move { checker.round().await });
    // Once the first look has read the block, the tip "reorganises":
    // the second look, after the wait, sees an honest provider.
    let started = std::time::Instant::now();
    while liar.blocks.load(Ordering::Relaxed) == 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "no first look");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    liar.lie_block.store(false, Ordering::Relaxed);
    round.await.expect("round");
    let provider = entry(&fleet.pool, 0);
    assert!(provider.eligible());
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Match)
    );
    assert_eq!(
        liar.blocks.load(Ordering::Relaxed),
        2,
        "both looks happened"
    );
}

#[tokio::test]
async fn a_provider_behind_the_chain_is_stale_and_left_to_the_probes() {
    let fleet = fleet(1).await;
    let finalized = fleet.chain.head() - FINALITY_LAG;
    fleet.providers[0]
        .height
        .store(finalized - 10, Ordering::Relaxed);
    let provider = entry(&fleet.pool, 0);
    let streak = provider.health_streak.load(Ordering::Relaxed);
    fleet.checker.round().await;
    assert!(provider.eligible(), "stale is not lying");
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Stale)
    );
    assert_eq!(
        provider.health_streak.load(Ordering::Relaxed),
        streak,
        "no health tick from a round"
    );
}

#[tokio::test]
async fn a_dead_reference_judges_nobody() {
    let fleet = fleet(2).await;
    fleet.providers[1].lie_block.store(true, Ordering::Relaxed);
    fleet.chain.fail_reference("down");
    fleet.checker.round().await;
    for i in 0..2 {
        let provider = entry(&fleet.pool, i);
        assert!(provider.eligible());
        assert_eq!(provider.last_verdict(), None, "not even the liar");
    }
}

#[tokio::test]
async fn a_provider_a_block_ahead_of_the_reference_is_unknown_this_round() {
    let fleet = fleet(1).await;
    fleet.providers[0]
        .height
        .store(fleet.chain.head() + 1, Ordering::Relaxed);
    fleet.checker.round().await;
    let provider = entry(&fleet.pool, 0);
    assert!(provider.eligible());
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Unknown)
    );
}

#[tokio::test]
async fn a_read_that_times_out_is_unknown_and_no_health_tick() {
    let fleet = fleet(1).await;
    fleet.providers[0]
        .delay_ms
        .store((ATTEMPT_TIMEOUT.as_millis() * 3) as u64, Ordering::Relaxed);
    let provider = entry(&fleet.pool, 0);
    let streak = provider.health_streak.load(Ordering::Relaxed);
    fleet.checker.round().await;
    assert!(provider.eligible());
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Unknown)
    );
    assert_eq!(provider.health_streak.load(Ordering::Relaxed), streak);
}

#[tokio::test]
async fn a_block_that_matches_while_the_sample_is_unknown_is_not_a_pass() {
    let fleet = fleet(1).await;
    fleet.providers[0].lie_entity.store(true, Ordering::Relaxed);
    fleet.checker.round().await;
    let provider = entry(&fleet.pool, 0);
    assert!(!provider.eligible());
    // Honest again, but a block ahead: the block read matches, the
    // sample cannot be judged.
    fleet.providers[0]
        .lie_entity
        .store(false, Ordering::Relaxed);
    fleet.providers[0]
        .height
        .store(fleet.chain.head() + 1, Ordering::Relaxed);
    fleet.checker.round().await;
    assert!(!provider.eligible(), "a pass needs both reads");
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Unknown)
    );
}

#[tokio::test]
async fn a_health_quarantined_provider_is_not_asked() {
    let fleet = fleet(2).await;
    entry(&fleet.pool, 1).quarantine(HealthSignal::Probe);
    fleet.checker.round().await;
    assert!(fleet.providers[0].blocks.load(Ordering::Relaxed) > 0);
    assert_eq!(fleet.providers[1].blocks.load(Ordering::Relaxed), 0);
    assert!(
        fleet.providers[1]
            .asked_keys
            .lock()
            .expect("asked")
            .is_empty()
    );
    assert_eq!(entry(&fleet.pool, 1).last_verdict(), None);
}

#[tokio::test]
async fn a_diverged_provider_stays_out_through_probes_and_returns_on_a_match() {
    let fleet = fleet(1).await;
    fleet.providers[0].lie_block.store(true, Ordering::Relaxed);
    fleet.checker.round().await;
    let provider = entry(&fleet.pool, 0);
    assert!(!provider.eligible());
    for _ in 0..3 {
        provider.record_health(true, 3, HealthSignal::Probe);
    }
    assert!(!provider.eligible(), "probes cannot lift it");
    fleet.providers[0].lie_block.store(false, Ordering::Relaxed);
    fleet.checker.round().await;
    assert!(provider.eligible(), "a passing round does");
}

#[tokio::test]
async fn the_first_round_runs_when_the_boot_window_closes() {
    let fleet = fleet(1).await;
    fleet.ready.store(false, Ordering::Relaxed);
    let checker = checker(&fleet.pool, &fleet.chain, &fleet.ready);
    let (_stop, shutdown) = watch::channel(false);
    tokio::spawn(checker.run(shutdown));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        fleet.providers[0].blocks.load(Ordering::Relaxed),
        0,
        "nothing before ready"
    );
    fleet.ready.store(true, Ordering::Relaxed);
    let started = std::time::Instant::now();
    let provider = entry(&fleet.pool, 0);
    while provider.last_verdict().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no round after ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Match)
    );
}

#[tokio::test]
async fn the_reference_is_asked_at_the_block_the_provider_answered_at() {
    // A provider a few blocks behind, within tolerance, answers the
    // entity at its own height; the reference is pinned there, not at
    // the head, or two-second drift alone could make the two differ.
    let fleet = fleet(1).await;
    let behind = fleet.chain.head() - 5;
    fleet.providers[0].height.store(behind, Ordering::Relaxed);
    fleet.checker.round().await;
    assert_eq!(fleet.chain.pinned_at(), vec![behind]);
    assert_eq!(
        entry(&fleet.pool, 0).last_verdict().map(|(v, _)| v),
        Some(Verdict::Match)
    );
}

#[tokio::test]
async fn a_provider_with_the_block_but_answering_far_back_is_stale() {
    let fleet = fleet(1).await;
    // Past the finalized height, so it has the block, but further
    // behind the head than the lag tolerance allows.
    let behind = LAG_TOLERANCE + 5;
    assert!(behind < FINALITY_LAG);
    fleet.providers[0]
        .height
        .store(fleet.chain.head() - behind, Ordering::Relaxed);
    fleet.checker.round().await;
    let provider = entry(&fleet.pool, 0);
    // Staleness is ignored by the integrity round; it is the Monitor's
    // probes' job to detect it.
    assert!(provider.eligible());
    assert_eq!(
        provider.last_verdict().map(|(v, _)| v),
        Some(Verdict::Stale)
    );
    assert_eq!(
        fleet.providers[0].blocks.load(Ordering::Relaxed),
        1,
        "the block was read"
    );
    assert!(
        fleet.chain.pinned_at().is_empty(),
        "no metered read for a comparison that will not happen"
    );
}

#[tokio::test]
async fn the_reference_is_read_once_per_block_the_fleet_answered_at() {
    let fleet = fleet(3).await;
    // Two at the head, one a block behind: two pinned reads, not three.
    fleet.providers[2]
        .height
        .store(fleet.chain.head() - 1, Ordering::Relaxed);
    fleet.checker.round().await;
    let mut pinned = fleet.chain.pinned_at();
    pinned.sort_unstable();
    assert_eq!(
        pinned,
        vec![fleet.chain.head() - 1, fleet.chain.head()],
        "once per block, not per provider"
    );
    for i in 0..3 {
        assert!(entry(&fleet.pool, i).eligible());
    }
}

#[tokio::test]
async fn rounds_keep_the_configured_cadence_and_stop_at_shutdown() {
    let fleet = fleet(1).await;
    let interval = Duration::from_millis(100);
    let checker = IntegrityChecker::new(
        fleet.pool.clone(),
        reqwest::Client::new(),
        fleet.chain.clone(),
        config::Integrity {
            interval,
            confirm_after: Duration::from_millis(10),
        },
        LAG_TOLERANCE,
        ATTEMPT_TIMEOUT,
        fleet.ready.clone(),
    );
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(checker.run(shutdown));
    let window = interval * 10;
    tokio::time::sleep(window).await;
    let rounds = fleet.providers[0].blocks.load(Ordering::Relaxed);
    // A ratio, not a count: one read per round, at least half the
    // rounds the window holds and no more than it holds plus one.
    assert!(
        (5..=11).contains(&rounds),
        "{rounds} rounds in ten intervals"
    );
    stop.send(true).expect("the task listens");
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the loop returns at shutdown")
        .expect("no panic");
}

/// One more entity on the fleet's chain, alive this many seconds.
fn write_entity(chain: &FakeChain, seconds: u64) -> alloy_primitives::B256 {
    chain.write_as(
        Address::ZERO,
        Agreement {
            provider: Address::ZERO,
            offer: alloy_primitives::B256::ZERO,
            wei_per_call: Wei::new(5),
            remote_port: 20002,
        }
        .encode(),
        Expiry::Seconds(seconds),
    )
}

#[tokio::test]
async fn an_expired_key_is_never_picked() {
    let fleet = fleet(1).await;
    // A second entity that expires soon; the page is read while both
    // are alive, then the chain moves past the short one's expiry.
    let short = write_entity(&fleet.chain, 20);
    fleet.checker.round().await;
    let expires_at = fleet.chain.entity(short).expect("stored").expires_at;
    fleet.chain.advance(expires_at + 1 - fleet.chain.head());
    fleet.providers[0]
        .height
        .store(fleet.chain.head(), Ordering::Relaxed);
    for _ in 0..10 {
        fleet.checker.round().await;
    }
    let asked = fleet.providers[0].asked_keys.lock().expect("asked").clone();
    assert_eq!(asked.len(), 11, "one key per round");
    assert!(
        asked[1..].iter().all(|key| *key != short),
        "the expired key was picked from the old page"
    );
}

#[tokio::test]
async fn a_page_with_no_live_key_left_skips_the_round() {
    let chain = FakeChain::new(Address::ZERO, CHAIN_ID);
    chain.advance(200);
    let only = write_entity(&chain, 20);
    let (addr, rpc) = rpc_provider_on(&chain).await;
    rpc.height.store(chain.head(), Ordering::Relaxed);
    let pool = Arc::new(
        Pool::new(&[config::Provider {
            id: "p0".into(),
            url: format!("http://{addr}"),
        }])
        .expect("url parses"),
    );
    pool.snapshot()[0].set_health(true);
    let checker = checker(&pool, &chain, &Arc::new(AtomicBool::new(true)));
    checker.round().await;
    let provider = pool.snapshot()[0].clone();
    let judged_at = provider.last_verdict().map(|(_, height)| height);
    assert!(judged_at.is_some(), "the first round judged");

    let expires_at = chain.entity(only).expect("stored").expires_at;
    chain.advance(expires_at + 1 - chain.head());
    rpc.height.store(chain.head(), Ordering::Relaxed);
    checker.round().await;
    assert_eq!(
        provider.last_verdict().map(|(_, height)| height),
        judged_at,
        "nobody judged: the verdict is the first round's"
    );
    assert_eq!(rpc.blocks.load(Ordering::Relaxed), 1, "no provider asked");
}

#[tokio::test]
async fn the_page_of_keys_is_read_once_per_interval() {
    let fleet = fleet(1).await;
    let interval = Duration::from_millis(200);
    let checker = checker(&fleet.pool, &fleet.chain, &fleet.ready).with_key_page_interval(interval);
    for _ in 0..3 {
        checker.round().await;
    }
    assert_eq!(fleet.chain.unpinned_reads(), 1, "three rounds, one page");
    tokio::time::sleep(interval + interval / 4).await;
    checker.round().await;
    assert_eq!(fleet.chain.unpinned_reads(), 2, "read again once it is old");
}

#[tokio::test]
async fn a_page_read_that_fails_keeps_the_page_and_is_tried_again() {
    let fleet = fleet(1).await;
    let interval = Duration::from_millis(100);
    let checker = checker(&fleet.pool, &fleet.chain, &fleet.ready).with_key_page_interval(interval);
    checker.round().await;
    assert_eq!(fleet.chain.unpinned_reads(), 1);

    // The page is due again, and the reference cannot answer queries:
    // the round goes on with the page it has.
    tokio::time::sleep(interval + interval / 4).await;
    fleet.chain.fail_queries("queries down");
    checker.round().await;
    let asked = fleet.providers[0].asked_keys.lock().expect("asked").len();
    assert_eq!(asked, 2, "the provider was still asked, from the old page");
    assert_eq!(
        fleet.chain.unpinned_reads(),
        1,
        "the failed read is not a read"
    );

    // Healed: the next round reads the page again rather than waiting
    // out another interval.
    fleet.chain.heal();
    checker.round().await;
    assert_eq!(fleet.chain.unpinned_reads(), 2, "tried again at once");
}
