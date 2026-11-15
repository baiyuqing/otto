//! Durable, workspace-scoped multi-agent workflows.
pub mod contracts;
pub mod error;
pub(crate) mod provenance;
pub mod repository;
pub mod service;
pub mod sqlite;
pub(crate) mod tool;
pub use crate::workflow::sqlite::SqliteWorkflowStore as Store;
pub use contracts::*;
pub use error::Error;
pub use repository::WorkflowStore;
pub use service::{Catalog, Controller, Executor};

#[cfg(test)]
pub(crate) mod conversation_tests;
