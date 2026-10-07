//! Durable memory contracts, policy, guards, repository port, and service.

pub mod adapter;
pub mod advice;
pub mod client;
pub mod codec;
pub mod contracts;
pub mod guard;
pub mod json;
pub mod migration;
pub mod proposed;
pub mod repository;
pub mod scope;
pub mod service;
pub mod turso;
pub mod validate;

pub use contracts::*;
pub use repository::MemoryStore;
pub use service::{Binding, Policy, Service};
