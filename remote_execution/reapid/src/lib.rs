//! `reapid`: a REAPI v2 server with a directory blob store.
//!
//! Actions execute as subprocesses on this host; results and blobs live in
//! a plain-file store (ContentAddressableStorage, ActionCache,
//! Capabilities, Execution, ByteStream) with TOML-configured eviction
//! policy. See `README.md`.

pub mod config;
pub mod digest;
pub mod execute;
pub mod mux;
pub mod policy;
pub mod prune;
pub mod services;
pub mod store;
