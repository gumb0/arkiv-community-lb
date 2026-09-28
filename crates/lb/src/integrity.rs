//! What an integrity round says about a provider. The checker itself
//! comes with the round; these are the words the pool entry records.

/// One provider's verdict from one round. Only a match lifts an
/// integrity quarantine; only a divergence, once confirmed, sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both reads answered and both agreed with the reference.
    Match,
    /// Behind the chain: no block at the height, or the entity answered
    /// too far back. The lag path's business, not integrity's.
    Stale,
    /// Confirmed different from the reference, in either read; the
    /// evidence event says which.
    Divergence,
    /// Nothing could be judged: the reference or the provider did not
    /// answer, or answered at a block the other side did not have.
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Stale => "stale",
            Self::Divergence => "divergence",
            Self::Unknown => "unknown",
        }
    }
}
