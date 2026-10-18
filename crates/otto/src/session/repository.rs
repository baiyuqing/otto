//! Native session operations exposed to runtime consumers.

use std::path::Path;
use std::sync::Arc;

use otto_core::model::Message;
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
    fn update_thinking_level(&self, thinking: &str) -> Result<(), String>;
    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String>;
    fn close(&self) -> Result<(), String>;

    /// Latest native custom record on the complete active path, including
    /// records before compaction. Unsupported repositories return no evidence.
    fn custom_data(&self, _custom_type: &str) -> Option<String> {
        None
    }

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
    /// Read-only native custom evidence lookup; unsupported repositories fail closed.
    fn custom_data(&self, _reference: &str, _custom_type: &str) -> Option<String> {
        None
    }
    fn custom_data_for(
        &self,
        root: &Path,
        workspace: &str,
        session_id: &str,
        custom_type: &str,
    ) -> Option<String> {
        let source = self
            .list(root, workspace, "", super::MAX_LIST_SESSIONS)
            .ok()?
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)?;
        self.custom_data(&source.path, custom_type)
    }
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
        request_id: Option<&str>,
    ) -> Result<Forked, ForkError>;
    /// The places `reference` can be forked, oldest first.
    fn fork_points(&self, reference: &str) -> Result<Vec<ForkPoint>, String>;
    /// The messages `reference` held before the turn that the user prompt
    /// named by `prompt` opens (see `fork_point_by_prompt`). `reference` is
    /// only read.
    fn fork_context(&self, reference: &str, prompt: &str) -> Result<Vec<Message>, String>;
}
