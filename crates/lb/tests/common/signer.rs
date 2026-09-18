//! A provider's key, for tests that need a real signature: the token
//! is signed the way the provider tooling signs it, EIP-191 over the
//! agreement id, so the LB's recovery is exercised for real.

use alloy_primitives::{Address, B256, Signature, eip191_hash_message};
use k256::ecdsa::SigningKey;

pub struct Signer {
    key: SigningKey,
}

impl Signer {
    /// A deterministic key from one byte, like `provider(n)` in the
    /// agent tests; `n` must not be zero.
    pub fn new(n: u8) -> Self {
        let key = SigningKey::from_bytes(&B256::repeat_byte(n).0.into()).expect("a valid scalar");
        Self { key }
    }

    pub fn address(&self) -> Address {
        Address::from_public_key(self.key.verifying_key())
    }

    /// The token: the signature over the message, as the hex string the
    /// tunnel client carries.
    pub fn token(&self, message: &str) -> String {
        let hash = eip191_hash_message(message);
        let (signature, recovery) = self
            .key
            .sign_prehash_recoverable(hash.as_slice())
            .expect("signs");
        let signature = Signature::from_signature_and_parity(signature, recovery.is_y_odd());
        format!("0x{}", alloy_primitives::hex::encode(signature.as_bytes()))
    }
}
