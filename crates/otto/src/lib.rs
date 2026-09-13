//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

pub mod cli;
pub mod config;
mod gourl;
pub mod memory;
pub mod provider;
pub mod sandbox;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod tool;
pub mod urlprivacy;
