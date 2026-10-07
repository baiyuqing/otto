//! Native session operations exposed to runtime consumers.

use std::path::Path;
use std::sync::Arc;

use otto_core::session::{
    ForkError, ForkPoint, ForkRecord, Header, ListResult, RuntimeMetadata, Session, SessionInfo,
    Snapshot, Warning,
};

use super::{ArchiveResult, Forked, Recovery};

/// The current session writer authority, independent of its storage protocol.
pub trait WriterOwnership: std::fmt::Debug + Send + Sync {
    fn check(&self) -> Result<(), String>;
    fn release(&self) -> Result<(), String>;
}

/// Native session operations used by frontends and runtime composition.
pub trait SessionHandle: Session + Send + Sync {
    fn header(&self) -> Header;
    fn name(&self) -> String;
    fn path(&self) -> String;
    fn rename(&self, name: &str) -> Result<(), String>;
    fn thinking_level(&self) -> String;
    fn update_thinking_level(&self, thinking: &str) -> Result<(), String>;
    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String>;
    fn close(&self) -> Result<(), String>;

    fn archive(&self, _root: &Path, _workspace: &str) -> Result<ArchiveResult, String> {
        Err("session persistence is disabled".to_string())
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }

    fn lease(&self) -> Option<Arc<dyn WriterOwnership>> {
        None
    }

    fn take_recovery(&self) -> Option<Recovery> {
        None
    }

    /// Where this session was forked from, when it is a fork.
    fn forked_from(&self) -> Option<ForkRecord> {
        None
    }
}

/// A prepared native session held until identity is revalidated and activated.
pub trait PreparedSession: Send + Sync {
    fn info(&self) -> SessionInfo;
    fn with_replayable(self: Box<Self>, replayable: fn(&str) -> bool) -> Box<dyn PreparedSession>;
    fn activate(self: Box<Self>) -> Result<(Arc<dyn SessionHandle>, Vec<Warning>), String>;
    fn close(self: Box<Self>) -> Result<(), String>;
}

/// Discovery, creation and preparation boundary for native session repositories.
/// References are opaque to callers; POSIX accepts the legacy path reference.
pub trait SessionRepository: Send + Sync {
    /// Checks an explicitly selected namespace without creating it.
    fn validate_existing_root(&self, root: &Path) -> Result<(), String>;
    fn create_lazy(
        &self,
        root: &Path,
        header: Header,
        failover_seconds: Option<u64>,
    ) -> Result<Arc<dyn SessionHandle>, String>;
    fn create_child_lazy(
        &self,
        parent_reference: &str,
        name: &str,
        header: Header,
        parent_writer: Option<Arc<dyn WriterOwnership>>,
    ) -> Result<(Arc<dyn SessionHandle>, String), String>;
    fn prepare(
        &self,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String>;
    fn prepare_in(
        &self,
        root: &Path,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String>;
    fn prepare_listed(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
        create_lease: Option<u64>,
    ) -> Result<Box<dyn PreparedSession>, String>;
    fn list(
        &self,
        root: &Path,
        workspace: &str,
        current_reference: &str,
        limit: usize,
    ) -> Result<ListResult, String>;
    fn archive(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
    ) -> Result<ArchiveResult, String>;
    /// Writes a new session `new_id` beside `reference` whose history is
    /// `reference`'s up to the turn the user message `before` opens, or up to
    /// the last turn boundary without `before`. `reference` is only read.
    fn fork(
        &self,
        reference: &str,
        before: Option<&str>,
        new_id: &str,
    ) -> Result<Forked, ForkError>;
    /// The places `reference` can be forked, oldest first.
    fn fork_points(&self, reference: &str) -> Result<Vec<ForkPoint>, String>;
}
