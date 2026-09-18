//! The admission decision: who the tunnel lets in, and the words the
//! others read in their client log.

mod common;

use alloy_primitives::B256;
use common::signer::Signer;
use lb::{
    chain::records::{Agreement, Stored, Wei},
    marketplace::admission::{Op, Rejection, Token, decide_admission, token_message},
};

const AGREEMENT: B256 = B256::repeat_byte(0xa1);
const OTHER: B256 = B256::repeat_byte(0xa2);

/// One live agreement, the provider's the signer's address.
fn agreement(provider: &Signer, port: u16) -> Stored<Agreement> {
    Stored {
        key: AGREEMENT,
        creator: alloy_primitives::Address::repeat_byte(0x11),
        expires_at: 1000,
        record: Agreement {
            provider: provider.address(),
            offer: B256::repeat_byte(0x0f),
            wei_per_call: Wei::new(1),
            remote_port: port,
        },
    }
}

/// The token as the client carries it: the id and the signature over it.
fn token_of(signer: &Signer, agreement: B256) -> Token {
    Token::parse(
        Some(&format!("{agreement:#x}")),
        Some(&signer.token(&token_message(agreement))),
    )
    .expect("well formed")
}

#[test]
fn the_message_is_the_prefixed_lowercase_id() {
    assert_eq!(
        token_message(AGREEMENT),
        format!("arkiv-rpc:0x{}", "a1".repeat(32))
    );
}

#[test]
fn a_parsed_token_names_its_agreement() {
    let provider = Signer::new(1);
    assert_eq!(token_of(&provider, AGREEMENT).agreement, AGREEMENT);
}

#[test]
fn the_providers_signature_is_admitted_at_login_and_at_its_port() {
    let provider = Signer::new(1);
    let token = token_of(&provider, AGREEMENT);
    let stored = agreement(&provider, 20003);

    decide_admission(Op::Login, &token, Some(&stored)).expect("admitted");
    let op = Op::NewProxy { remote_port: 20003 };
    decide_admission(op, &token, Some(&stored)).expect("admitted at its port");
    // frpc re-registers a proxy every ~33 s; the answer is the same.
    decide_admission(op, &token, Some(&stored)).expect("admitted again");
}

#[test]
fn the_id_meta_may_be_uppercase_since_the_message_is_lowercase_anyway() {
    let provider = Signer::new(1);
    let uppercase = format!("0x{}", "A1".repeat(32));
    let token = Token::parse(
        Some(&uppercase),
        Some(&provider.token(&token_message(AGREEMENT))),
    )
    .expect("well formed");
    assert_eq!(token.agreement, AGREEMENT);
    decide_admission(Op::Login, &token, Some(&agreement(&provider, 20003))).expect("admitted");
}

#[test]
fn a_signature_that_parses_but_cannot_be_recovered_is_not_a_signature() {
    let provider = Signer::new(1);
    // Well formed, 65 bytes with a valid parity byte, but r = s = 0.
    let degenerate = format!("0x{}1b", "00".repeat(64));
    let token = Token::parse(Some(&format!("{AGREEMENT:#x}")), Some(&degenerate))
        .expect("the shape is right");
    let error = decide_admission(Op::Login, &token, Some(&agreement(&provider, 20003)))
        .expect_err("rejected");
    assert_eq!(error, Rejection::TokenNotASignature);
}

#[test]
fn another_key_over_the_same_id_is_an_impostor() {
    let provider = Signer::new(1);
    let impostor = Signer::new(2);
    let error = decide_admission(
        Op::Login,
        &token_of(&impostor, AGREEMENT),
        Some(&agreement(&provider, 20003)),
    )
    .expect_err("rejected");
    assert_eq!(error, Rejection::WrongSigner);
    assert_eq!(
        error.to_string(),
        "signature was not made by the agreement's provider over this agreement id"
    );
}

#[test]
fn the_providers_signature_over_another_id_does_not_carry() {
    let provider = Signer::new(1);
    // The id the client claims, with a signature made over another.
    let token = Token::parse(
        Some(&format!("{AGREEMENT:#x}")),
        Some(&provider.token(&token_message(OTHER))),
    )
    .expect("well formed");
    let error = decide_admission(Op::Login, &token, Some(&agreement(&provider, 20003)))
        .expect_err("rejected");
    assert_eq!(error, Rejection::WrongSigner);
}

#[test]
fn an_unknown_agreement_is_named() {
    let provider = Signer::new(1);
    let error =
        decide_admission(Op::Login, &token_of(&provider, OTHER), None).expect_err("rejected");
    assert_eq!(error, Rejection::NoAgreement(OTHER));
    assert_eq!(
        error.to_string(),
        format!("no agreement 0x{}", "a2".repeat(32))
    );
}

#[test]
fn the_wrong_port_names_both_ports() {
    let provider = Signer::new(1);
    let error = decide_admission(
        Op::NewProxy { remote_port: 20007 },
        &token_of(&provider, AGREEMENT),
        Some(&agreement(&provider, 20003)),
    )
    .expect_err("rejected");
    assert_eq!(
        error,
        Rejection::WrongPort {
            requested: 20007,
            assigned: 20003
        }
    );
    assert_eq!(
        error.to_string(),
        "port 20007 requested, agreement assigns 20003"
    );
}

#[test]
fn the_signature_is_checked_before_the_port() {
    let provider = Signer::new(1);
    let impostor = Signer::new(2);
    // Both wrong: the operator is told about the signature first, since
    // a port fix is pointless until the key is right.
    let error = decide_admission(
        Op::NewProxy { remote_port: 20007 },
        &token_of(&impostor, AGREEMENT),
        Some(&agreement(&provider, 20003)),
    )
    .expect_err("rejected");
    assert_eq!(error, Rejection::WrongSigner);
}

#[test]
fn mangled_metas_are_told_apart_from_a_missing_agreement() {
    let provider = Signer::new(1);
    let id = format!("{AGREEMENT:#x}");
    let token = provider.token(&token_message(AGREEMENT));

    for bad in [None, Some(""), Some("0x01"), Some("not hex")] {
        let error = Token::parse(bad, Some(&token)).expect_err("rejected");
        assert_eq!(error, Rejection::AgreementNotAKey, "{bad:?}");
    }
    assert_eq!(
        Rejection::AgreementNotAKey.to_string(),
        "agreement id is not an entity key"
    );
    // The last one is the id in the token's place: the metas swapped.
    for bad in [None, Some(""), Some("0xdeadbeef"), Some(id.as_str())] {
        let error = Token::parse(Some(&id), bad).expect_err("rejected");
        assert_eq!(error, Rejection::TokenNotASignature, "{bad:?}");
    }
    assert_eq!(
        Rejection::TokenNotASignature.to_string(),
        "token is not a signature"
    );
}

// ---------------------------------------------------------------------------
// The route, as the tunnel server calls it

use std::time::Duration;

use alloy_primitives::Address;
use common::fake_chain::FakeChain;
use lb::chain::{records::Record, writer::Expiry};

const LB: Address = Address::repeat_byte(0x11);

/// A service with the marketplace section and probing off, over the
/// fake chain, holding one agreement with `provider` at `port`.
async fn service_with(provider: &Signer, port: u16) -> (lb::service::Service, B256) {
    let chain = FakeChain::new(LB, 1337);
    let key = chain.write_as(
        LB,
        Agreement {
            provider: provider.address(),
            offer: B256::repeat_byte(0x0f),
            wei_per_call: Wei::new(1),
            remote_port: port,
        }
        .encode(),
        Expiry::Seconds(3600),
    );
    let mut config = lb::config::Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.health.disable_probing = true;
    config.marketplace = Some(lb::config::Marketplace {
        writer_url: "http://127.0.0.1:8560/".to_owned(),
        wei_per_call: Wei::new(1),
        tunnel_server: "203.0.113.10:7000".to_owned(),
        max_providers: 10,
        remote_port_start: 20000,
        discovery_interval: Duration::from_secs(300),
        accept_window: Duration::from_secs(7200),
        refresh_interval: Duration::from_secs(3600),
        agreement_life: Duration::from_secs(3 * 24 * 3600),
        listing_life: Duration::from_secs(30 * 24 * 3600),
        counter_record_life: Duration::from_secs(180 * 24 * 3600),
        offer_max_lifetime: Duration::from_secs(2 * 24 * 3600),
        gas_warn_below: Wei::new(1),
    });
    let service = lb::service::start_with(config, Some((chain.clone(), chain)))
        .await
        .expect("starts");
    (service, key)
}

/// One callback, the way frps posts it: the op and version in the
/// query too, the content as observed against v0.61.1.
async fn callback(
    service: &lb::service::Service,
    op: &str,
    content: serde_json::Value,
) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/admission?op={op}&version=0.1.0",
            service.admin_addr
        ))
        .json(&serde_json::json!({ "version": "0.1.0", "op": op, "content": content }))
        .send()
        .await
        .expect("answers");
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn metas(signer: &Signer, agreement: B256) -> serde_json::Value {
    serde_json::json!({
        "agreement": format!("{agreement:#x}"),
        "token": signer.token(&token_message(agreement)),
    })
}

/// A login body as frps posts it. The route reads the metas and the
/// client address; the rest is what a real body carries (the
/// `privilege_key` is frp's own token digest) and must be ignored.
fn login(signer: &Signer, agreement: B256) -> serde_json::Value {
    serde_json::json!({
        "version": "0.61.1", "os": "linux", "arch": "amd64",
        "privilege_key": "deadbeefdeadbeefdeadbeefdeadbeef", "timestamp": 1789725184,
        "metas": metas(signer, agreement), "client_spec": {}, "pool_count": 1,
        "client_address": "203.0.113.5:53502",
    })
}

fn new_proxy(signer: &Signer, agreement: B256, port: u16) -> serde_json::Value {
    serde_json::json!({
        "user": { "user": "", "metas": metas(signer, agreement), "run_id": "abc" },
        "proxy_name": format!("node-rpc-{port}"), "proxy_type": "tcp", "remote_port": port,
    })
}

#[tokio::test]
async fn the_route_admits_the_providers_login_and_proxy() {
    let provider = Signer::new(1);
    let (service, key) = service_with(&provider, 20003).await;

    let (status, body) = callback(&service, "Login", login(&provider, key)).await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "unchange": true }));
    let (status, body) = callback(&service, "NewProxy", new_proxy(&provider, key, 20003)).await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "unchange": true }));
    service.shutdown().await;
}

#[tokio::test]
async fn an_admitted_proxy_makes_the_next_probe_due_at_once() {
    let provider = Signer::new(1);
    let (service, key) = service_with(&provider, 20003).await;
    let entry = service.pool.snapshot()[0].clone();
    // Probes have been failing since acceptance and backed off.
    let backed_off = std::time::Instant::now() + Duration::from_secs(300);
    *entry.next_probe() = backed_off;

    callback(&service, "Login", login(&provider, key)).await;
    assert_eq!(
        *entry.next_probe(),
        backed_off,
        "a login is not a tunnel yet"
    );
    callback(&service, "NewProxy", new_proxy(&provider, key, 20003)).await;
    assert!(
        *entry.next_probe() <= std::time::Instant::now(),
        "the proxy is up: probe now"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn the_route_rejects_with_the_text_the_operator_reads() {
    let provider = Signer::new(1);
    let impostor = Signer::new(2);
    let (service, key) = service_with(&provider, 20003).await;

    let (status, body) = callback(&service, "Login", login(&impostor, key)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({
            "reject": true,
            "reject_reason": "signature was not made by the agreement's provider over this agreement id",
        })
    );
    let (_, body) = callback(&service, "NewProxy", new_proxy(&provider, key, 20007)).await;
    assert_eq!(
        body["reject_reason"],
        "port 20007 requested, agreement assigns 20003"
    );
    let (_, body) = callback(&service, "Login", login(&provider, OTHER)).await;
    assert_eq!(body["reject_reason"], format!("no agreement {OTHER:#x}"));
    service.shutdown().await;
}

#[tokio::test]
async fn a_client_with_no_metas_at_all_reads_the_id_text() {
    let provider = Signer::new(1);
    let (service, _) = service_with(&provider, 20003).await;
    // frpc without a `metadatas` section sends no `metas` key: the
    // body still parses and the operator reads what is missing.
    let (status, body) = callback(
        &service,
        "Login",
        serde_json::json!({ "version": "0.61.1", "client_address": "203.0.113.5:1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["reject_reason"], "agreement id is not an entity key");
    service.shutdown().await;
}

#[tokio::test]
async fn a_content_that_does_not_fit_its_op_is_a_400() {
    let provider = Signer::new(1);
    let (service, key) = service_with(&provider, 20003).await;
    let mut proxy = new_proxy(&provider, key, 20003);
    proxy["remote_port"] = serde_json::json!(70000);
    let (status, _) = callback(&service, "NewProxy", proxy).await;
    assert_eq!(
        status, 400,
        "not a port; frps refuses the client on a plugin error"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn an_op_the_route_does_not_judge_passes_unchanged() {
    let provider = Signer::new(1);
    let (service, _) = service_with(&provider, 20003).await;
    let (status, body) = callback(&service, "Ping", serde_json::json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "unchange": true }));
    service.shutdown().await;
}

#[tokio::test]
async fn a_body_that_is_not_a_callback_is_a_400() {
    let provider = Signer::new(1);
    let (service, _) = service_with(&provider, 20003).await;
    let status = reqwest::Client::new()
        .post(format!("http://{}/admission", service.admin_addr))
        .body("not json")
        .header("content-type", "application/json")
        .send()
        .await
        .expect("answers")
        .status()
        .as_u16();
    assert_eq!(status, 400);
    service.shutdown().await;
}

#[tokio::test]
async fn without_the_marketplace_there_is_no_route() {
    let mut config = lb::config::Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.health.disable_probing = true;
    let service = lb::service::start_with::<FakeChain, FakeChain>(config, None)
        .await
        .expect("starts");
    let (status, _) = callback(&service, "Login", serde_json::json!({})).await;
    assert_eq!(status, 404);
    service.shutdown().await;
}
