//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

pub mod acp;
pub mod app;
pub mod auth;
pub mod cli;
pub mod config;
pub mod deadline;
pub mod failover;
mod gourl;
pub mod inbound;
pub mod mcp;
pub mod memory;
pub mod provider;
pub mod reflection;
pub mod retry;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod tool;
pub mod tui;
pub mod urlprivacy;
pub mod usage;
pub mod workflow;
