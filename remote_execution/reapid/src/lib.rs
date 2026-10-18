//! `reapid`: a cache-only REAPI v2 server with a directory blob store.
//!
//! Buck2 runs every action locally; this daemon serves only cache traffic
//! (ContentAddressableStorage, ActionCache, Capabilities, ByteStream) from
//! plain files, with TOML-configured eviction policy. See `README.md`.

pub mod config;
pub mod digest;
pub mod mux;
pub mod policy;
pub mod services;
pub mod store;
