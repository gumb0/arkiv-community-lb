//! The fake chain keeps the promises the live smoke checks against a
//! real node: the same steps as `chain_live.rs`, with the head moved by
//! hand instead of waited for, plus what only a fake can show — the
//! failure switch, the atomic batch, the page limit.

mod common;

use alloy_primitives::{Address, U256};
use common::fake_chain::{FakeChain, Transaction};
use lb::chain::{
    ChainReader, ChainWriter,
    reader::{PAGE_LIMIT, Page, Query},
    records::{Agreement, AttributeValue, KIND_AGREEMENT, KIND_OFFER, Record, Stored, Wei},
    writer::{Batch, Create, Delete, Expiry, Extend, Patch, WriteError},
};

const LB: Address = Address::repeat_byte(0x11);
const PROVIDER: Address = Address::repeat_byte(0x22);

fn agreement(port: u16) -> Agreement {
    Agreement {
        provider: PROVIDER,
        wei_per_call: Wei::new(1_000_000_000_000_000),
        remote_port: port,
    }
}

#[tokio::test]
async fn records_are_written_read_back_changed_and_expire() {
    let chain = FakeChain::new(LB, 1337);
    let query = Query::kind(KIND_AGREEMENT).attr_addr("provider", PROVIDER);

    let kept = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(16)))
        .await
        .expect("create lands");
    let doomed = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(60)))
        .await
        .expect("second create lands");
    assert_eq!(kept.expires_at, 1 + 8, "16 s is 8 blocks from head 1");
    assert_ne!(kept.entity_key, doomed.entity_key);

    let page = chain.query(&query).await.expect("query");
    assert_eq!(page.entities.len(), 2);
    assert!(!page.more);
    let stored = Stored::<Agreement>::decode(&page.entities[0]).expect("decodes");
    assert_eq!(stored.key, kept.entity_key);
    assert_eq!(stored.expires_at, kept.expires_at);
    assert_eq!(stored.record, agreement(20000));
    assert_eq!(chain.count(&query).await.expect("count"), 2);
    let identity = chain.identity().await.expect("identity");
    assert_eq!(identity.address, stored.creator);
    assert_eq!(identity.chain_id, 1337);
    let balance = chain.balance(stored.creator).await.expect("balance");
    assert!(balance > U256::ZERO);

    let extended = chain
        .execute_batch(&Batch {
            extensions: vec![Extend {
                entity_key: kept.entity_key,
                expires: Expiry::Seconds(30),
            }],
        })
        .await
        .expect("batch lands");
    assert_eq!(extended.extended_entities, [kept.entity_key]);

    let changed = agreement(20001);
    chain
        .patch(&Patch {
            entity_key: kept.entity_key,
            set: None,
            payload: Some(changed.encode().payload),
        })
        .await
        .expect("patch lands");
    chain
        .delete(&Delete {
            entity_key: doomed.entity_key,
        })
        .await
        .expect("delete lands");

    let page = chain.query(&query).await.expect("query");
    assert_eq!(page.entities.len(), 1);
    let after = Stored::<Agreement>::decode(&page.entities[0]).expect("decodes");
    assert_eq!(after.key, kept.entity_key);
    assert_eq!(after.record, changed);
    assert_eq!(after.expires_at, 1 + 15, "extended to 30 s from the head");
    assert_eq!(chain.count(&query).await.expect("count"), 1);

    chain.advance(15);
    assert_eq!(chain.block_number().await.expect("head"), 16);
    assert!(
        chain
            .query(&query)
            .await
            .expect("query")
            .entities
            .is_empty(),
        "expired at the head, so gone from reads"
    );
    assert!(chain.entity(kept.entity_key).is_some(), "still stored");
    assert!(chain.entity(doomed.entity_key).is_none(), "deleted");

    assert_eq!(
        chain.transactions(),
        [
            Transaction::Create(kept.entity_key),
            Transaction::Create(doomed.entity_key),
            Transaction::Extend(vec![kept.entity_key]),
            Transaction::Patch(kept.entity_key),
            Transaction::Delete(doomed.entity_key),
        ]
    );
}

#[tokio::test]
async fn a_query_filters_by_creator_attributes_and_expiry() {
    let chain = FakeChain::new(LB, 1337);
    let mine = chain
        .create(&Create::new(
            agreement(20000).encode(),
            Expiry::Seconds(100),
        ))
        .await
        .expect("create lands");
    let other = Address::repeat_byte(0x33);
    let theirs = chain.write_as(other, agreement(20000).encode(), Expiry::Seconds(1000));

    let by_creator = |creator| Query::kind(KIND_AGREEMENT).creator(creator);
    let keys = |page: Page| page.entities.into_iter().map(|e| e.key).collect::<Vec<_>>();
    assert_eq!(
        keys(chain.query(&by_creator(LB)).await.expect("query")),
        [mine.entity_key]
    );
    assert_eq!(
        keys(chain.query(&by_creator(other)).await.expect("query")),
        [theirs]
    );
    assert!(
        chain
            .query(&Query::kind(KIND_OFFER))
            .await
            .expect("query")
            .entities
            .is_empty(),
        "another kind"
    );
    assert!(
        chain
            .query(&Query::kind(KIND_AGREEMENT).attr_addr("provider", other))
            .await
            .expect("query")
            .entities
            .is_empty(),
        "an attribute value nobody has"
    );

    // Lifetime bounds, as discovery uses them.
    let head = chain.head();
    assert_eq!(
        keys(
            chain
                .query(&Query::kind(KIND_AGREEMENT).expires_by(head + 50))
                .await
                .expect("query")
        ),
        [mine.entity_key]
    );
    assert_eq!(
        keys(
            chain
                .query(&Query::kind(KIND_AGREEMENT).expires_after(head + 50))
                .await
                .expect("query")
        ),
        [theirs]
    );
}

#[tokio::test]
async fn a_newer_schema_version_is_not_returned() {
    let chain = FakeChain::new(LB, 1337);
    let mut newer = agreement(20000).encode();
    newer.attributes = newer.attributes.with("v", AttributeValue::I32(2));
    chain.write_as(LB, newer, Expiry::Seconds(100));
    chain
        .create(&Create::new(
            agreement(20000).encode(),
            Expiry::Seconds(100),
        ))
        .await
        .expect("create lands");

    let page = chain
        .query(&Query::kind(KIND_AGREEMENT))
        .await
        .expect("query");
    assert_eq!(page.entities.len(), 1, "the query filters on v");
    assert!(Stored::<Agreement>::decode(&page.entities[0]).is_ok());
    assert_eq!(
        chain
            .count(&Query::kind(KIND_AGREEMENT))
            .await
            .expect("count"),
        1
    );
}

#[tokio::test]
async fn writes_fail_while_the_switch_is_on() {
    let chain = FakeChain::new(LB, 1337);
    chain.fail_writes("gas required exceeds allowance");
    let error = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect_err("writes fail");
    assert!(matches!(&error, WriteError::Failed(links) if links[0].message.contains("gas")));
    assert!(chain.transactions().is_empty(), "nothing landed");

    chain.heal();
    chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect("writes land again");
    assert_eq!(chain.transactions().len(), 1);
}

#[tokio::test]
async fn a_batch_is_all_or_nothing() {
    let chain = FakeChain::new(LB, 1337);
    let one = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect("create lands");

    let missing = alloy_primitives::B256::repeat_byte(0xff);
    let error = chain
        .execute_batch(&Batch {
            extensions: vec![
                Extend {
                    entity_key: one.entity_key,
                    expires: Expiry::Seconds(100),
                },
                Extend {
                    entity_key: missing,
                    expires: Expiry::Seconds(100),
                },
            ],
        })
        .await
        .expect_err("a missing key fails the batch");
    assert!(matches!(error, WriteError::Failed(_)));
    assert_eq!(
        chain.entity(one.entity_key).expect("stored").expires_at,
        one.expires_at,
        "nothing in the failed batch moved"
    );
    assert_eq!(chain.transactions().len(), 1, "only the create landed");
}

#[tokio::test]
async fn a_page_holds_the_limit_and_says_there_is_more() {
    let chain = FakeChain::new(LB, 1337);
    for _ in 0..=PAGE_LIMIT {
        chain
            .create(&Create::new(
                agreement(20000).encode(),
                Expiry::Seconds(100),
            ))
            .await
            .expect("create lands");
    }
    let query = Query::kind(KIND_AGREEMENT);
    let page = chain.query(&query).await.expect("query");
    assert_eq!(page.entities.len() as u64, PAGE_LIMIT);
    assert!(page.more);
    assert_eq!(chain.count(&query).await.expect("count"), PAGE_LIMIT + 1);
}
