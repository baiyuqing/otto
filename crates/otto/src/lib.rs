//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

/// Build version: `X.Y.Z+<12 hex>[.dirty]` from the git commit the binary was
/// built from, `X.Y.Z+<OTTO_GIT_HASH>` when git is unavailable and that
/// variable is set, or `X.Y.Z+unknown`. `+` starts SemVer build metadata.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("OTTO_BUILD_METADATA"));

pub mod app;
pub mod approval;
pub mod auth;
pub mod child_fence;
pub mod cli;
pub mod client;
pub mod config;
pub mod deadline;
pub mod decision;
pub mod delivery;
pub mod embed;
pub mod failover;
#[cfg(any(test, feature = "fault-injection"))]
pub mod faults;
/// Test support for this crate's tests and for the tests of dependent crates;
/// see the `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures;
pub mod fsops;
mod gourl;
pub mod herdr;
pub mod hex;
pub mod log;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod plugin;
pub mod program_status;
pub mod provider;
pub(crate) mod random;
pub mod retry;
pub mod runtime;
pub mod sandbox;
pub mod schedule;
pub mod session;
pub mod skill;
pub mod storage;
pub mod subagent;
pub mod tls_provider;
pub mod tool;
pub mod tui;
pub mod typesafe;
pub mod urlprivacy;
pub mod usage;
pub mod workflow;
pub mod workspace;
