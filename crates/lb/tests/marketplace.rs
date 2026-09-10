//! The marketplace agent over the fake chain: what it reads back at
//! startup, what it writes, and when it refuses to start.

mod common;

use std::time::Duration;

use alloy_primitives::Address;
use common::fake_chain::{FakeChain, Transaction};
use lb::{
    chain::{
        ChainReader,
        reader::{PAGE_LIMIT, Query},
        records::{Agreement, KIND_LB_LISTING, LbListing, Record, Stored, Wei},
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
        offer_max_lifetime: Duration::from_secs(2 * 24 * 3600),
        offer_max_lag_blocks: 1000,
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
        wei_per_call: RATE,
        remote_port: port,
    };
    chain.write_as(LB, record.encode(), Expiry::Seconds(life))
}

async fn start(chain: &FakeChain, config: &Marketplace, pool: &Pool) -> Result<Agent, StartError> {
    Agent::start(chain, chain, config, pool).await
}

#[tokio::test]
async fn a_first_start_writes_the_listing_and_reloads_nothing() {
    let chain = FakeChain::new(LB, 1337);
    let config = marketplace();
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
    let agent = start(&chain, &marketplace(), &pool).await.expect("starts");
    assert!(agent.agreements().is_empty());
    assert!(pool.snapshot().is_empty());
}

#[tokio::test]
async fn a_second_record_for_one_provider_is_skipped() {
    let chain = FakeChain::new(LB, 1337);
    let first = seed_agreement(&chain, provider(1), 20000, 3600);
    seed_agreement(&chain, provider(1), 20001, 3600);
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
    let agent = start(&chain, &config, &pool).await.expect("starts");
    assert_eq!(agent.agreements().len(), 3);
    assert_eq!(pool.snapshot().len(), 3);
}

#[tokio::test]
async fn an_unchanged_listing_is_left_alone() {
    let chain = FakeChain::new(LB, 1337);
    let config = marketplace();
    let existing = chain.write_as(LB, listing_of(&config).encode(), Expiry::Seconds(3600));
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
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
        wei_per_call: RATE,
        remote_port: 20000,
    }
    .encode();
    agreement.payload = b"not json".to_vec();
    chain.write_as(LB, agreement, Expiry::Seconds(3600));
    let mut listing = listing_of(&config).encode();
    listing.payload = b"{}".to_vec();
    let broken = chain.write_as(LB, listing, Expiry::Seconds(3600));
    let pool = Pool::new(&[]).expect("empty pool");
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
    let pool = Pool::new(&[]).expect("empty pool");
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
