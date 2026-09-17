//! The marketplace agent over the fake chain: what it reads back at
//! startup, what it writes, and when it refuses to start.

mod common;

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use common::fake_chain::{FakeChain, Transaction};
use lb::chain::reader::Query;
use lb::{
    chain::{
        ChainReader,
        reader::PAGE_LIMIT,
        records::{
            Agreement, CounterRecord, CounterState, Hardware, KIND_COUNTER, KIND_LB_LISTING,
            LbListing, Offer, Record, Specs, Stored, Wei,
        },
        writer::Expiry,
    },
    config::Marketplace,
    marketplace::agent::{Agent, StartError},
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
    let first = seed_agreement(&chain, provider(1), 20000, 3600);
    seed_agreement(&chain, provider(1), 20001, 3600);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1);
    assert_eq!(agreements[0].key, first, "the first record seen is kept");
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
    let newer = chain.write_as(LB, listing_of(&old).encode(), Expiry::Seconds(2000));
    let older = chain.write_as(LB, listing_of(&old).encode(), Expiry::Seconds(1000));
    let config = marketplace();
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &config, &pool).await.expect("starts");

    assert_eq!(
        agent.listing_key(),
        older,
        "the earliest expiry is the oldest"
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
    agent.poll().await;
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
    agent.poll().await;
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
    agent.poll().await;
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
    agent.poll().await;

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
    agent.poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 1, "a failed read is not an absence");
    assert_eq!(agreements[0].key, expiring, "and adopts nothing either");
    assert_eq!(pool.snapshot().len(), 1);

    chain.heal();
    agent.poll().await;
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

    agent.poll().await;

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
        chain.transactions().len() - writes_before,
        2,
        "the agreement, then its counter record"
    );
    assert!(
        matches!(chain.transactions()[writes_before], Transaction::Create(key) if key == agreement.key)
    );

    // The same offer is not accepted again: an agreement points at it.
    agent.poll().await;
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
    agent.poll().await;
    post(&chain, provider(5), &good, DAY);

    // A head far behind the current one is not judged: the probes are.
    let mut behind = good.clone();
    behind.specs.head = 1;
    post(&chain, provider(6), &behind, DAY);

    let writes_before = chain.transactions().len();
    agent.poll().await;
    let agreements = agent.agreements();
    assert_eq!(agreements.len(), 2, "the seeded one and provider 6");
    assert!(
        agreements.iter().any(|a| a.record.provider == provider(6)),
        "a stale head is no reason to skip"
    );
    assert_eq!(chain.transactions().len() - writes_before, 2);
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
    agent.poll().await;
    chain.advance(100);
    let offer = offer_for(&agent, &chain);
    let second = post(&chain, provider(2), &offer, DAY);
    chain.advance(1);
    let third = post(&chain, provider(3), &offer, DAY);
    agent.poll().await;
    let accepted: Vec<_> = agent.agreements().iter().map(|a| a.record.offer).collect();
    assert_eq!(accepted.len(), 2);
    assert!(
        accepted.contains(&first) && accepted.contains(&second),
        "{accepted:?}"
    );
    assert!(!accepted.contains(&third), "the youngest waits");

    // Nothing changes while the cap is full.
    agent.poll().await;
    assert_eq!(agent.agreements().len(), 2);

    // The first agreement ends (never refreshed: its accept window
    // runs out) while the second is still alive. The oldest waiting
    // offer takes the freed slot, in the same poll.
    chain.advance(config.accept_window.as_secs() / 2 - 100);
    let fourth = post(&chain, provider(4), &offer, DAY);
    agent.poll().await;
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

    agent.poll().await;
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

    agent.poll().await;
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
    agent.poll().await;
    assert!(agent.agreements().is_empty(), "not known yet");
    assert!(pool.snapshot().is_empty());
    assert_eq!(chain.transactions().len() - writes_before, 1, "it landed");

    // The next poll adopts what landed and accepts nothing twice.
    agent.poll().await;
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
async fn an_agreement_stands_when_its_counter_record_does_not_follow() {
    let chain = FakeChain::new(LB, CHAIN_ID);
    let pool = Arc::new(Pool::new(&[]).expect("empty pool"));
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    let offer = offer_for(&agent, &chain);
    post(&chain, provider(1), &offer, DAY);

    chain.fail_sidecar_after(1, "connection refused");
    agent.poll().await;
    assert_eq!(agent.agreements().len(), 1, "the agreement landed");
    assert_eq!(pool.snapshot().len(), 1);
    assert!(
        counter_records(&chain).await.is_empty(),
        "the counter record did not"
    );

    chain.heal();
    agent.poll().await;
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
    agent.poll().await;
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());

    chain.heal();
    agent.poll().await;
    assert_eq!(agent.agreements().len(), 1);
    assert_eq!(counter_records(&chain).await.len(), 1);
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
    assert_eq!(nodes[1]["id"], format!("{:#x}", provider(1)));
    assert_eq!(nodes[1]["url"], "http://127.0.0.1:20007/");
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
