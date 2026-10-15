//! Provider-neutral core of Otto.
//!
//! This crate must build for `wasm32-unknown-unknown`. It therefore uses no
//! filesystem, network, process, or wall-clock API: callers inject the clock
//! and the identifier generator. `make rust-wasm-check` (`cargo check -p
//! otto-core --target wasm32-unknown-unknown`) is the architecture guard for
//! that rule.

pub mod agent;
pub mod approval;
pub mod config;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod diagnostics;
pub mod mailbox;
pub mod memory;
pub mod model;
pub mod openaicompat;
pub mod openairesponses;
pub mod operation;
pub mod provider;
pub mod safetext;
pub mod session;
pub mod tool;
pub mod verification;
pub mod wire;
