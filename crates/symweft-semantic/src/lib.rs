//! Compiler-backed C/C++ facts, separate from A1 syntax and its cache schema.
pub mod adaptive;
pub mod compdb;
pub mod fingerprint;
pub mod heat;
pub mod hot_pool;
pub mod ledger;
pub mod model;
pub mod preamble;
pub mod prefetch;
pub mod profile;
pub mod provider;
pub mod query_index;
pub mod service;
pub mod store;
pub mod transport;
pub mod watch;
pub use symweft_index::Result;
pub const PROVIDER_KEY: &str = "a2-clang-v3";
