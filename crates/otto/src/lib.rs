//! Otto's native (non-wasm) layer.
//!
//! Everything portable lives in `otto-core`; this crate holds the parts that
//! need a filesystem, a clock or the operating system.

pub mod session;
