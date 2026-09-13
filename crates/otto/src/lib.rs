//! Native side of Otto: everything that needs the filesystem, processes, or
//! the network. The provider-neutral half lives in `otto-core`.

mod gourl;
pub mod safetext;
pub mod sandbox;
pub mod tool;
pub mod urlprivacy;
