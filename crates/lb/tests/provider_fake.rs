//! The fake provider's own tests: read through the real reader, it
//! answers as the chain it serves, and lies exactly as told.

use std::{sync::atomic::Ordering, time::Duration};

use alloy_primitives::Address;
use lb::chain::{
    ChainReader,
    reader::{BlockAt, Query, Reader},
    records::{Agreement, Record, Wei},
    writer::Expiry,
};

mod common;
use common::{
    fake_chain::{FINALITY_LAG, FakeChain},
    fake_provider::{Rpc, rpc_provider_on},
};

const CHAIN_ID: u64 = 1337;

async fn provider_and_chain() -> (Reader, std::sync::Arc<Rpc>, FakeChain) {
    let chain = FakeChain::new(Address::ZERO, CHAIN_ID);
    chain.advance(200);
    let (addr, rpc) = rpc_provider_on(&chain).await;
    rpc.height.store(chain.head(), Ordering::Relaxed);
    let reader = Reader::new(
        reqwest::Client::new(),
        format!("http://{addr}/").parse().expect("url"),
        None,
        Duration::from_secs(2),
    );
    (reader, rpc, chain)
}

fn an_agreement() -> lb::chain::records::EncodedRecord {
    Agreement {
        provider: Address::ZERO,
        offer: alloy_primitives::B256::ZERO,
        wei_per_call: Wei::new(5),
        remote_port: 20001,
    }
    .encode()
}

#[tokio::test]
async fn an_honest_provider_holds_the_chain_s_block_and_entity() {
    let (reader, _rpc, chain) = provider_and_chain().await;
    let finalized = chain.head() - FINALITY_LAG;
    let key = chain.write_as(Address::ZERO, an_agreement(), Expiry::Seconds(3600));

    let from_provider = reader
        .block(BlockAt::Number(finalized))
        .await
        .expect("read");
    let from_chain = chain.block(BlockAt::Number(finalized)).await.expect("read");
    assert_eq!(from_provider, from_chain);

    let page = reader.query(&Query::by_key(key)).await.expect("page");
    let entity = chain.entity(key).expect("stored").as_arkiv_entity();
    assert_eq!(page.entities.len(), 1);
    assert_eq!(page.entities[0].key, key);
    assert_eq!(page.entities[0].payload, entity.payload);
    assert_eq!(page.block, chain.head(), "answered at its own height");
}

#[tokio::test]
async fn a_lying_provider_serves_another_hash_and_another_payload() {
    let (reader, rpc, chain) = provider_and_chain().await;
    let key = chain.write_as(Address::ZERO, an_agreement(), Expiry::Seconds(3600));
    let truth_block = chain
        .block(BlockAt::Number(10))
        .await
        .expect("read")
        .expect("held");
    let truth = chain.entity(key).expect("stored").as_arkiv_entity();

    rpc.lie_block.store(true, Ordering::Relaxed);
    rpc.lie_entity.store(true, Ordering::Relaxed);

    let block = reader
        .block(BlockAt::Number(10))
        .await
        .expect("read")
        .expect("held");
    assert_ne!(block.hash, truth_block.hash);
    assert_eq!(
        block.state_root, truth_block.state_root,
        "only the hash lies"
    );
    let page = reader.query(&Query::by_key(key)).await.expect("page");
    assert_ne!(page.entities[0].payload, truth.payload);
}

#[tokio::test]
async fn a_provider_behind_the_chain_has_no_block_past_its_height() {
    let (reader, rpc, chain) = provider_and_chain().await;
    rpc.height.store(chain.head() - 100, Ordering::Relaxed);
    let missing = reader
        .block(BlockAt::Number(chain.head() - 50))
        .await
        .expect("read");
    assert_eq!(missing, None);
    let held = reader
        .block(BlockAt::Number(chain.head() - 150))
        .await
        .expect("read");
    assert!(held.is_some());
}

#[tokio::test]
async fn a_delayed_provider_times_a_block_read_out_and_answers_a_probe_at_once() {
    let (reader, rpc, _chain) = provider_and_chain().await;
    rpc.delay_ms.store(3_000, Ordering::Relaxed);
    let error = reader
        .block(BlockAt::Number(1))
        .await
        .expect_err("timed out");
    assert!(matches!(error, lb::chain::reader::ReadError::Transport(_)));
    let height = reader.block_number().await.expect("a probe is not delayed");
    assert_eq!(height, rpc.height.load(Ordering::Relaxed));
}
