//! The tunnel's admission check, behind frps's `Login` and `NewProxy`
//! callbacks: a provider's client is let in when the signature it
//! carries was made by the agreement's provider over the agreement id,
//! and, at `NewProxy`, when the port it asks for is the one the
//! agreement assigns. The decision is a pure function; the route that
//! speaks frps's wire format is in `admin.rs`.

use std::fmt;

use alloy_primitives::Signature;

use crate::chain::records::{Agreement, EntityKey, Stored};

/// What frps is asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Login,
    NewProxy { remote_port: u16 },
}

/// Why a client is turned away. The text is what its operator reads in
/// the tunnel client's log, so each one names the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    AgreementNotAKey,
    TokenNotASignature,
    NoAgreement(EntityKey),
    /// Recovery gives some address for any well-formed signature, so a
    /// signature by another key and one by the right key over another
    /// id look the same: an address that is not the provider's.
    WrongSigner,
    WrongPort {
        requested: u16,
        assigned: u16,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgreementNotAKey => write!(f, "agreement id is not an entity key"),
            Self::TokenNotASignature => write!(f, "token is not a signature"),
            Self::NoAgreement(key) => write!(f, "no agreement {key:#x}"),
            Self::WrongSigner => write!(
                f,
                "signature was not made by the agreement's provider over this agreement id"
            ),
            Self::WrongPort {
                requested,
                assigned,
            } => write!(
                f,
                "port {requested} requested, agreement assigns {assigned}"
            ),
        }
    }
}

/// Where the route finds this LB's live agreements: the agent, and in
/// tests whatever stands in for it.
pub trait Agreements: Send + Sync {
    fn agreement(&self, key: EntityKey) -> Option<Stored<Agreement>>;
}

/// The message a provider signs for its token: the agreement id under
/// a prefix that keeps the signature useless for anything else.
pub fn token_message(agreement: EntityKey) -> String {
    format!("arkiv-rpc:{agreement:#x}")
}

/// What the client carries: the agreement it claims and the signature
/// over that claim. Parsed first, so a mangled configuration reads as
/// such and not as "no agreement".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub agreement: EntityKey,
    signature: Signature,
}

impl Token {
    pub fn parse(agreement: Option<&str>, token: Option<&str>) -> Result<Self, Rejection> {
        let agreement = agreement
            .and_then(|text| text.parse().ok())
            .ok_or(Rejection::AgreementNotAKey)?;
        let signature = token
            .and_then(|text| text.parse().ok())
            .ok_or(Rejection::TokenNotASignature)?;
        Ok(Self {
            agreement,
            signature,
        })
    }
}

/// Whether to admit the client, given its token and this LB's live
/// agreement under the token's key, if there is one.
pub fn decide_admission(
    op: Op,
    token: &Token,
    agreement: Option<&Stored<Agreement>>,
) -> Result<(), Rejection> {
    let stored = agreement.ok_or(Rejection::NoAgreement(token.agreement))?;
    // EIP-191 recovery: the signer's address comes out of the signature
    // and the message, and only the provider's key produces its address.
    // Recovery refuses only a degenerate signature that parsed anyway.
    let signer = token
        .signature
        .recover_address_from_msg(token_message(token.agreement))
        .map_err(|_| Rejection::TokenNotASignature)?;
    if signer != stored.record.provider {
        return Err(Rejection::WrongSigner);
    }
    if let Op::NewProxy { remote_port } = op
        && remote_port != stored.record.remote_port
    {
        return Err(Rejection::WrongPort {
            requested: remote_port,
            assigned: stored.record.remote_port,
        });
    }
    Ok(())
}
