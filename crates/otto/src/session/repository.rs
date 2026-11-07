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
    /// Persist an already-sanitized local submission event before clearing the editor.
    fn append_input_submissions(&self, _encoded: &str) -> Result<(), String> {
        Err("input recovery recording is unavailable for this session repository".into())
    }
    /// Append-order local submission events; unsupported repositories fail closed.
    fn input_submission_events(&self) -> Result<Vec<String>, String> {
        Err("input recovery reading is unavailable for this session repository".into())
    }
    /// Record an already-scanned independent Pi custom event, synchronously.
    fn append_local_command(&self, _encoded: &str) -> Result<(), String> {
        Err("local command recording is unavailable for this session repository".into())
    }
    /// Human-only command observations; never part of `Session::messages`.
    fn local_commands(&self) -> Vec<otto_core::session::local_command::LocalCommandReceipt> {
        Vec::new()
    }
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

    /// Irreversibly removes this exclusively owned session and its sidecars.
    /// Unsupported repositories fail closed.
    fn permanent_delete(&self, _root: &Path, _workspace: &str) -> Result<(), String> {
        Err("permanent session deletion is unavailable".into())
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

    /// Appends a plain custom entry once the session is persisted. A handle
    /// whose backing file does not exist yet queues the entry and writes it
    /// right after the header and runtime entry when the first durable write
    /// creates the file; a handle with a file writes it immediately. The
    /// default implementation writes immediately through
    /// [`Session::append_custom`]. The returned future is boxed so the trait
    /// stays usable as `dyn SessionHandle`.
    fn append_custom_when_created<'a>(
        &'a self,
        custom_type: &'a str,
        data: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), otto_core::session::SessionError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(self.append_custom(custom_type, data))
    }
}

/// A prepared native session held until identity is revalidated and activated.
pub trait PreparedSession: Send + Sync {
    fn info(&self) -> SessionInfo;
    fn with_replayable(self: Box<Self>, replayable: fn(&str) -> bool) -> Box<dyn PreparedSession>;
    fn activate(self: Box<Self>) -> Result<(Arc<dyn SessionHandle>, Vec<Warning>), String>;
    fn close(self: Box<Self>) -> Result<(), String>;
}

/// A concrete session location for deletion target resolution.
///
/// Duplicate IDs and names are deliberately retained. Callers must resolve an
/// ID before a name and reject ambiguous matches across the complete result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteCandidate {
    pub id: String,
    pub name: String,
    pub path: String,
    pub archived: bool,
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
    /// Enumerates all active and archived sessions owned by this workspace.
    /// No display limit, truncation or ID deduplication is applied. Any unreadable,
    /// invalid or changed candidate fails the entire read; never delete from a
    /// partial result. Deletion must still revalidate the selected path.
    fn list_for_delete(
        &self,
        _root: &Path,
        _workspace: &str,
    ) -> Result<Vec<DeleteCandidate>, String> {
        Err("complete session enumeration for deletion is unavailable".into())
    }
    fn archive(
        &self,
        root: &Path,
        workspace: &str,
        reference: &str,
    ) -> Result<ArchiveResult, String>;
    /// Irreversibly removes a listed session, refusing any active writer.
    fn permanent_delete(
        &self,
        _root: &Path,
        _workspace: &str,
        _reference: &str,
    ) -> Result<(), String> {
        Err("permanent session deletion is unavailable".into())
    }
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
