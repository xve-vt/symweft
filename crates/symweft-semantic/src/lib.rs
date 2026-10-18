//! Compiler-backed C/C++ facts, separate from A1 syntax and its cache schema.
pub mod compdb;
pub mod fingerprint;
pub mod model;
pub mod profile;
pub mod provider;
pub mod service;
pub mod store;
pub mod watch;
pub use symweft_index::Result;
pub const PROVIDER_KEY: &str = "a2-clang-v3";
