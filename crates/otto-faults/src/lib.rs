//! Test-only fault injection: one typed fault vocabulary ([`plan`]) and
//! decorators over the seams of `otto-platform` and `otto-storage`, plus the
//! scripted HTTP origin server ([`http`]).
//!
//! Test support only. No shipped build links this crate: `otto` depends on it
//! through its `fault-injection` feature and a dev-dependency, and the other
//! crates through dev-dependencies. Doubles that need `otto` domain modules
//! stay in `otto::faults`. Production code never names this crate;
//! `crates/otto/tests/faults_boundary.rs` enforces that. See "Fault
//! injection" in `docs/development/testing.md`.

pub mod crash;
pub mod files;
pub mod http;
pub mod plan;
pub mod publish;
pub mod sql;

pub use plan::{Fault, FaultError, FaultPlan, Filter, Fired, Rule, SqlFault, Trigger};
