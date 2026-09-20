//! Durable memory: the domain contracts, the policy and secret guards, the
//! SQLite store, and the service the tools and REPL commands call.
//!
//! The store keeps the schema, the pragmas, the file location and the on-disk
//! encoding byte-compatible with what the previously released binary wrote, so
//! an existing database file still opens.

pub mod contracts;
pub mod guard;
pub mod json;
pub mod scope;
pub mod service;
pub mod sqlite;
pub mod validate;

pub use contracts::*;
pub use service::{Binding, Policy, Service};
