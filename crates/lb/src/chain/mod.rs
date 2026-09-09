//! Everything that touches the Arkiv chain: the records the LB writes and
//! reads, the client for the chain-writer sidecar, and the trusted read
//! client. Nothing else in the crate speaks to the chain.

pub mod records;
pub mod writer;
