//! Durable memory: the domain contracts, the policy and secret guards, the
//! SQLite store, and the service the tools and REPL commands call.
//!
//! Ported from Go `internal/memory`. The store shares its database file with
//! the Go binary, so the schema, pragmas, file location and on-disk encoding
//! are byte-compatible rather than merely equivalent.

pub mod contracts;
pub mod guard;
pub mod json;
pub mod scope;
pub mod service;
pub mod sqlite;
pub mod validate;

pub use contracts::*;
pub use service::{Binding, Policy, Service};
