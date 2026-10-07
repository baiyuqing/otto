//! Test-only fault injection: one typed fault vocabulary ([`plan`]) and
//! decorators over the seams production code already accepts.
//!
//! Compiled only for this crate's unit tests and with the off-by-default
//! `fault-injection` feature, which the crate's integration tests enable
//! through a self dev-dependency. Production code never names this module;
//! `tests/faults_boundary.rs` enforces that. See "Fault injection" in
//! `docs/development.md`.

pub mod crash;
pub mod http;
pub mod plan;
pub mod sql;

pub use plan::{Fault, FaultError, FaultPlan, Filter, Fired, Rule, SqlFault, Trigger};
