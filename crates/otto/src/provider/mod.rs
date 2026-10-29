//! Native provider implementations.
//!
//! The neutral contract ([`otto_core::provider::Provider`]) and the wire codecs
//! live in `otto-core`; this module owns the transport: connection settings,
//! the retry policy, and credential redaction.

pub mod catalog;
mod catalog_cache;
pub mod chatgpt;
pub mod claude;
pub mod openaicompat;
