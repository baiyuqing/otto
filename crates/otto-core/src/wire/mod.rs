//! The `otto serve` wire format and the browser-side reducers that read it.
//!
//! The server serializes [`events::WireEvent`] into SSE frames and the browser
//! parses the same frames back, so both sides are built from one definition
//! here instead of two that must be kept in step.
//!
//! Everything in this module is wasm-safe: no clock, filesystem, network or
//! process access.

pub mod events;
pub mod sse;
pub mod transcript;
