//! The fake chain keeps the promises the live smoke checks against a
//! real node: the same steps as `chain_live.rs`, with the head moved by
//! hand instead of waited for, plus what only a fake can show — the
//! failure switch, the atomic batch, the page limit.

mod common;

use alloy_primitives::{Address, U256};
use common::fake_chain::{BatchLog, FakeChain, Transaction};
use lb::chain::{
    ChainReader, ChainWriter,
    reader::{PAGE_LIMIT, Page, Query, ReadError},
    records::{
        Agreement, AttributeValue, Attributes, KIND_AGREEMENT, KIND_OFFER, Record, Stored, Wei,
    },
    writer::{Batch, Create, Delete, Expiry, Extend, Group, Operation, Patch, WriteError, send},
};

const LB: Address = Address::repeat_byte(0x11);
const PROVIDER: Address = Address::repeat_byte(0x22);

fn agreement(port: u16) -> Agreement {
    Agreement {
        provider: PROVIDER,
        offer: alloy_primitives::B256::repeat_byte(0x0f),
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
        .execute_batch(&Batch::single(Operation::Extend(Extend {
            entity_key: kept.entity_key,
            expires: Expiry::Seconds(30),
        })))
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
            Transaction::Batch(BatchLog {
                extended: vec![kept.entity_key],
                ..Default::default()
            }),
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
async fn the_sidecar_and_the_reference_can_be_taken_down() {
    let chain = FakeChain::new(LB, 1337);
    chain.fail_reference("connection refused");
    let error = chain
        .count(&Query::kind(KIND_AGREEMENT))
        .await
        .expect_err("reads fail");
    assert!(matches!(&error, ReadError::Rpc { message, .. } if message.contains("refused")));
    chain.heal();

    chain.fail_sidecar("gas required exceeds allowance");
    assert!(
        chain.identity().await.is_err(),
        "the identity is the sidecar's too"
    );
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
    let mut batch = Batch::new();
    batch.push(Group::single(Operation::Extend(Extend {
        entity_key: one.entity_key,
        expires: Expiry::Seconds(100),
    })));
    batch.push(Group::single(Operation::Extend(Extend {
        entity_key: missing,
        expires: Expiry::Seconds(100),
    })));
    let error = chain
        .execute_batch(&batch)
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

#[tokio::test]
async fn a_batch_lands_every_kind_of_operation_at_once() {
    let chain = FakeChain::new(LB, 1337);
    let existing = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect("create lands");
    let doomed = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect("create lands");

    // An acceptance-shaped group, a close-and-open-shaped group, and
    // the refresh's kind, in one batch.
    let mut batch = Batch::new();
    batch.push(Group::new(vec![
        Operation::Create(Create::new(agreement(20001).encode(), Expiry::Seconds(100))),
        Operation::Create(Create::new(agreement(20002).encode(), Expiry::Seconds(100))),
    ]));
    batch.push(Group::new(vec![
        // A close-shaped patch: the payload and an attribute together.
        Operation::Patch(Patch {
            entity_key: existing.entity_key,
            set: Some(Attributes::default().with("state", AttributeValue::Str("closed".into()))),
            payload: Some(agreement(20009).encode().payload),
        }),
        Operation::Delete(Delete {
            entity_key: doomed.entity_key,
        }),
    ]));
    batch.push(Group::single(Operation::Extend(Extend {
        entity_key: existing.entity_key,
        expires: Expiry::Seconds(100),
    })));
    let result = chain.execute_batch(&batch).await.expect("lands");
    assert_eq!(result.created_entities.len(), 2);
    assert_eq!(result.patched_entities, [existing.entity_key]);
    assert_eq!(result.deleted_entities, [doomed.entity_key]);
    assert_eq!(result.extended_entities, [existing.entity_key]);

    let patched = chain.entity(existing.entity_key).expect("stored");
    assert_eq!(patched.expires_at, 1 + 50, "extended in the same batch");
    assert_eq!(
        Stored::<Agreement>::decode(&patched.as_arkiv_entity())
            .expect("decodes")
            .record
            .remote_port,
        20009
    );
    assert_eq!(
        patched.attributes.get("state"),
        Some(&AttributeValue::Str("closed".into())),
        "the attribute set in the same patch"
    );
    assert!(chain.entity(doomed.entity_key).is_none());
    assert_eq!(
        chain
            .count(&Query::kind(KIND_AGREEMENT))
            .await
            .expect("count"),
        3
    );
}

#[tokio::test]
async fn a_batch_over_the_limit_is_refused_and_send_splits_it() {
    let chain = FakeChain::new(LB, 1337);
    // Room for two operations per transaction.
    chain.set_operation_limit(2);
    let mut batch = Batch::new();
    for port in 20000..20005 {
        batch.push(Group::single(Operation::Create(Create::new(
            agreement(port).encode(),
            Expiry::Seconds(100),
        ))));
    }
    let error = chain
        .execute_batch(&batch)
        .await
        .expect_err("five creates are over the limit");
    assert!(matches!(error, WriteError::TooLarge(_)), "{error}");
    assert!(error.to_string().contains("oversized data"), "{error}");
    assert!(chain.transactions().is_empty(), "nothing was sent");

    let sent = send(&chain, batch).await;
    assert!(sent.iter().all(|s| s.result.is_ok()), "{sent:?}");
    assert_eq!(
        sent.iter().map(|s| s.batch.groups().len()).sum::<usize>(),
        5,
        "every group landed"
    );
    assert!(
        sent.len() >= 3,
        "split until each transaction fit: {}",
        sent.len()
    );
    assert_eq!(
        chain
            .count(&Query::kind(KIND_AGREEMENT))
            .await
            .expect("count"),
        5
    );
    // In order: the first transaction holds the first ports.
    let first = sent[0].result.as_ref().expect("ok");
    let stored = chain.entity(first.created_entities[0]).expect("stored");
    assert_eq!(
        Stored::<Agreement>::decode(&stored.as_arkiv_entity())
            .expect("decodes")
            .record
            .remote_port,
        20000
    );
}

#[tokio::test]
async fn send_reports_a_group_that_is_too_large_on_its_own() {
    let chain = FakeChain::new(LB, 1337);
    chain.set_operation_limit(1);
    // An acceptance-shaped group: two creates that must land together.
    let mut batch = Batch::new();
    batch.push(Group::new(vec![
        Operation::Create(Create::new(agreement(20000).encode(), Expiry::Seconds(100))),
        Operation::Create(Create::new(agreement(20001).encode(), Expiry::Seconds(100))),
    ]));
    let sent = send(&chain, batch).await;
    assert_eq!(sent.len(), 1);
    assert!(matches!(sent[0].result, Err(WriteError::TooLarge(_))));
    assert!(chain.transactions().is_empty());
}

#[tokio::test]
async fn send_sends_nothing_for_an_empty_batch() {
    let chain = FakeChain::new(LB, 1337);
    assert!(send(&chain, Batch::new()).await.is_empty());
    assert!(chain.transactions().is_empty());
}

#[tokio::test]
async fn a_part_that_fails_for_another_reason_does_not_stop_the_rest() {
    let chain = FakeChain::new(LB, 1337);
    let existing = chain
        .create(&Create::new(agreement(20000).encode(), Expiry::Seconds(10)))
        .await
        .expect("create lands");
    // One operation per transaction, so the two groups are sent apart.
    chain.set_operation_limit(1);
    let mut batch = Batch::new();
    batch.push(Group::single(Operation::Extend(Extend {
        entity_key: alloy_primitives::B256::repeat_byte(0xff),
        expires: Expiry::Seconds(100),
    })));
    batch.push(Group::single(Operation::Extend(Extend {
        entity_key: existing.entity_key,
        expires: Expiry::Seconds(100),
    })));

    let sent = send(&chain, batch).await;
    assert_eq!(sent.len(), 2);
    assert!(
        matches!(sent[0].result, Err(WriteError::Failed(_))),
        "the missing key fails its own part: {:?}",
        sent[0].result
    );
    assert!(
        sent[1].result.is_ok(),
        "the other part lands: {:?}",
        sent[1].result
    );
    assert_eq!(
        chain
            .entity(existing.entity_key)
            .expect("stored")
            .expires_at,
        1 + 50,
        "extended despite the failure before it"
    );
}
