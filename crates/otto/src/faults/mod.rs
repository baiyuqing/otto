//! Test-only fault injection for doubles that need `otto` domain modules.
//!
//! The fault vocabulary, the file, publish, SQL and crash doubles and the
//! scripted HTTP server live in the `otto-faults` crate; this module holds the
//! decorators over `otto` traits ([`mcp`], [`sandbox`]).
//!
//! Compiled only for this crate's unit tests and with the off-by-default
//! `fault-injection` feature, which the crate's integration tests enable
//! through a self dev-dependency. Production code never names this module;
//! `tests/faults_boundary.rs` enforces that. See "Fault injection" in
//! `docs/development/testing.md`.

pub mod mcp;
pub mod sandbox;
