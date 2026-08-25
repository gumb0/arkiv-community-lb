//! The community load balancer, built as a library so integration tests
//! run the real service in-process against fake providers. The binary in
//! `main.rs` stays a thin wrapper.

pub mod admin;
pub mod config;
pub mod forwarder;
pub mod jsonrpc;
pub mod pool;
pub mod proxy;
pub mod service;
