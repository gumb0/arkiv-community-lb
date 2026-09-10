//! The chain path against a live network and a running sidecar: write a
//! record, read it back, extend it, let it expire. Ignored by default —
//! it spends gas and takes about half a minute. Run by hand:
//!
//!   ARKIV_RPC_URL=… ARKIV_API_KEY=… cargo test -p lb --test chain_live -- --ignored
//!
//! with the sidecar up (`npm run service` in writer/, same endpoint, a
//! funded key). WRITER_URL overrides the sidecar's default address.

use std::time::{Duration, Instant};

use alloy_primitives::{Address, keccak256};
use lb::chain::{
    reader::{Query, Reader},
    records::{Agreement, KIND_AGREEMENT, Record, Stored, Wei},
    writer::{Create, Expiry, Extend, Writer},
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
#[ignore = "spends gas on a live network: run with --ignored and ARKIV_RPC_URL set"]
async fn a_record_is_written_read_back_extended_and_expires() {
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
    // this run's record and no other.
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

    // Lifetimes are whole blocks: at the 2 s block time, even seconds.
    let created = writer
        .create(&Create::new(agreement.encode(), Expiry::Seconds(16)))
        .await
        .expect("create lands");
    println!(
        "created {} expiring at {}",
        created.entity_key, created.expires_at
    );

    // A query can trail the receipt by a moment on a fast chain.
    let query = Query::kind(KIND_AGREEMENT).attr_addr("provider", provider);
    let stored = poll(Duration::from_secs(20), || async {
        let page = reader.query(&query).await.expect("query");
        (page.entities.len() == 1).then(|| Stored::<Agreement>::decode(&page.entities[0]))
    })
    .await
    .expect("the record is readable")
    .expect("the record decodes");
    assert_eq!(stored.key, created.entity_key);
    assert_eq!(stored.expires_at, created.expires_at);
    assert_eq!(stored.record, agreement);
    println!("read back, creator {:#x}", stored.creator);

    let extended = writer
        .extend(&Extend {
            entity_key: created.entity_key,
            expires: Expiry::Seconds(20),
        })
        .await
        .expect("extend lands");
    assert!(
        extended.expires_at > created.expires_at,
        "the expiry moved out"
    );
    println!("extended to {}", extended.expires_at);

    let gone = poll(Duration::from_secs(120), || async {
        let head = reader.block_number().await.expect("head");
        if head <= extended.expires_at {
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
