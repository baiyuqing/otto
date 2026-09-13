//! Provider-neutral core of Otto.
//!
//! This crate must build for `wasm32-unknown-unknown`. It therefore uses no
//! filesystem, network, process, or wall-clock API: callers inject the clock
//! and the identifier generator. The `wasm32` build is the architecture guard
//! for that rule, the role `internal/architecture/imports_test.go` plays for
//! the Go packages.

pub mod agent;
pub mod model;
pub mod openaicompat;
pub mod provider;
pub mod session;
pub mod tool;
