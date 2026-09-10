//! The chain path against a live network and a running sidecar: every
//! route of the writer client and every call of the read client, in
//! one run. Ignored by default — it spends gas and takes under a
//! minute. Run by hand:
//!
//!   ARKIV_RPC_URL=… ARKIV_API_KEY=… cargo test -p lb --test chain_live -- --ignored
//!
//! with the sidecar up (`npm run service` in writer/, same endpoint, a
//! funded key). WRITER_URL overrides the sidecar's default address.

use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256, keccak256};
use lb::chain::{
    reader::{Query, Reader},
    records::{Agreement, KIND_AGREEMENT, Record, Stored, Wei},
    writer::{Batch, Create, Delete, Expiry, Extend, Patch, Writer},
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
#[ignore = "spends gas on a live network: run with --ignored and ARKIV_RPC_URL set"]
async fn records_are_written_read_back_changed_and_expire() {
    let rpc_url = env("ARKIV_RPC_URL").expect("ARKIV_RPC_URL");
    let writer_url = env("WRITER_URL").unwrap_or_else(|| "http://127.0.0.1:8560/".to_owned());
    let client = reqwest::Client::new();
    let reader = Reader::new(
        client.clone(),
        rpc_url.parse().expect("ARKIV_RPC_URL parses"),
        env("ARKIV_API_KEY"),
        Duration::from_secs(10),
    );
    let writer = Writer::new(client, writer_url.parse().expect("WRITER_URL parses"));

    // A provider address nobody else uses, so the read-back query finds
    // this run's records and no others.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_le_bytes();
    let provider = Address::from_word(keccak256(nonce));
    let agreement = Agreement {
        provider,
        wei_per_call: Wei::new(1_000_000_000_000_000),
        remote_port: 20000,
    };
    let query = Query::kind(KIND_AGREEMENT).attr_addr("provider", provider);

    // Two records: one lives through the run and expires at the end,
    // the other is deleted. Lifetimes are whole blocks: at the 2 s block
    // time, even seconds. The first one's is short so the wait at the
    // end is; the writes before the extend must land within it.
    let kept = writer
        .create(&Create::new(agreement.encode(), Expiry::Seconds(16)))
        .await
        .expect("create lands");
    let doomed = writer
        .create(&Create::new(agreement.encode(), Expiry::Seconds(60)))
        .await
        .expect("second create lands");
    println!(
        "created {} expiring at {}, and {} to delete",
        kept.entity_key, kept.expires_at, doomed.entity_key
    );

    // A query can trail the receipt by a moment on a fast chain.
    let stored = poll(Duration::from_secs(20), || async {
        let page = reader.query(&query).await.expect("query");
        (page.entities.len() == 2).then(|| {
            let kept_row = page
                .entities
                .iter()
                .find(|entity| entity.key == kept.entity_key)
                .expect("the kept record is on the page");
            Stored::<Agreement>::decode(kept_row)
        })
    })
    .await
    .expect("both records are readable")
    .expect("the record decodes");
    assert_eq!(stored.expires_at, kept.expires_at);
    assert_eq!(stored.record, agreement);
    assert_eq!(reader.count(&query).await.expect("count"), 2);
    let balance = reader.balance(stored.creator).await.expect("balance");
    assert!(balance > U256::ZERO, "the writer's key is funded");
    println!(
        "read back, creator {:#x} holding {balance} wei",
        stored.creator
    );

    // The refresh path: an extend in a batch, before the short lifetime
    // runs out.
    let batch = Batch {
        extensions: vec![Extend {
            entity_key: kept.entity_key,
            expires: Expiry::Seconds(30),
        }],
    };
    let extended = writer.execute_batch(&batch).await.expect("batch lands");
    assert_eq!(extended.extended_entities, [kept.entity_key]);
    println!("extended in transaction {}", extended.tx_hash);

    let changed = Agreement {
        remote_port: 20001,
        ..agreement.clone()
    };
    let patched = writer
        .patch(&Patch {
            entity_key: kept.entity_key,
            set: None,
            payload: Some(changed.encode().payload),
        })
        .await
        .expect("patch lands");
    assert_eq!(patched.entity_key, kept.entity_key);
    let deleted = writer
        .delete(&Delete {
            entity_key: doomed.entity_key,
        })
        .await
        .expect("delete lands");
    assert_eq!(deleted.entity_key, doomed.entity_key);

    // Both changes show at once: the new payload on the kept record,
    // the deleted one gone from the page and the count.
    let after = poll(Duration::from_secs(20), || async {
        let page = reader.query(&query).await.expect("query");
        (page.entities.len() == 1).then(|| Stored::<Agreement>::decode(&page.entities[0]))
    })
    .await
    .expect("one record remains")
    .expect("the record decodes");
    assert_eq!(after.key, kept.entity_key);
    assert_eq!(after.record, changed);
    assert!(after.expires_at > kept.expires_at, "the expiry moved out");
    assert_eq!(reader.count(&query).await.expect("count"), 1);
    println!(
        "patched, and the other deleted; expiring at {}",
        after.expires_at
    );

    let gone = poll(Duration::from_secs(120), || async {
        let head = reader.block_number().await.expect("head");
        if head <= after.expires_at {
            return None;
        }
        let page = reader.query(&query).await.expect("query");
        page.entities.is_empty().then_some(head)
    })
    .await
    .expect("the record expired and left the query results");
    println!("gone at head {gone}");
}

async fn poll<T, F, Fut>(within: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let started = Instant::now();
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if started.elapsed() > within {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
