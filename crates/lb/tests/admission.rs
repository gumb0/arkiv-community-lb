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
