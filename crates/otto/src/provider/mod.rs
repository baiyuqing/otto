//! Native provider implementations.
//!
//! Port of the HTTP half of `internal/provider`. The neutral contract
//! ([`otto_core::provider::Provider`]) and the wire codecs live in
//! `otto-core`; this module owns the transport: connection settings, the
//! retry policy, and credential redaction.

pub mod openaicompat;
