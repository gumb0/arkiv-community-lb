//! Everything that touches the Arkiv chain: the records the LB writes and
//! reads, the client for the chain-writer sidecar, and the read
//! client. Nothing else in the crate speaks to the chain.

pub mod reader;
pub mod records;
pub mod writer;

/// A transport error with its causes: reqwest's own message stops at
/// "error sending request", and the reason, a timeout or a refused
/// connection, is in the errors under it.
pub(crate) fn transport_causes(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

pub use reader::ChainReader;
pub use writer::ChainWriter;
