//! The marketplace side of the LB: the agent that turns offers into
//! providers and keeps their agreements alive. Everything here goes to
//! the chain through the two chain traits, so the tests run over a fake.

pub mod admission;
pub mod agent;
