//! Native provider implementations.
//!
//! The neutral contract ([`kite_core::provider::Provider`]) and the wire codecs
//! live in `kite-core`; this module owns the transport: connection settings,
//! the retry policy, and credential redaction.

pub mod chatgpt;
pub mod openaicompat;
