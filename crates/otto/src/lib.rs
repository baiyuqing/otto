//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

/// Build version: `X.Y.Z+<12 hex>[.dirty]` from the git commit the binary was
/// built from, `X.Y.Z+<OTTO_GIT_HASH>` when git is unavailable and that
/// variable is set, or `X.Y.Z+unknown`. `+` starts SemVer build metadata.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("OTTO_BUILD_METADATA"));

pub mod acp;
pub mod app;
pub mod approval;
pub mod auth;
pub mod cli;
pub mod client;
pub mod config;
pub mod deadline;
pub mod failover;
mod gourl;
pub mod log;
pub mod mcp;
pub mod memory;
pub mod provider;
pub mod reflection;
pub mod retry;
pub mod sandbox;
pub mod schedule;
pub mod server;
pub mod session;
pub mod skill;
pub mod storage;
pub mod subagent;
pub mod tool;
pub mod tui;
pub mod typesafe;
pub mod urlprivacy;
pub mod usage;
pub mod workflow;
