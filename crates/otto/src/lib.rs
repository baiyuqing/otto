//! Native side of Otto: everything that needs the filesystem, processes, the
//! clock, or the network. The portable half lives in `otto-core`.

mod gourl;
pub mod safetext;
pub mod sandbox;
pub mod session;
pub mod tool;
pub mod urlprivacy;
