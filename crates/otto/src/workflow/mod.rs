//! Durable, workspace-scoped multi-agent workflows.
pub mod contracts;
pub mod error;
pub mod migration;
pub(crate) mod provenance;
pub mod repository;
pub mod service;
pub use crate::storage::backends::turso::workflow::TursoWorkflowStore as Store;
pub use contracts::*;
pub use error::Error;
pub use repository::WorkflowStore;
pub use service::{Catalog, Controller, Executor};

#[cfg(test)]
pub(crate) mod conversation_tests;
