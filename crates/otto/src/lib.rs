//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

pub mod app;
pub mod auth;
pub mod cli;
pub mod config;
mod gourl;
pub mod provider;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod tool;
pub mod urlprivacy;
