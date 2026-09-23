//! Provider-neutral core of Kite.
//!
//! This crate must build for `wasm32-unknown-unknown`. It therefore uses no
//! filesystem, network, process, or wall-clock API: callers inject the clock
//! and the identifier generator. `make rust-wasm-check` (`cargo check -p
//! kite-core --target wasm32-unknown-unknown`) is the architecture guard for
//! that rule.

pub mod agent;
pub mod config;
pub mod model;
pub mod openaicompat;
pub mod openairesponses;
pub mod provider;
pub mod safetext;
pub mod session;
pub mod tool;
pub mod wire;
