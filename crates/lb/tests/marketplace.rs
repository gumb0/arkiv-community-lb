//! The marketplace agent over the fake chain: what it reads back at
//! startup, what it writes, and when it refuses to start.

mod common;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use alloy_primitives::Address;
use common::fake_chain::{FakeChain, Transaction};
use lb::chain::reader::Query;
use lb::{
    chain::{
        ChainReader, ChainWriter,
        reader::PAGE_LIMIT,
        records::{
            Agreement, CounterRecord, CounterState, Hardware, KIND_COUNTER, KIND_LB_LISTING,
            LbListing, Offer, Record, Specs, Stored, Wei,
        },
        writer::{Delete, Expiry},
    },
    config::Marketplace,
    marketplace::agent::{Agent, OpenCounter, StartError},
    pool::{Pool, Source},
};

const LB: Address = Address::repeat_byte(0x11);
const RATE: Wei = Wei::new(1_000_000_000_000_000);

fn marketplace() -> Marketplace {
    Marketplace {
        writer_url: "http://127.0.0.1:8560/".to_owned(),
        wei_per_call: RATE,
        tunnel_server: "203.0.113.10:7000".to_owned(),
        max_providers: 100,
        remote_port_start: 20000,
        discovery_interval: Duration::from_secs(300),
        accept_window: Duration::from_secs(7200),
        refresh_interval: Duration::from_secs(3600),
        agreement_life: Duration::from_secs(3 * 24 * 3600),
        listing_life: Duration::from_secs(30 * 24 * 3600),
        counter_record_life: Duration::from_secs(180 * 24 * 3600),
        offer_max_lifetime: Duration::from_secs(2 * 24 * 3600),
        settlement_period: Duration::from_secs(7 * 24 * 3600),
        flush_interval: Duration::from_secs(24 * 3600),
        gas_warn_below: Wei::new(20_000_000_000_000_000),
    }
}

fn listing_of(config: &Marketplace) -> LbListing {
    LbListing {
        wei_per_call: config.wei_per_call,
        tunnel_server: config.tunnel_server.clone(),
        max_providers: config.max_providers,
    }
}

fn provider(n: u8) -> Address {
    Address::repeat_byte(0x20 + n)
}

/// An agreement record the LB wrote before it restarted.
fn seed_agreement(
    chain: &FakeChain,
    provider: Address,
    port: u16,
    life: u64,
) -> alloy_primitives::B256 {
    let record = Agreement {
        provider,
        offer: alloy_primitives::B256::repeat_byte(0x0f),
        wei_per_call: RATE,
        remote_port: port,
    };
    chain.write_as(LB, record.encode(), Expiry::Seconds(life))
}

type FakeAgent = Agent<FakeChain, FakeChain>;

async fn start(
    chain: &FakeChain,
    config: &Marketplace,
    pool: &Arc<Pool>,
) -> Result<FakeAgent, StartError> {
    Agent::start(chain.clone(), chain.clone(), config.clone(), pool.clone()).await
}

const CHAIN_ID: u64 = 1337;
const DAY: u64 = 24 * 3600;

/// An offer as the tooling posts it, against the agent's listing.
fn offer_for(agent: &FakeAgent, chain: &FakeChain) -> Offer {
    Offer {
        lb_listing: agent.listing_key(),
        specs: Specs {
            chain_id: CHAIN_ID,
            head: chain.head(),
            el: "arkiv-reth/v0.2.0".to_owned(),
            cl: "lighthouse/v8.2.1".to_owned(),
            hw: Hardware {
                cpus: 8,
                mem_gb: 32,
            },
        },
    }
}

fn post(chain: &FakeChain, provider: Address, offer: &Offer, life: u64) -> alloy_primitives::B256 {
    chain.write_as(provider, offer.encode(), Expiry::Seconds(life))
}

/// An open counter record the LB wrote before it restarted.
fn seed_counter(
    chain: &FakeChain,
    agreement: alloy_primitives::B256,
    provider: Address,
    count: u64,
    opened_block: u64,
) -> alloy_primitives::B256 {
    let record = CounterRecord {
        agreement,
        provider,
        state: CounterState::Open,
        count,
        wei_per_call: RATE,
        opened_block,
        closed_block: None,
    };
    chain.write_as(LB, record.encode(), Expiry::Seconds(180 * DAY))
}

async fn counter_records(chain: &FakeChain) -> Vec<Stored<CounterRecord>> {
    chain
        .query(&Query::kind(KIND_COUNTER).creator(LB))
        .await
        .expect("query")
        .entities
        .iter()
        .map(|entity| Stored::<CounterRecord>::decode(entity).expect("decodes"))
        .collect()
}

#[tokio::test]
async fn a_first_start_writes_the_listing_and_reloads_nothing() {
    let chain = FakeChain::new(LB, 1337);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    assert_eq!(agent.identity().address, LB);
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());

    let listing = chain
        .entity(agent.listing_key())
        .expect("the listing is on the chain");
    assert_eq!(listing.creator, LB);
    assert_eq!(
        listing.expires_at,
        chain.head() + config.listing_life.as_secs() / 2,
        "the listing lives one listing life"
    );
    let page = chain
        .query(&Query::kind(KIND_LB_LISTING).creator(LB))
        .await
        .expect("query");
    let stored = Stored::<LbListing>::decode(&page.entities[0]).expect("decodes");
    assert_eq!(stored.record, listing_of(&config));
    assert_eq!(
        chain.transactions(),
        [Transaction::Create(agent.listing_key())]
    );
}

#[tokio::test]
async fn agreement_records_become_marketplace_providers() {
    let chain = FakeChain::new(LB, 1337);
    let first = seed_agreement(&chain, provider(1), 20000, 3600);
    let second = seed_agreement(&chain, provider(2), 20003, 3600);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 2);
    let by_key = |key| agreements.iter().find(|a| a.key == key).expect("known");
    assert_eq!(by_key(first).record.remote_port, 20000);
    assert_eq!(by_key(second).record.remote_port, 20003);

    let members = pool.snapshot();
    assert_eq!(members.len(), 2);
    let one = &members[0];
    assert_eq!(one.id, format!("{:#x}", provider(1)));
    assert_eq!(one.url.as_str(), "http://127.0.0.1:20000/");
    assert_eq!(
        one.source,
        Source::Marketplace {
            address: provider(1),
            agreement_id: first,
            port: 20000,
        }
    );
    assert!(!one.eligible(), "born ineligible, like every provider");
    assert_eq!(
        members[1].source,
        Source::Marketplace {
            address: provider(2),
            agreement_id: second,
            port: 20003,
        }
    );
}

#[tokio::test]
async fn an_expired_agreement_is_gone_and_stays_gone() {
    let chain = FakeChain::new(LB, 1337);
    seed_agreement(&chain, provider(1), 20000, 60);
    chain.advance(30);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());
}

#[tokio::test]
async fn a_second_record_for_one_provider_is_skipped() {
    let chain = FakeChain::new(LB, 1337);
    // The older record has been refreshed, so its expiry is the later
    // one; the younger expires first and is the one to ignore.
    let first = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    chain.advance(10);
    seed_agreement(&chain, provider(1), 20001, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].key, first, "the oldest record is kept");
    assert_eq!(pool.snapshot().len(), 1);
}

#[tokio::test]
async fn a_cap_below_the_live_count_evicts_nobody() {
    let chain = FakeChain::new(LB, 1337);
    for n in 1..=3 {
        seed_agreement(&chain, provider(n), 20000 + u16::from(n), 3600);
    }
    let mut config = marketplace();
    config.max_providers = 2;
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    assert_eq!(agent.agreements().len(), 3);
    assert_eq!(pool.snapshot().len(), 3);
}

#[tokio::test]
async fn an_unchanged_listing_is_left_alone() {
    let chain = FakeChain::new(LB, 1337);
    let config = marketplace();
    let existing = chain.write_as(LB, listing_of(&config).encode(), Expiry::Seconds(3600));
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    assert_eq!(agent.listing_key(), existing);
    assert!(chain.transactions().is_empty(), "nothing to write");
}

#[tokio::test]
async fn a_changed_configuration_patches_the_oldest_listing_and_ignores_the_rest() {
    let chain = FakeChain::new(LB, 1337);
    let mut old = marketplace();
    old.wei_per_call = Wei::new(5);
    // The older listing has been refreshed since, so its expiry is the
    // later one: expiry says nothing about age.
    let older = chain.write_as(LB, listing_of(&old).encode(), Expiry::Seconds(20000));
    chain.advance(10);
    let newer = chain.write_as(LB, listing_of(&old).encode(), Expiry::Seconds(1000));
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    assert_eq!(
        agent.listing_key(),
        older,
        "the first created is the oldest"
    );
    assert_eq!(chain.transactions(), [Transaction::Patch(older)]);
    let kept = chain.entity(older).expect("kept");
    let stored: LbListing =
        serde_json::from_slice(&kept.payload).expect("the payload is the listing");
    assert_eq!(stored, listing_of(&config));
    let untouched = chain.entity(newer).expect("left alone");
    let stored: LbListing =
        serde_json::from_slice(&untouched.payload).expect("the payload is the listing");
    assert_eq!(stored, listing_of(&old));
    assert_eq!(
        chain
            .count(&Query::kind(KIND_LB_LISTING).creator(LB))
            .await
            .expect("count"),
        2
    );
}

#[tokio::test]
async fn refuses_to_start_without_the_sidecar() {
    let chain = FakeChain::new(LB, 1337);
    chain.fail_sidecar("connection refused");
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let error = start(&chain, &marketplace(), &pool)
        .await
        .expect_err("refuses");
    assert!(matches!(error, StartError::Sidecar(_)), "{error}");
    assert!(pool.snapshot().is_empty());
}

#[tokio::test]
async fn refuses_to_start_without_the_reference() {
    let chain = FakeChain::new(LB, 1337);
    chain.fail_reference("connection refused");
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let error = start(&chain, &marketplace(), &pool)
        .await
        .expect_err("refuses");
    assert!(matches!(error, StartError::Chain(_)), "{error}");
    assert!(
        chain.transactions().is_empty(),
        "a refused start writes nothing"
    );
}

#[tokio::test]
async fn records_that_do_not_decode_are_skipped() {
    let chain = FakeChain::new(LB, 1337);
    let config = marketplace();
    let mut agreement = Agreement {
        provider: provider(1),
        offer: alloy_primitives::B256::repeat_byte(0x0f),
        wei_per_call: RATE,
        remote_port: 20000,
    }
    .encode();
    agreement.payload = b"not json".to_vec();
    chain.write_as(LB, agreement, Expiry::Seconds(3600));
    let mut listing = listing_of(&config).encode();
    listing.payload = b"{}".to_vec();
    let broken = chain.write_as(LB, listing, Expiry::Seconds(3600));
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    assert!(agent.agreements().is_empty(), "the agreement is skipped");
    assert!(pool.snapshot().is_empty());
    assert_ne!(agent.listing_key(), broken, "the listing is skipped");
    assert_eq!(
        chain.transactions(),
        [Transaction::Create(agent.listing_key())]
    );
}

#[tokio::test]
async fn refuses_to_start_when_the_records_do_not_fit_one_page() {
    let chain = FakeChain::new(LB, 1337);
    for n in 0..=PAGE_LIMIT {
        let address = Address::from_word(alloy_primitives::B256::from(
            alloy_primitives::U256::from(n + 1),
        ));
        seed_agreement(&chain, address, 20000, 3600);
    }
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let error = start(&chain, &marketplace(), &pool)
        .await
        .expect_err("refuses");
    assert!(matches!(error, StartError::TooManyAgreements { count } if count == PAGE_LIMIT + 1));
    assert!(error.to_string().contains("201"), "{error}");
    assert!(pool.snapshot().is_empty(), "nothing was loaded");
    assert!(
        chain.transactions().is_empty(),
        "a refused start writes nothing"
    );
}

#[tokio::test]
async fn a_gone_agreement_leaves_the_pool_at_the_next_poll() {
    let chain = FakeChain::new(LB, 1337);
    let short = seed_agreement(&chain, provider(1), 20000, 60);
    let long = seed_agreement(&chain, provider(2), 20001, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(agent.agreements().len(), 2);

    chain.advance(31);
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].key, long);
    let members = pool.snapshot();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].id, format!("{:#x}", provider(2)));
    assert!(
        chain.entity(short).is_some(),
        "expired, not deleted; just gone from reads"
    );
    assert!(
        chain.transactions().len() <= 1,
        "the reconcile writes nothing"
    );
}

#[tokio::test]
async fn an_agreement_unknown_to_memory_is_adopted_at_the_poll() {
    let chain = FakeChain::new(LB, 1337);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert!(agent.agreements().is_empty());

    // Written outside memory: an acceptance whose answer never came, say.
    let key = seed_agreement(&chain, provider(1), 20005, 3600);
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].key, key);
    assert_eq!(pool.snapshot()[0].url.as_str(), "http://127.0.0.1:20005/");
}

#[tokio::test]
async fn a_poll_over_a_page_skips_the_reconcile() {
    let chain = FakeChain::new(LB, 1337);
    let first = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    for n in 0..PAGE_LIMIT {
        let address = Address::from_word(alloy_primitives::B256::from(
            alloy_primitives::U256::from(n + 1),
        ));
        seed_agreement(&chain, address, 20001, 3600);
    }
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1, "nothing adopted, nothing dropped");
    assert_eq!(agreements[0].key, first);
    assert_eq!(pool.snapshot().len(), 1);
}

#[tokio::test]
async fn one_poll_drops_the_gone_and_adopts_the_new() {
    let chain = FakeChain::new(LB, 1337);
    let expiring = seed_agreement(&chain, provider(1), 20000, 60);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    chain.advance(31);
    let unknown = seed_agreement(&chain, provider(2), 20001, 3600);
    agent.discovery_poll().await;

    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].key, unknown);
    assert_ne!(agreements[0].key, expiring);
    let members = pool.snapshot();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].id, format!("{:#x}", provider(2)));
}

#[tokio::test]
async fn a_poll_that_cannot_read_the_chain_changes_nothing() {
    let chain = FakeChain::new(LB, 1337);
    let expiring = seed_agreement(&chain, provider(1), 20000, 60);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    chain.advance(31);
    let unknown = seed_agreement(&chain, provider(2), 20001, 3600);
    chain.fail_reference("connection refused");
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1, "a failed read is not an absence");
    assert_eq!(agreements[0].key, expiring, "and adopts nothing either");
    assert_eq!(pool.snapshot().len(), 1);

    chain.heal();
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(
        agreements[0].key, unknown,
        "the next poll catches up both ways"
    );
}

// ---------------------------------------------------------------------------
// Discovery and acceptance

#[tokio::test]
async fn an_offer_becomes_an_agreement_and_a_counter_record() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    let offer_key = post(&chain, provider(1), &offer, DAY);
    chain.advance(10);
    let writes_before = chain.transactions().len();

    agent.discovery_poll().await;

    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    let agreement = &agreements[0];
    assert_eq!(agreement.creator, LB);
    assert_eq!(agreement.record.provider, provider(1));
    assert_eq!(agreement.record.offer, offer_key);
    assert_eq!(agreement.record.wei_per_call, config.wei_per_call);
    assert_eq!(agreement.record.remote_port, 20000, "the first port");
    assert_eq!(
        agreement.expires_at,
        chain.head() + config.accept_window.as_secs() / 2,
        "the accept window"
    );
    let members = pool.snapshot();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].id, format!("{:#x}", provider(1)));
    assert!(!members[0].eligible(), "born ineligible");

    let counters = counter_records(&chain).await;
    assert_eq!(counters.len(), 1);
    let counter = &counters[0];
    assert_eq!(counter.record.agreement, agreement.key);
    assert_eq!(counter.record.provider, provider(1));
    assert_eq!(counter.record.state, CounterState::Open);
    assert_eq!(counter.record.count, 0);
    assert_eq!(counter.record.wei_per_call, config.wei_per_call);
    assert_eq!(counter.record.opened_block, chain.head());
    assert_eq!(counter.record.closed_block, None);
    assert_eq!(
        counter.expires_at,
        chain.head() + config.counter_record_life.as_secs() / 2
    );
    assert_eq!(
        agent.open_counters()[&agreement.key],
        Some(OpenCounter {
            key: counter.key,
            opened_block: chain.head(),
        }),
        "the agent knows the record it opened"
    );
    assert_eq!(
        members[0].served.load(Ordering::Relaxed),
        0,
        "counts from 0"
    );
    assert_eq!(
        chain.transactions().len() - writes_before,
        2,
        "the agreement, then its counter record"
    );
    assert!(
        matches!(chain.transactions()[writes_before], Transaction::Create(key) if key == agreement.key)
    );

    // The same offer is not accepted again: an agreement points at it.
    agent.discovery_poll().await;
    assert_eq!(agent.agreements().len(), 1);
    assert_eq!(chain.transactions().len() - writes_before, 2);
}

#[tokio::test]
async fn offers_are_filtered_before_any_gas_is_spent() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    chain.advance(2_000);
    let good = offer_for(&agent, &chain);

    let mut other_chain = good.clone();
    other_chain.specs.chain_id = 7;
    post(&chain, provider(1), &other_chain, DAY);

    // Alive far past offer_max_lifetime: not even read.
    post(&chain, provider(3), &good, 30 * DAY);

    let mut other_listing = good.clone();
    other_listing.lb_listing = alloy_primitives::B256::repeat_byte(0xaa);
    post(&chain, provider(4), &other_listing, DAY);

    // Under agreement already: skipped.
    seed_agreement(&chain, provider(5), 20003, 3600);
    agent.discovery_poll().await;
    post(&chain, provider(5), &good, DAY);

    // A head far behind the current one is not judged: the probes are.
    let mut behind = good.clone();
    behind.specs.head = 1;
    post(&chain, provider(6), &behind, DAY);

    let writes_before = chain.transactions().len();
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 2, "the seeded one and provider 6");
    assert!(
        agreements.iter().any(|a| a.record.provider == provider(6)),
        "a stale head is no reason to skip"
    );
    assert_eq!(chain.transactions().len() - writes_before, 2);
}

#[tokio::test]
async fn an_expired_offer_is_not_accepted() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    post(&chain, provider(1), &offer_for(&agent, &chain), 3600);
    chain.advance(3600 / 2);
    let writes_before = chain.transactions().len();

    agent.discovery_poll().await;
    assert!(agent.agreements().is_empty());
    assert_eq!(chain.transactions().len(), writes_before);
}

#[tokio::test]
async fn an_offer_that_does_not_decode_is_skipped() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    // Anyone can post against the listing. The broken one is the older.
    let mut broken = offer.encode();
    broken.payload = b"not json".to_vec();
    chain.write_as(provider(1), broken, Expiry::Seconds(DAY));
    chain.advance(1);
    let good = post(&chain, provider(2), &offer, DAY);

    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].record.offer, good);
}

#[tokio::test]
async fn more_offers_than_a_page_still_fill_the_slots_from_the_page() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let mut config = marketplace();
    config.max_providers = 3;
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    let address = |n: u64| {
        Address::from_word(alloy_primitives::B256::from(alloy_primitives::U256::from(
            n + 1,
        )))
    };
    // One more than a page, each a block after the last. The reconcile
    // skips a poll over a page; the discovery works the page it got.
    for n in 0..=PAGE_LIMIT {
        post(&chain, address(n), &offer, DAY);
        chain.advance(1);
    }

    agent.discovery_poll().await;
    let mut providers: Vec<Address> = agent
        .agreements()
        .iter()
        .map(|a| a.record.provider)
        .collect();
    providers.sort_unstable();
    assert_eq!(
        providers,
        [address(0), address(1), address(2)],
        "the three oldest"
    );
}

#[tokio::test]
async fn the_cap_full_waits_and_a_freed_slot_goes_to_the_oldest_offer() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let mut config = marketplace();
    config.max_providers = 2;
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    // Two slots. The first provider is accepted alone, so its accept
    // window runs out before the second's. Its offer expires with its
    // agreement: a live one would be accepted again (issue #21).
    let first = post(
        &chain,
        provider(1),
        &offer_for(&agent, &chain),
        config.accept_window.as_secs(),
    );
    agent.discovery_poll().await;
    chain.advance(100);
    let offer = offer_for(&agent, &chain);
    let second = post(&chain, provider(2), &offer, DAY);
    chain.advance(1);
    let third = post(&chain, provider(3), &offer, DAY);
    agent.discovery_poll().await;
    let accepted: Vec<_> = agent.agreements().iter().map(|a| a.record.offer).collect();
    assert_eq!(accepted.len(), 2);
    assert!(
        accepted.contains(&first) && accepted.contains(&second),
        "{accepted:?}"
    );
    assert!(!accepted.contains(&third), "the youngest waits");

    // Nothing changes while the cap is full.
    agent.discovery_poll().await;
    assert_eq!(agent.agreements().len(), 2);

    // The first agreement ends (never refreshed: its accept window
    // runs out) while the second is still alive. The oldest waiting
    // offer takes the freed slot, in the same poll.
    chain.advance(config.accept_window.as_secs() / 2 - 100);
    let fourth = post(&chain, provider(4), &offer, DAY);
    agent.discovery_poll().await;
    let accepted: Vec<_> = agent.agreements().iter().map(|a| a.record.offer).collect();
    assert_eq!(accepted.len(), 2, "{accepted:?}");
    assert!(accepted.contains(&second), "still alive");
    assert!(
        accepted.contains(&third),
        "the oldest waiting offer got the slot"
    );
    assert!(!accepted.contains(&fourth), "the newer one waits");
    let ports: Vec<u16> = agent
        .agreements()
        .iter()
        .map(|a| a.record.remote_port)
        .collect();
    assert!(
        ports.contains(&20000),
        "the freed port is reused: {ports:?}"
    );
}

#[tokio::test]
async fn one_provider_gets_one_agreement_from_its_oldest_offer() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    let older = post(&chain, provider(1), &offer, DAY);
    chain.advance(1);
    let newer = post(&chain, provider(1), &offer, DAY);

    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].record.offer, older);
    assert_ne!(agreements[0].record.offer, newer);
    assert_eq!(pool.snapshot().len(), 1);
}

#[tokio::test]
async fn the_lowest_free_port_is_assigned() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    seed_agreement(&chain, provider(1), 20000, 3600);
    seed_agreement(&chain, provider(2), 20002, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(3), &offer, DAY);
    post(&chain, provider(4), &offer, DAY);

    agent.discovery_poll().await;
    let mut ports: Vec<u16> = agent
        .agreements()
        .iter()
        .map(|a| a.record.remote_port)
        .collect();
    ports.sort_unstable();
    assert_eq!(ports, [20000, 20001, 20002, 20003]);
}

#[tokio::test]
async fn an_unresolved_acceptance_is_adopted_at_the_next_poll_not_repeated() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    let offer_key = post(&chain, provider(1), &offer, DAY);
    let writes_before = chain.transactions().len();

    // The agreement lands, but the answer is a 504.
    chain.unresolved_next();
    agent.discovery_poll().await;
    assert!(agent.agreements().is_empty(), "not known yet");
    assert!(pool.snapshot().is_empty());
    assert_eq!(chain.transactions().len() - writes_before, 1, "it landed");

    // The next poll adopts what landed and accepts nothing twice.
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].record.offer, offer_key);
    assert_eq!(pool.snapshot().len(), 1);
    assert_eq!(
        chain.transactions().len() - writes_before,
        1,
        "no second agreement; the counter record is the flush's to open"
    );
}

#[tokio::test]
async fn a_port_bound_by_a_stale_tunnel_is_skipped() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let mut config = marketplace();
    config.max_providers = 2;
    // A range nothing else on this machine is likely to use.
    config.remote_port_start = 41100;
    // A tunnel whose agreement ended but whose client never left: the
    // tunnel server still has its port bound on loopback.
    let stale = tokio::net::TcpListener::bind("127.0.0.1:41100")
        .await
        .expect("binds");
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(1), &offer, DAY);
    chain.advance(1);
    post(&chain, provider(2), &offer, DAY);

    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1, "the second slot has no usable port");
    assert_eq!(agreements[0].record.provider, provider(1));
    assert_eq!(
        agreements[0].record.remote_port, 41101,
        "past the bound one"
    );

    // The stale client leaves: its port is usable again.
    drop(stale);
    agent.discovery_poll().await;
    let mut ports: Vec<u16> = agent
        .agreements()
        .iter()
        .map(|a| a.record.remote_port)
        .collect();
    ports.sort_unstable();
    assert_eq!(ports, [41100, 41101]);
}

#[tokio::test]
async fn an_unresolved_acceptance_holds_its_port_and_slot() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let mut config = marketplace();
    config.max_providers = 2;
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(1), &offer, DAY);
    chain.advance(1);
    post(&chain, provider(2), &offer, DAY);
    chain.advance(1);
    post(&chain, provider(3), &offer, DAY);

    // The first acceptance lands unanswered. It may have taken port
    // 20000 and a slot, so the same poll gives neither away.
    chain.unresolved_next();
    agent.discovery_poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1, "the second offer, known");
    assert_eq!(agreements[0].record.provider, provider(2));
    assert_eq!(agreements[0].record.remote_port, 20001);

    // The next poll adopts the first: two agreements, two ports, no
    // third at a cap of two.
    agent.discovery_poll().await;
    let mut agreements = agent.agreements();
    agreements.sort_by_key(|a| a.record.remote_port);
    assert_eq!(agreements.len(), 2);
    assert_eq!(agreements[0].record.provider, provider(1));
    assert_eq!(agreements[0].record.remote_port, 20000);
    assert_eq!(agreements[1].record.provider, provider(2));
    assert_eq!(agreements[1].record.remote_port, 20001);
}

#[tokio::test]
async fn an_agreement_stands_when_its_counter_record_does_not_follow() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(1), &offer, DAY);

    chain.fail_sidecar_after(1, "connection refused");
    agent.discovery_poll().await;
    assert_eq!(agent.agreements().len(), 1, "the agreement landed");
    assert_eq!(pool.snapshot().len(), 1);
    assert!(
        counter_records(&chain).await.is_empty(),
        "the counter record did not"
    );
    let key = agent.agreements()[0].key;
    assert_eq!(agent.open_counters()[&key], None, "known to need one");

    chain.heal();
    agent.discovery_poll().await;
    assert_eq!(agent.agreements().len(), 1, "not accepted again");
}

#[tokio::test]
async fn a_failed_acceptance_is_retried_at_the_next_poll() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(1), &offer, DAY);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.discovery_poll().await;
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());

    chain.heal();
    agent.discovery_poll().await;
    assert_eq!(agent.agreements().len(), 1);
    assert_eq!(counter_records(&chain).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The counting state, before any flush

#[tokio::test]
async fn an_open_counter_record_reloads_at_start() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    chain.advance(5);
    let counter = seed_counter(&chain, agreement, provider(1), 48213, 900);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(
        agent.open_counters()[&agreement],
        Some(OpenCounter {
            key: counter,
            opened_block: 900,
        })
    );
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        48213,
        "the period's count so far, counted on from"
    );
}

#[tokio::test]
async fn an_agreement_without_an_open_record_is_known_to_need_one() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(agent.open_counters()[&agreement], None);
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn of_two_open_records_for_one_agreement_the_oldest_counts() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let first = seed_counter(&chain, agreement, provider(1), 10, 1);
    chain.advance(3);
    seed_counter(&chain, agreement, provider(1), 0, 4);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let open = agent.open_counters()[&agreement]
        .clone()
        .expect("one counts");
    assert_eq!(open.key, first);
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        10,
        "the oldest record's count, not the younger's"
    );
}

#[tokio::test]
async fn an_open_record_of_a_gone_agreement_is_not_counted() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    seed_counter(
        &chain,
        alloy_primitives::B256::repeat_byte(0x9c),
        provider(1),
        5,
        1,
    );
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert!(agent.open_counters().is_empty());
}

#[tokio::test]
async fn an_agreement_adopted_at_a_poll_learns_its_open_record() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let counter = seed_counter(&chain, agreement, provider(1), 7, 1);

    agent.discovery_poll().await;
    let open = agent.open_counters()[&agreement]
        .clone()
        .expect("learned at the poll");
    assert_eq!(open.key, counter);
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        7,
        "the entry counts on from the record"
    );

    // The same read runs at every poll, and a record already known is
    // not counted in a second time.
    agent.discovery_poll().await;
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 7);

    // Gone from the chain: gone from the agent's records too.
    chain.advance(3600 / 2);
    agent.discovery_poll().await;
    assert!(agent.open_counters().is_empty());
}

#[tokio::test]
async fn a_counter_record_deleted_from_the_chain_is_forgotten() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let key = seed_counter(&chain, agreement, provider(1), 5, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(
        agent.open_counters()[&agreement]
            .clone()
            .expect("known")
            .key,
        key
    );

    chain
        .delete(&Delete { entity_key: key })
        .await
        .expect("deleted");
    agent.discovery_poll().await;
    assert_eq!(
        agent.open_counters()[&agreement],
        None,
        "the next flush opens a fresh one"
    );
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        5,
        "what the record counted is the entry's now"
    );
}

#[tokio::test]
async fn each_provider_is_seeded_from_the_record_of_its_own_agreement() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let first = seed_agreement(&chain, provider(1), 20000, 3600);
    let second = seed_agreement(&chain, provider(2), 20001, 3600);
    seed_counter(&chain, first, provider(1), 11, 1);
    // A record that counted nothing yet, which seeds nothing.
    seed_counter(&chain, second, provider(2), 0, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    assert_eq!(agent.open_counters().len(), 2);
    for (address, count) in [(provider(1), 11), (provider(2), 0)] {
        let entry = pool.get(&format!("{address:#x}")).expect("in the pool");
        assert_eq!(
            entry.served.load(Ordering::Relaxed),
            count,
            "{address} counts from its own record"
        );
    }
}

#[tokio::test]
async fn a_counter_record_that_does_not_decode_is_skipped() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let mut broken = CounterRecord {
        agreement,
        provider: provider(1),
        state: CounterState::Open,
        count: 5,
        wei_per_call: RATE,
        opened_block: 1,
        closed_block: None,
    }
    .encode();
    broken.payload = b"not json".to_vec();
    chain.write_as(LB, broken, Expiry::Seconds(180 * DAY));
    chain.advance(1);
    let good = seed_counter(&chain, agreement, provider(1), 9, 2);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    // The broken one is older, so only skipping it leaves the good one.
    assert_eq!(
        agent.open_counters()[&agreement]
            .clone()
            .expect("the one that decodes")
            .key,
        good
    );
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 9);
}

#[tokio::test]
async fn a_record_replaced_by_another_is_not_counted_in_again() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let first = seed_counter(&chain, agreement, provider(1), 5, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 5);

    // The record is gone and another stands in its place, by a hand or
    // by a write whose answer was lost.
    chain
        .delete(&Delete { entity_key: first })
        .await
        .expect("deleted");
    let second = seed_counter(&chain, agreement, provider(1), 5, 1);
    agent.discovery_poll().await;
    assert_eq!(
        agent.open_counters()[&agreement]
            .clone()
            .expect("the one on the chain")
            .key,
        second
    );
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        5,
        "the entry was already counting: the count is not added twice"
    );
}

#[tokio::test]
async fn a_poll_over_a_page_of_open_records_leaves_memory_as_it_is() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert_eq!(agent.open_counters()[&agreement], None, "none at start");

    // More records than a page: the count is read, the page is not, so
    // the agreement's record stays unknown and nothing is counted in.
    for _ in 0..=PAGE_LIMIT {
        seed_counter(&chain, agreement, provider(1), 7, 1);
    }
    agent.discovery_poll().await;
    assert_eq!(agent.open_counters()[&agreement], None);
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn refuses_to_start_when_the_open_records_do_not_fit_one_page() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3600);
    for _ in 0..=PAGE_LIMIT {
        seed_counter(&chain, agreement, provider(1), 0, 1);
    }
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let error = start(&chain, &marketplace(), &pool)
        .await
        .expect_err("refuses");
    assert!(
        matches!(error, StartError::TooManyCounters { .. }),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// The flush

/// The chain's counter record, decoded.
async fn counter(chain: &FakeChain, key: alloy_primitives::B256) -> CounterRecord {
    Stored::<CounterRecord>::decode(&chain.entity(key).expect("stored").as_arkiv_entity())
        .expect("decodes")
        .record
}

/// What this provider's entry has counted for its period.
fn served_by(pool: &Pool, address: Address) -> u64 {
    pool.get(&format!("{address:#x}"))
        .expect("in the pool")
        .served
        .load(Ordering::Relaxed)
}

/// Requests answered by this provider, the way the Proxy counts them.
fn serve(pool: &Pool, address: Address, requests: u64) {
    let member = pool.get(&format!("{address:#x}")).expect("in the pool");
    for _ in 0..requests {
        member.record_served();
    }
}

#[tokio::test]
async fn a_flush_writes_what_the_provider_served() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 5);
    let writes_before = chain.transactions().len();

    agent.flush().await;
    let record = counter(&chain, key).await;
    assert_eq!(
        record.count, 15,
        "the record's own count and the five since"
    );
    assert_eq!(record.state, CounterState::Open);
    assert_eq!(record.opened_block, 1, "the opening block stays");
    assert_eq!(
        chain.transactions().len() - writes_before,
        1,
        "one patch, in one batch"
    );

    // The record already says what the entry counted: nothing written.
    agent.flush().await;
    assert_eq!(chain.transactions().len() - writes_before, 1);

    serve(&pool, provider(1), 2);
    agent.flush().await;
    assert_eq!(counter(&chain, key).await.count, 17);
}

#[tokio::test]
async fn an_agreement_without_a_record_gets_a_fresh_one_at_zero() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    serve(&pool, provider(1), 4);
    chain.advance(7);

    agent.flush().await;
    let records = counter_records(&chain).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.record.agreement, agreement);
    assert_eq!(record.record.provider, provider(1));
    assert_eq!(
        record.record.count, 0,
        "opened at zero: the count follows by patch"
    );
    assert_eq!(record.record.state, CounterState::Open);
    assert_eq!(record.record.opened_block, chain.head());
    assert_eq!(record.record.wei_per_call, RATE);
    assert_eq!(
        record.expires_at,
        chain.head() + config.counter_record_life.as_secs() / 2
    );

    // The record is remembered at the next read, and the next flush
    // writes the count into it. What was served before it existed is
    // still on the entry, so nothing is lost by opening at zero.
    serve(&pool, provider(1), 1);
    agent.flush().await;
    assert_eq!(counter_records(&chain).await.len(), 1, "no second record");
    assert_eq!(counter(&chain, record.key).await.count, 5);
}

#[tokio::test]
async fn a_record_this_flush_opened_is_not_counted_into_the_entry_again() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 6);

    // The create lands and its answer is lost, so the LB does not know
    // the key. The record it left behind carries nothing to count.
    chain.unresolved_next();
    agent.flush().await;
    assert_eq!(counter_records(&chain).await.len(), 1, "it landed");
    agent.discovery_poll().await;
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        6,
        "read back, and nothing added to the entry"
    );

    agent.flush().await;
    assert_eq!(counter_records(&chain).await[0].record.count, 6);
}

#[tokio::test]
async fn a_flush_that_cannot_read_the_chain_writes_nothing() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 3);
    let writes_before = chain.transactions().len();

    chain.fail_reference("connection refused");
    agent.flush().await;
    assert_eq!(chain.transactions().len(), writes_before);

    chain.heal();
    agent.flush().await;
    assert_eq!(
        counter(&chain, key).await.count,
        13,
        "written the next time"
    );
}

#[tokio::test]
async fn a_flush_that_did_not_land_is_made_again_at_the_next_one() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 3);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.flush().await;
    assert_eq!(counter(&chain, key).await.count, 10, "nothing written");

    chain.heal();
    serve(&pool, provider(1), 2);
    agent.flush().await;
    assert_eq!(
        counter(&chain, key).await.count,
        15,
        "the count of the moment, not the one the failed write carried"
    );
}

#[tokio::test]
async fn the_agent_flushes_on_its_interval() {
    let chain = FakeChain::new(LB, 1337);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 0, 1);
    let mut config = service_config();
    config.marketplace.as_mut().expect("present").flush_interval = Duration::from_millis(20);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    serve(&service.pool, provider(1), 1);

    wait_for("the count reaches the chain", || {
        chain.entity(key).is_some_and(|entity| {
            Stored::<CounterRecord>::decode(&entity.as_arkiv_entity())
                .map(|stored| stored.record.count == 1)
                .unwrap_or(false)
        })
    })
    .await;
    service.shutdown().await;
}

#[tokio::test]
async fn a_younger_open_record_is_deleted_at_the_flush() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let oldest = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    // A create whose answer was lost, landing after the LB had opened
    // another: two open records for one agreement.
    chain.advance(3);
    let younger = seed_counter(&chain, agreement, provider(1), 0, 4);

    agent.flush().await;
    assert!(chain.entity(younger).is_none(), "the younger is deleted");
    assert_eq!(counter_records(&chain).await.len(), 1);
    assert_eq!(
        counter(&chain, oldest).await.count,
        10,
        "the oldest counts on, untouched"
    );
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 10);

    serve(&pool, provider(1), 2);
    agent.flush().await;
    assert_eq!(counter(&chain, oldest).await.count, 12);
}

// ---------------------------------------------------------------------------
// The settlement period

/// A marketplace with short periods, so a flush by hand can close one.
fn short_periods() -> Marketplace {
    Marketplace {
        settlement_period: Duration::from_secs(20),
        flush_interval: Duration::from_secs(20),
        ..marketplace()
    }
}

#[tokio::test]
async fn a_period_over_closes_the_record_and_opens_its_successor() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = short_periods();
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    serve(&pool, provider(1), 3);
    chain.advance(20);
    let head = chain.head();
    let writes_before = chain.transactions().len();

    agent.flush().await;
    let closed = counter(&chain, key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 13, "what the entry counted this period");
    assert_eq!(closed.opened_block, 1, "the period it covers");
    assert_eq!(closed.closed_block, Some(head));
    assert_eq!(
        chain.transactions().len() - writes_before,
        2,
        "the close, then the successor"
    );
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        0,
        "the written period leaves the entry"
    );

    let records = counter_records(&chain).await;
    let successor = records
        .iter()
        .find(|record| record.key != key)
        .expect("the successor");
    assert_eq!(successor.record.state, CounterState::Open);
    assert_eq!(successor.record.count, 0, "it counts from here");
    assert_eq!(successor.record.opened_block, head, "the close's block");
    assert_eq!(successor.record.agreement, agreement);
    assert_eq!(successor.record.wei_per_call, RATE);

    // The next period counts into the successor.
    serve(&pool, provider(1), 2);
    agent.flush().await;
    assert_eq!(counter(&chain, successor.key).await.count, 2);
    assert_eq!(
        counter(&chain, key).await.count,
        13,
        "the closed one stands"
    );
}

#[tokio::test]
async fn a_record_with_no_count_stays_open_past_its_period() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 0, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &short_periods(), &pool)
        .await
        .expect("starts");
    chain.advance(100);
    let writes_before = chain.transactions().len();

    agent.flush().await;
    assert_eq!(chain.transactions().len(), writes_before, "nothing written");
    assert_eq!(counter(&chain, key).await.state, CounterState::Open);
    assert_eq!(counter_records(&chain).await.len(), 1, "no successor");
}

#[tokio::test]
async fn a_close_that_did_not_land_gets_no_successor_and_is_closed_next_time() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &short_periods(), &pool)
        .await
        .expect("starts");
    serve(&pool, provider(1), 3);
    chain.advance(20);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.flush().await;
    assert_eq!(counter(&chain, key).await.state, CounterState::Open);
    assert_eq!(counter_records(&chain).await.len(), 1, "no successor");
    assert_eq!(
        pool.snapshot()[0].served.load(Ordering::Relaxed),
        13,
        "the period is still the entry's to write"
    );

    chain.heal();
    serve(&pool, provider(1), 1);
    agent.flush().await;
    let closed = counter(&chain, key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 14, "the count of this flush");
    assert_eq!(counter_records(&chain).await.len(), 2);
    assert_eq!(pool.snapshot()[0].served.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn one_flush_writes_each_agreement_its_own_count() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = short_periods();
    // One with no record at all, one with a record still in its
    // period, one with a record whose period is over. Seeded a block
    // apart, so they join the pool in this order and the one that
    // closes is not the first member.
    let fresh = seed_agreement(&chain, provider(3), 20002, 3 * DAY);
    chain.advance(1);
    let counting = seed_agreement(&chain, provider(2), 20001, 3 * DAY);
    chain.advance(1);
    let closing = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let closing_key = seed_counter(&chain, closing, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    assert_eq!(
        pool.snapshot()[0].id,
        format!("{:#x}", provider(3)),
        "the closing one is not first"
    );
    chain.advance(20);
    let counting_key = seed_counter(&chain, counting, provider(2), 0, chain.head());
    serve(&pool, provider(1), 1);
    serve(&pool, provider(2), 2);
    serve(&pool, provider(3), 3);

    agent.flush().await;

    let closed = counter(&chain, closing_key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 11, "its own record's count and its own one");
    let open = counter(&chain, counting_key).await;
    assert_eq!(open.state, CounterState::Open, "its period is not over");
    assert_eq!(open.count, 2, "its own two");
    let opened: Vec<_> = counter_records(&chain)
        .await
        .into_iter()
        .filter(|record| record.record.agreement == fresh)
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].record.count, 0, "opened at zero");

    // Only the closed period left its entry.
    assert_eq!(served_by(&pool, provider(1)), 0);
    assert_eq!(served_by(&pool, provider(2)), 2);
    assert_eq!(served_by(&pool, provider(3)), 3);
}

#[tokio::test]
async fn a_flush_over_the_transaction_limit_lands_in_several() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let records: Vec<_> = (1..=3)
        .map(|n| {
            let agreement = seed_agreement(&chain, provider(n), 20000 + u16::from(n), 3 * DAY);
            seed_counter(&chain, agreement, provider(n), 0, 1)
        })
        .collect();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    for n in 1..=3 {
        serve(&pool, provider(n), u64::from(n));
    }
    // Room for two patches per transaction: three need at least two.
    chain.set_operation_limit(2);
    let writes_before = chain.transactions().len();

    agent.flush().await;
    for (n, key) in records.iter().enumerate() {
        assert_eq!(
            counter(&chain, *key).await.count,
            n as u64 + 1,
            "every count lands, whichever part carried it"
        );
    }
    assert!(chain.transactions().len() - writes_before >= 2, "split");
}

#[tokio::test]
async fn a_successor_that_did_not_land_is_opened_at_the_next_flush() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = short_periods();
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    serve(&pool, provider(1), 3);
    chain.advance(20);

    // The close lands and the batch that carries its successor does not.
    chain.fail_sidecar_after(1, "connection refused");
    agent.flush().await;
    let closed = counter(&chain, key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 13, "the period is written and paid");
    assert_eq!(counter_records(&chain).await.len(), 1, "no successor");
    assert_eq!(
        served_by(&pool, provider(1)),
        0,
        "the closed period left the entry"
    );

    // The agreement has no open record, so the next flush opens one,
    // and the one after writes what has been served since.
    chain.heal();
    serve(&pool, provider(1), 4);
    agent.flush().await;
    let records = counter_records(&chain).await;
    let successor = records
        .iter()
        .find(|record| record.key != key)
        .expect("opened");
    assert_eq!(successor.record.count, 0, "opened at zero");
    agent.flush().await;
    assert_eq!(counter(&chain, successor.key).await.count, 4);
    assert_eq!(counter(&chain, key).await.count, 13, "the closed stands");
}

#[tokio::test]
async fn a_deliberate_stop_writes_the_counts_and_nothing_else() {
    let chain = FakeChain::new(LB, 1337);
    let agreement = seed_agreement(&chain, provider(1), 20000, 3 * DAY);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    // A second record for that agreement, which a scheduled flush
    // would delete.
    chain.advance(1);
    let duplicate = seed_counter(&chain, agreement, provider(1), 0, 2);
    // An agreement with no record, which a scheduled flush would open
    // one for, and a record nobody counts for, which it would close.
    let bare = seed_agreement(&chain, provider(2), 20001, 3 * DAY);
    let stray = seed_counter(
        &chain,
        alloy_primitives::B256::repeat_byte(0x9c),
        provider(3),
        5,
        1,
    );
    let mut config = service_config();
    // A period long over: a scheduled flush would close the record.
    let marketplace = config.marketplace.as_mut().expect("present");
    marketplace.settlement_period = Duration::from_secs(20);
    marketplace.flush_interval = Duration::from_secs(3600);
    chain.advance(100);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    serve(&service.pool, provider(1), 3);
    serve(&service.pool, provider(2), 4);
    let writes_before = chain.transactions().len();

    service.shutdown().await;
    let record = counter(&chain, key).await;
    assert_eq!(record.count, 13, "written at the stop");
    assert_eq!(record.state, CounterState::Open, "no close at a stop");
    assert_eq!(
        counter(&chain, stray).await.state,
        CounterState::Open,
        "the stray waits for a scheduled flush"
    );
    assert_eq!(
        counter(&chain, duplicate).await.count,
        0,
        "the duplicate waits for one too"
    );
    assert!(
        counter_records(&chain)
            .await
            .iter()
            .all(|record| record.record.agreement != bare),
        "no record is opened for the one without: its four requests are lost"
    );
    assert_eq!(counter_records(&chain).await.len(), 3, "nothing opened");
    assert_eq!(
        chain.transactions().len() - writes_before,
        1,
        "one write, and the stop is over"
    );
}

// ---------------------------------------------------------------------------
// The agreement's end

#[tokio::test]
async fn an_agreement_that_ends_closes_its_record_with_what_it_served() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 60);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 2);
    chain.advance(31);

    agent.discovery_poll().await;
    let closed = counter(&chain, key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(
        closed.count, 12,
        "the record's count and the two since the last flush"
    );
    assert_eq!(closed.closed_block, Some(chain.head()));
    assert_eq!(closed.opened_block, 1, "the period it covers");
    assert_eq!(closed.provider, provider(1));
    assert_eq!(closed.wei_per_call, RATE);
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());
}

#[tokio::test]
async fn an_agreement_that_ends_without_a_count_deletes_its_record() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 60);
    let key = seed_counter(&chain, agreement, provider(1), 0, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    chain.advance(31);

    agent.discovery_poll().await;
    assert!(
        chain.entity(key).is_none(),
        "deleted: a closed record at zero would be a receipt for nothing"
    );
    assert!(counter_records(&chain).await.is_empty());
}

/// Whatever the entry counted goes with it. That loses nothing in
/// practice: a provider that served anything was eligible, so its
/// agreement record was extended every hour and expires three days
/// after it stops being eligible, and every flush in those three days
/// opens a record for an agreement without one. A provider that never
/// became eligible never served.
#[tokio::test]
async fn an_agreement_that_ends_without_a_record_writes_nothing() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    seed_agreement(&chain, provider(1), 20000, 60);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 2);
    chain.advance(31);
    let writes_before = chain.transactions().len();

    agent.discovery_poll().await;
    assert_eq!(chain.transactions().len(), writes_before);
    assert!(agent.agreements().is_empty());
}

#[tokio::test]
async fn only_the_agreement_that_ended_is_written_for() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let ending = seed_agreement(&chain, provider(1), 20000, 60);
    let ending_key = seed_counter(&chain, ending, provider(1), 10, 1);
    let staying = seed_agreement(&chain, provider(2), 20001, 3 * DAY);
    let staying_key = seed_counter(&chain, staying, provider(2), 5, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 1);
    serve(&pool, provider(2), 3);
    chain.advance(31);

    agent.discovery_poll().await;
    assert_eq!(counter(&chain, ending_key).await.count, 11);
    let staying_record = counter(&chain, staying_key).await;
    assert_eq!(staying_record.state, CounterState::Open);
    assert_eq!(staying_record.count, 5, "its count waits for the flush");
    assert_eq!(served_by(&pool, provider(2)), 8, "and stays on its entry");
}

#[tokio::test]
async fn two_agreements_that_end_at_once_are_written_for_in_one_batch() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let counted = seed_agreement(&chain, provider(1), 20000, 60);
    let counted_key = seed_counter(&chain, counted, provider(1), 10, 1);
    let empty = seed_agreement(&chain, provider(2), 20001, 60);
    let empty_key = seed_counter(&chain, empty, provider(2), 0, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 2);
    chain.advance(31);
    let writes_before = chain.transactions().len();

    agent.discovery_poll().await;
    let closed = counter(&chain, counted_key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 12);
    assert!(
        chain.entity(empty_key).is_none(),
        "the empty one is deleted"
    );
    assert_eq!(
        chain.transactions().len() - writes_before,
        1,
        "a close and a delete, in one batch"
    );
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());
}

#[tokio::test]
async fn a_final_write_that_did_not_land_leaves_the_record_open() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 60);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 2);
    chain.advance(31);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.discovery_poll().await;
    assert_eq!(counter(&chain, key).await.state, CounterState::Open);
    assert!(
        agent.agreements().is_empty(),
        "the agreement is over either way"
    );
    assert!(pool.snapshot().is_empty(), "and its entry is gone with it");
}

#[tokio::test]
async fn a_record_whose_agreement_is_gone_is_closed_at_the_flush() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    // Records of agreements this LB no longer has: a final write that
    // did not land, or records found at a start.
    let counted = seed_counter(
        &chain,
        alloy_primitives::B256::repeat_byte(0x9c),
        provider(1),
        5,
        1,
    );
    let empty = seed_counter(
        &chain,
        alloy_primitives::B256::repeat_byte(0x9d),
        provider(2),
        0,
        1,
    );
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    chain.advance(3);

    agent.flush().await;
    let closed_at = chain.head();
    let closed = counter(&chain, counted).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 5, "the count it holds is all there is");
    assert_eq!(closed.closed_block, Some(chain.head()));
    assert!(chain.entity(empty).is_none(), "deleted: it never counted");

    // A closed record is not an open one, so no later flush finds it.
    let writes_before = chain.transactions().len();
    chain.advance(3);
    agent.flush().await;
    assert_eq!(chain.transactions().len(), writes_before, "nothing more");
    assert_eq!(counter(&chain, counted).await.closed_block, Some(closed_at));
}

#[tokio::test]
async fn a_gone_agreement_with_two_open_records_keeps_the_oldest_closed() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = alloy_primitives::B256::repeat_byte(0x9c);
    let oldest = seed_counter(&chain, agreement, provider(1), 7, 1);
    chain.advance(3);
    let younger = seed_counter(&chain, agreement, provider(1), 0, 4);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    agent.flush().await;
    assert!(chain.entity(younger).is_none(), "the younger is deleted");
    let closed = counter(&chain, oldest).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(closed.count, 7, "the count the oldest holds");
    assert_eq!(counter_records(&chain).await.len(), 1);
}

#[tokio::test]
async fn a_final_write_that_did_not_land_is_closed_at_the_next_flush() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let agreement = seed_agreement(&chain, provider(1), 20000, 60);
    let key = seed_counter(&chain, agreement, provider(1), 10, 1);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    serve(&pool, provider(1), 2);
    chain.advance(31);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.discovery_poll().await;
    assert_eq!(counter(&chain, key).await.state, CounterState::Open);

    chain.heal();
    agent.flush().await;
    let closed = counter(&chain, key).await;
    assert_eq!(closed.state, CounterState::Closed);
    assert_eq!(
        closed.count, 10,
        "what the last flush wrote: the two since went with the entry"
    );
}

// ---------------------------------------------------------------------------
// The refresh

/// When the entity expires, as the chain has it.
fn expires_at(chain: &FakeChain, key: alloy_primitives::B256) -> u64 {
    chain.entity(key).expect("stored").expires_at
}

/// Flips the marketplace provider's eligibility, the Monitor's job.
fn set_eligible(pool: &Pool, address: Address, value: bool) {
    pool.snapshot()
        .iter()
        .find(|p| p.id == format!("{address:#x}"))
        .expect("in the pool")
        .set_eligible(value);
}

#[tokio::test]
async fn a_refresh_extends_the_listing_and_the_eligible_providers_only() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let eligible = seed_agreement(&chain, provider(1), 20000, 3600);
    let ghost = seed_agreement(&chain, provider(2), 20001, 3600);
    // A static provider has no record to extend, eligible or not.
    let pool = Arc::new(
        Pool::new(&[lb::config::Provider {
            id: "static-1".to_owned(),
            url: "http://127.0.0.1:18545".to_owned(),
        }])
        .expect("pool"),
    );
    let agent = start(&chain, &config, &pool).await.expect("starts");
    set_eligible(&pool, provider(1), true);
    pool.snapshot()
        .iter()
        .find(|p| p.id == "static-1")
        .expect("in the pool")
        .set_eligible(true);
    chain.advance(100);
    let writes_before = chain.transactions().len();

    agent.refresh().await;

    let transactions = chain.transactions();
    assert_eq!(
        transactions.len() - writes_before,
        1,
        "one transaction for all of it"
    );
    let Transaction::Batch(log) = &transactions[writes_before] else {
        panic!("a batch, not {:?}", transactions[writes_before]);
    };
    let mut extended = log.extended.clone();
    extended.sort_unstable();
    let mut expected = vec![agent.listing_key(), eligible];
    expected.sort_unstable();
    assert_eq!(
        extended, expected,
        "the listing and the eligible provider, nothing else"
    );
    let head = chain.head();
    assert_eq!(
        expires_at(&chain, agent.listing_key()),
        head + config.listing_life.as_secs() / 2,
        "the listing, to listing_life"
    );
    assert_eq!(
        expires_at(&chain, eligible),
        head + config.agreement_life.as_secs() / 2,
        "the eligible provider, to agreement_life"
    );
    assert_eq!(
        expires_at(&chain, ghost),
        1 + 3600 / 2,
        "the ghost keeps its accept window"
    );
}

#[tokio::test]
async fn a_refresh_over_the_transaction_limit_lands_in_several() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let keys: Vec<_> = (1..=3)
        .map(|n| seed_agreement(&chain, provider(n), 20000 + u16::from(n), 3600))
        .collect();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    for n in 1..=3 {
        set_eligible(&pool, provider(n), true);
    }
    chain.advance(10);
    // Room for two extends per transaction: the listing and three
    // records need at least two.
    chain.set_operation_limit(2);
    let writes_before = chain.transactions().len();

    agent.refresh().await;

    let head = chain.head();
    for key in &keys {
        assert_eq!(
            expires_at(&chain, *key),
            head + config.agreement_life.as_secs() / 2
        );
    }
    assert_eq!(
        expires_at(&chain, agent.listing_key()),
        head + config.listing_life.as_secs() / 2
    );
    assert!(chain.transactions().len() - writes_before >= 2);
}

#[tokio::test]
async fn an_idle_lb_keeps_its_listing_alive() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    chain.advance(10);

    agent.refresh().await;
    assert_eq!(
        expires_at(&chain, agent.listing_key()),
        chain.head() + config.listing_life.as_secs() / 2
    );
}

#[tokio::test]
async fn a_quarantined_provider_misses_a_cycle_and_is_refreshed_next() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let key = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");

    set_eligible(&pool, provider(1), true);
    chain.advance(10);
    agent.refresh().await;
    let first = expires_at(&chain, key);

    set_eligible(&pool, provider(1), false);
    chain.advance(10);
    agent.refresh().await;
    assert_eq!(expires_at(&chain, key), first, "missed while quarantined");

    set_eligible(&pool, provider(1), true);
    chain.advance(10);
    agent.refresh().await;
    assert!(
        expires_at(&chain, key) > first,
        "refreshed once eligible again"
    );
}

#[tokio::test]
async fn a_record_memory_knows_expired_is_left_out_of_the_refresh() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let expired = seed_agreement(&chain, provider(1), 20000, 60);
    let live = seed_agreement(&chain, provider(2), 20001, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    set_eligible(&pool, provider(1), true);
    set_eligible(&pool, provider(2), true);
    // Past the first record's expiry, with no poll in between: memory
    // still holds it. An extend of a gone record would fail the batch.
    chain.advance(31);

    agent.refresh().await;
    assert_eq!(
        expires_at(&chain, live),
        chain.head() + config.agreement_life.as_secs() / 2,
        "the live one is refreshed"
    );
    assert_eq!(
        expires_at(&chain, expired),
        1 + 30,
        "the gone one is left alone"
    );
}

#[tokio::test]
async fn the_reconcile_learns_a_refreshed_expiry() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");
    post(&chain, provider(1), &offer_for(&agent, &chain), DAY);
    agent.discovery_poll().await;
    let key = agent.agreements()[0].key;
    set_eligible(&pool, provider(1), true);

    // Refreshed within its accept window, then a poll, then past the
    // window: memory must know the record lives on, or the next
    // refresh would leave it out as expired.
    agent.refresh().await;
    let refreshed = expires_at(&chain, key);
    agent.discovery_poll().await;
    chain.advance(config.accept_window.as_secs() / 2 + 1);
    agent.refresh().await;
    assert!(
        expires_at(&chain, key) > refreshed,
        "refreshed again past the accept window"
    );
}

#[tokio::test]
async fn a_refresh_goes_on_when_the_reference_cannot_be_read() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let key = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    set_eligible(&pool, provider(1), true);
    let before = expires_at(&chain, key);
    chain.advance(10);

    // No head, no balance: the sidecar is up, so the extends go out.
    chain.fail_reference("connection refused");
    agent.refresh().await;
    assert!(expires_at(&chain, key) > before);
}

#[tokio::test]
async fn a_failed_refresh_changes_nothing_and_the_next_one_extends() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let key = seed_agreement(&chain, provider(1), 20000, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    set_eligible(&pool, provider(1), true);
    let before = expires_at(&chain, key);
    chain.advance(10);

    chain.fail_sidecar("gas required exceeds allowance");
    agent.refresh().await;
    assert_eq!(expires_at(&chain, key), before);
    assert_eq!(pool.snapshot().len(), 1, "nothing leaves the pool");

    chain.heal();
    agent.refresh().await;
    assert!(expires_at(&chain, key) > before);
}

// ---------------------------------------------------------------------------
// The service wiring

fn service_config() -> lb::config::Config {
    let mut config = lb::config::Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.health.disable_probing = true;
    config.marketplace = Some(marketplace());
    config
}

async fn nodes(service: &lb::service::Service) -> serde_json::Value {
    reqwest::get(format!("http://{}/nodes", service.admin_addr))
        .await
        .expect("nodes answers")
        .json()
        .await
        .expect("json")
}

#[tokio::test]
async fn the_service_starts_the_agent_when_the_section_is_present() {
    let chain = FakeChain::new(LB, 1337);
    let key = seed_agreement(&chain, provider(1), 20007, 3600);
    let mut config = service_config();
    config.providers = vec![lb::config::Provider {
        id: "static-1".to_owned(),
        url: "http://127.0.0.1:18545".to_owned(),
    }];
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");

    // The static provider and the marketplace one share the pool.
    let nodes = nodes(&service).await;
    assert_eq!(nodes.as_array().expect("a list").len(), 2);
    assert_eq!(nodes[0]["id"], "static-1");
    assert_eq!(nodes[0]["source"], "static");
    assert_eq!(nodes[0]["url"], "http://127.0.0.1:18545/");
    assert_eq!(nodes[0]["agreement_id"], serde_json::Value::Null);
    assert_eq!(nodes[1]["id"], format!("{:#x}", provider(1)));
    assert_eq!(nodes[1]["source"], "marketplace");
    assert_eq!(nodes[1]["url"], "http://127.0.0.1:20007/");
    assert_eq!(nodes[1]["agreement_id"], format!("{key:#x}"));
    assert_eq!(nodes[1]["eligible"], false);
    assert!(chain.entity(key).is_some());
    assert!(
        matches!(chain.transactions().as_slice(), [Transaction::Create(_)]),
        "the listing was written, nothing else"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn without_the_section_the_chain_is_never_touched() {
    let chain = FakeChain::new(LB, 1337);
    let mut config = service_config();
    config.marketplace = None;
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    assert!(chain.transactions().is_empty());
    assert_eq!(nodes(&service).await, serde_json::json!([]));
    service.shutdown().await;
}

#[tokio::test]
async fn the_marketplace_refuses_to_start_without_the_reference_url() {
    let config = service_config();
    assert!(config.reference.is_none());
    let error = lb::service::start(config).await.expect_err("refuses");
    assert!(error.to_string().contains("ARKIV_RPC_URL"), "{error}");
}

#[tokio::test]
async fn the_service_refuses_to_start_when_the_sidecar_is_down() {
    let chain = FakeChain::new(LB, 1337);
    chain.fail_sidecar("connection refused");
    let error = lb::service::start_with(service_config(), Some((chain.clone(), chain.clone())))
        .await
        .expect_err("refuses");
    assert!(
        matches!(
            error,
            lb::service::StartError::Marketplace(StartError::Sidecar(_))
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_chain_id_that_disagrees_with_the_sidecar_refuses_to_start() {
    let chain = FakeChain::new(LB, 1337);
    let mut config = service_config();
    config.health.chain_id = Some(7);
    let error = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect_err("refuses");
    assert!(matches!(
        error,
        lb::service::StartError::ChainMismatch {
            configured: 7,
            actual: 1337
        }
    ));
    assert!(error.to_string().contains("1337"), "{error}");

    let mut config = service_config();
    config.health.chain_id = Some(1337);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("the matching chain id starts");
    service.shutdown().await;
}

#[tokio::test]
async fn an_unparsable_writer_url_refuses_to_start() {
    let mut config = service_config();
    config.reference = Some("http://127.0.0.1:1".to_owned());
    config.marketplace.as_mut().expect("present").writer_url = "not a url".to_owned();
    // Refused at the parse, before anything is contacted.
    let error = lb::service::start(config).await.expect_err("refuses");
    assert!(error.to_string().contains("writer_url"), "{error}");
    assert!(error.to_string().contains("not a url"), "{error}");
}

async fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn the_agent_polls_on_its_interval() {
    let chain = FakeChain::new(LB, 1337);
    seed_agreement(&chain, provider(1), 20000, 60);
    let mut config = service_config();
    config
        .marketplace
        .as_mut()
        .expect("present")
        .discovery_interval = Duration::from_millis(20);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    assert_eq!(service.pool.snapshot().len(), 1);

    chain.advance(31);
    wait_for("the expired agreement leaves the pool", || {
        service.pool.snapshot().is_empty()
    })
    .await;
    service.shutdown().await;
}

#[tokio::test]
async fn the_agent_refreshes_on_its_interval() {
    let chain = FakeChain::new(LB, 1337);
    let key = seed_agreement(&chain, provider(1), 20000, 3600);
    let mut config = service_config();
    config
        .marketplace
        .as_mut()
        .expect("present")
        .refresh_interval = Duration::from_millis(20);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    set_eligible(&service.pool, provider(1), true);
    let before = expires_at(&chain, key);
    chain.advance(10);

    wait_for("the eligible provider is refreshed", || {
        expires_at(&chain, key) > before
    })
    .await;
    service.shutdown().await;
}

#[tokio::test]
async fn the_running_agent_accepts_an_offer_posted_after_start() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let mut config = service_config();
    config
        .marketplace
        .as_mut()
        .expect("present")
        .discovery_interval = Duration::from_millis(20);
    let service = lb::service::start_with(config, Some((chain.clone(), chain.clone())))
        .await
        .expect("starts");
    let listing = chain
        .query(&Query::kind(KIND_LB_LISTING).creator(LB))
        .await
        .expect("query")
        .entities[0]
        .key;
    let offer = Offer {
        lb_listing: listing,
        specs: Specs {
            chain_id: CHAIN_ID,
            head: chain.head(),
            el: "arkiv-reth/v0.2.0".to_owned(),
            cl: "lighthouse/v8.2.1".to_owned(),
            hw: Hardware {
                cpus: 8,
                mem_gb: 32,
            },
        },
    };
    post(&chain, provider(1), &offer, DAY);
    wait_for("the offer is accepted", || {
        service.pool.snapshot().len() == 1
    })
    .await;
    let nodes = nodes(&service).await;
    assert_eq!(nodes[0]["id"], format!("{:#x}", provider(1)));
    assert_eq!(nodes[0]["url"], "http://127.0.0.1:20000/");
    service.shutdown().await;
}
