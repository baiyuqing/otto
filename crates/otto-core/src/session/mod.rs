//! The transcript store contract and an in-memory implementation.
//!
//! Phase 0 carries the two operations the agent loop needs; the Pi v3 JSONL
//! store arrives in phase 2.
//!
//! Ownership: `append` takes the message by value and the session owns it
//! afterwards. `messages` returns an independent copy of the transcript.
//!
//! Concurrency: implementations take `&self` and must be safe for concurrent
//! use. [`MemorySession`] is guarded by a mutex.
//!
//! Errors: history is append-only, so a rejected message is not stored and the
//! call returns a [`SessionError`]. A rejection never leaves partial state.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::model::{BlockType, Message, Role, ValidationError};

pub mod codec;
pub mod compaction;
pub mod context;
pub mod fork;
pub mod memory_recall;
pub mod operation;
pub mod pi;
pub mod plan;
pub mod provider_call;
pub mod remote_compaction;
pub mod session_types;
pub mod trace;
pub mod turn;

pub use codec::{PiRecord, decode_pi_entry, decode_pi_file, decode_pi_header, encode_pi_record};
pub use context::{
    BRANCH_CONTEXT_TYPE, CAPABILITIES_CUSTOM_TYPE, CAPABILITY_UPDATE_CONTEXT_TYPE,
    COMPACTION_CONTEXT_TYPE, Capabilities, ContextEntryIndex, HistoryMessage,
    OTTO_RUNTIME_CUSTOM_TYPE, ResolvedContext, active_branch_history, active_context_path,
    build_context, decode_capabilities, encode_capabilities, index_context_entries, is_pi_entry_id,
    new_context_message,
};
pub use fork::{
    FORK_CUSTOM_TYPE, FORK_SCHEMA_VERSION, ForkError, ForkPlan, ForkPoint, ForkPromptError,
    ForkRecord, decode_fork_record, encode_fork_record, fork_origin, fork_point_by_prompt,
    fork_points, plan_fork,
};
pub use memory_recall::{
    MEMORY_RECALL_CUSTOM_TYPE, MEMORY_RECALL_MAXIMUM_RECORDS, MEMORY_RECALL_SCHEMA_VERSION,
    MemoryRecallRecord, MemoryRecordRef, MemoryReferenceCoverage, TurnMemoryReferences,
    decode_memory_recall, encode_memory_recall, normalize_memory_records,
};
pub use operation::{
    DecodedOperationFact, OPERATION_CUSTOM_TYPE, OPERATION_SCHEMA_VERSION, OperationFact,
    OperationFactError, OperationKind, OperationLedger, OperationRecord, decode_operation_fact,
    encode_operation_fact,
};
pub use pi::{
    MAX_SESSION_ENTRY_BYTES, MAX_SESSION_FILE_BYTES, PI_SESSION_VERSION, PiBranchSummary,
    PiCompaction, PiContentBlock, PiContextEdit, PiCost, PiCustom, PiCustomMessage, PiEntry,
    PiFile, PiHeader, PiLabel, PiMessage, PiModelChange, PiOttoDetails, PiSessionInfo,
    PiThinkingLevelChange, PiUsage, decode_pi_otto_details, encode_pi_otto_details,
};
pub use plan::{
    MAX_PLAN_STEP_CHARS, MAX_PLAN_STEPS, PLAN_CUSTOM_TYPE, PLAN_SCHEMA_VERSION, PlanStatus,
    PlanStep, decode_plan, encode_plan, plan_checklist, plan_summary, validate_plan,
};
pub use provider_call::{
    PROVIDER_CALL_CUSTOM_TYPE, PROVIDER_CALL_SCHEMA_VERSION, ProviderCallKind, ProviderCallRecord,
    ProviderRetryRecord, RequestEstimate, decode_provider_call,
};
pub use remote_compaction::{
    MAX_REMOTE_COMPACTION_CONTENT_BYTES, MAX_REMOTE_COMPACTION_ID_BYTES,
    MAX_REMOTE_COMPACTION_PROVIDER_BYTES, REMOTE_COMPACTION_CUSTOM_TYPE,
    REMOTE_COMPACTION_SCHEMA_VERSION, RemoteCompaction, decode_remote_compaction,
    encode_remote_compaction, validate_remote_compaction,
};
pub use session_types::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionDetails, CompactionMetadata, Header,
    ListResult, RuntimeMetadata, SessionInfo, Snapshot, Warning,
};
pub use turn::{
    InterruptedTurn, TURN_CUSTOM_TYPE, TurnOutcome, TurnRecord, TurnTrigger, decode_turn_record,
    encode_turn_record,
};

/// Which error class a [`PiError`] corresponds to.
///
/// Callers branch on the kind rather than on the message: the kind survives
/// every `context` prefix, so the classification of a failure deep inside a
/// file survives being reported as "session line 7: ...".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiErrorKind {
    /// The record is not a Pi v3 session, or carries an unsupported version.
    UnsupportedFormat,
    /// The record is a Pi session but breaks a validation rule.
    Invalid,
    /// One record exceeds [`MAX_SESSION_ENTRY_BYTES`].
    EntryTooLarge,
    /// The file exceeds [`MAX_SESSION_FILE_BYTES`].
    FileTooLarge,
    /// The record is valid Pi but carries content Otto cannot represent.
    UnsupportedContent,
    /// The session has been closed.
    Closed,
    /// A durable write failed. The store is poisoned and refuses every later
    /// write.
    FatalPersistence,
    /// Anything with no dedicated kind, such as an encoding failure.
    Other,
}

impl PiErrorKind {
    /// The error text this kind prefixes its message with.
    fn text(self) -> &'static str {
        match self {
            Self::UnsupportedFormat => "unsupported session format",
            Self::Invalid => "invalid session",
            Self::EntryTooLarge => "session entry too large",
            Self::FileTooLarge => "session file too large",
            Self::UnsupportedContent => "unsupported session content",
            Self::Closed => "session is closed",
            Self::FatalPersistence => "fatal session persistence failure",
            Self::Other => "",
        }
    }
}

/// A session codec or store failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PiError {
    kind: PiErrorKind,
    message: String,
}

impl PiError {
    /// Builds `"<sentinel>: <detail>"` for one of the sentinel kinds.
    pub fn new(kind: PiErrorKind, detail: impl fmt::Display) -> Self {
        Self {
            message: format!("{}: {detail}", kind.text()),
            kind,
        }
    }

    /// Shorthand for [`PiErrorKind::Invalid`], by far the most common kind.
    pub fn invalid(detail: impl fmt::Display) -> Self {
        Self::new(PiErrorKind::Invalid, detail)
    }

    /// An error with no dedicated kind; the message is used verbatim.
    pub fn other(message: impl fmt::Display) -> Self {
        Self {
            kind: PiErrorKind::Other,
            message: message.to_string(),
        }
    }

    /// The closed-session error. It carries no detail.
    pub fn closed() -> Self {
        Self {
            kind: PiErrorKind::Closed,
            message: PiErrorKind::Closed.text().to_owned(),
        }
    }

    /// Wraps the cause of a failed durable write. Once a store returns this it
    /// returns the same error from every later write.
    pub fn fatal(cause: impl fmt::Display) -> Self {
        Self::new(PiErrorKind::FatalPersistence, cause)
    }

    /// The size-limit message shared by the two too-large kinds.
    pub fn size(kind: PiErrorKind, limit: usize) -> Self {
        Self::new(kind, format!("maximum is {limit} bytes"))
    }

    /// Prefixes the message, keeping the kind.
    #[must_use]
    pub fn context(self, prefix: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            message: format!("{prefix}: {}", self.message),
        }
    }

    /// The sentinel this error corresponds to.
    pub fn kind(&self) -> PiErrorKind {
        self.kind
    }
}

/// Why a message could not be appended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The message itself is malformed. See [`Message::validate`].
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    /// The message is well formed but breaks the tool-call ordering rule.
    #[error("{0}")]
    Sequence(&'static str),
    /// An operation fact is malformed or conflicts with prior history.
    #[error(transparent)]
    Operation(#[from] OperationFactError),
    /// The message is well formed but could not be persisted. Only a
    /// file-backed session produces this; [`MemorySession`] never does.
    #[error("{0}")]
    Persist(String),
}

/// Bounds for an append-only transcript: session futures are `Send`, so
/// shared session references must be `Sync`.
pub trait SessionBounds: Sync {}

impl<T: Sync + ?Sized> SessionBounds for T {}

/// An append-only transcript.
#[async_trait::async_trait]
pub trait Session: SessionBounds {
    /// Stable runner identity when this transcript has one (for example its
    /// persisted header ID). Wrappers must forward this identity unchanged.
    fn session_id(&self) -> Option<String> {
        None
    }

    /// A copy of the transcript in append order.
    fn messages(&self) -> Vec<Message>;

    /// Model-visible projection. File-backed Pi sessions override this for
    /// append-only `context_edit`; ordinary sessions use their transcript.
    fn model_messages(&self) -> Vec<Message> {
        self.messages()
    }

    /// Appends one message after validating it and the tool-call ordering
    /// rule. Validation errors store nothing. A persistence failure may have
    /// committed without acknowledgement: stop and recover durable history
    /// before retrying, rather than assuming rollback.
    async fn append(&self, message: Message) -> Result<(), SessionError>;

    /// The compaction checkpoint currently in force, or `None` when the session
    /// has never been compacted.
    fn latest_compaction(&self) -> Option<CompactionMetadata>;

    /// Records a compaction checkpoint: the transcript becomes the summary
    /// context message followed by the messages from
    /// `checkpoint.first_kept_entry_id` onward. Validation failures do not
    /// change history; persistence failures follow the same uncertain-commit
    /// recovery contract as `append`.
    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError>;

    /// Appends one typed operation fact after strict history validation.
    async fn append_operation_fact(&self, fact: OperationFact) -> Result<(), SessionError>;

    /// Typed operation facts in append order. Stores that support operation
    /// recovery override this; compatibility wrappers may return no history.
    fn operation_history(&self) -> Vec<OperationFact> {
        Vec::new()
    }

    /// A snapshot of the validated operation ledger. Stores that support
    /// operation recovery override this.
    fn operation_ledger(&self) -> OperationLedger {
        OperationLedger::default()
    }

    /// Appends a `custom` entry with the given `customType` and JSON-encoded
    /// `data`, outside the message/context path: [`pi_entry_to_context_messages`]
    /// never turns a bare `custom` entry into a context message, the same way
    /// it already ignores `otto.runtime`.
    ///
    /// [`pi_entry_to_context_messages`]: crate::session::context::pi_entry_to_context_messages
    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError>;

    /// The provider-managed compaction checkpoint currently in force.
    fn remote_compaction(&self) -> Option<RemoteCompaction> {
        None
    }

    /// Model messages appended after [`Self::remote_compaction`]. When no
    /// remote checkpoint is active this is the ordinary model projection.
    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.model_messages()
    }

    /// Atomically appends and activates a provider-managed checkpoint.
    /// Implementations that persist Pi sessions should override this to append
    /// an `otto.remote_compaction` custom entry before publishing live state.
    async fn append_remote_compaction(
        &self,
        checkpoint: RemoteCompaction,
    ) -> Result<(), SessionError> {
        let checkpoint_json = encode_remote_compaction(&checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))?;
        self.append_custom(REMOTE_COMPACTION_CUSTOM_TYPE, &checkpoint_json)
            .await
    }

    /// The latest `otto.capabilities` record on the active path, or `None`
    /// when there is none. Like `append_custom`, it has no
    /// default body: every implementation forwards or states its behavior.
    fn observed_capabilities(&self) -> Option<Capabilities>;

    /// The newest `otto.turn` start on the active path with no matching end:
    /// a turn process loss interrupted. `None` when every recorded turn
    /// ended or none was recorded. No default body, like
    /// [`Self::observed_capabilities`].
    fn interrupted_turn(&self) -> Option<InterruptedTurn>;

    /// The steps of the latest valid `otto.plan` record on the active path;
    /// empty when there is none or the latest one cleared the plan. No
    /// default body, like [`Self::observed_capabilities`].
    fn plan(&self) -> Vec<PlanStep>;

    /// Latest valid goal record on the active path.
    fn goal(&self) -> Option<crate::goal::Goal> {
        None
    }

    /// Atomically append a goal only if the active goal equals `expected`.
    async fn compare_and_set_goal(
        &self,
        expected: Option<&crate::goal::Goal>,
        goal: &crate::goal::Goal,
    ) -> Result<bool, SessionError> {
        let _ = (expected, goal);
        Err(SessionError::Persist(
            "goal compare-and-set unsupported".into(),
        ))
    }

    /// The `otto.memory_recall` records on the active path, oldest first,
    /// each anchored to the nearest preceding `Role::User` message, including
    /// records of turns a compaction checkpoint has since summarized. Empty
    /// for a session written before the record existed. No default body,
    /// like [`Self::observed_capabilities`].
    fn memory_references(&self) -> Vec<TurnMemoryReferences>;

    /// Appends `message` as the delivery of input `input_id` unless a message
    /// with that input id is already on the active path. The check and the
    /// append run under one lock; a file-backed store persists before it
    /// returns [`AppendOutcome::Appended`]. [`AppendOutcome::AlreadyPresent`]
    /// is the same committed delivery outcome. No default body: every
    /// implementor, including wrappers, forwards or states its behavior.
    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: Message,
    ) -> Result<AppendOutcome, SessionError>;

    /// Whether a message with input id `input_id` is on the active path.
    fn has_input(&self, input_id: &str) -> bool;
}

/// What [`Session::append_input_if_absent`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    AlreadyPresent,
}

/// Tool calls from the most recent assistant message that have no result yet.
#[derive(Debug, Default)]
struct State {
    messages: Vec<Message>,
    /// Tool-call id to tool name, for calls awaiting a result.
    pending: HashMap<String, String>,
    /// Every tool-call id seen in this session, to reject reuse.
    seen_call_ids: HashSet<String>,
    /// Every message id seen in this session, to reject reuse.
    seen_ids: HashSet<String>,
    latest_compaction: Option<CompactionMetadata>,
    remote_compaction: Option<RemoteCompaction>,
    post_remote_compaction_messages: Vec<Message>,
    /// Numbers the synthetic checkpoint ids and the ids generated for messages
    /// appended without one. An in-memory session has no file to share ids
    /// with, so a counter is enough and keeps tests deterministic.
    checkpoint_counter: u64,
    operation_facts: Vec<OperationFact>,
    operation_ledger: OperationLedger,
    observed_capabilities: Option<Capabilities>,
    interrupted_turn: Option<InterruptedTurn>,
    plan: Vec<PlanStep>,
    goal: Option<crate::goal::Goal>,
    /// Input ids of every appended message, for
    /// [`Session::append_input_if_absent`].
    seen_inputs: HashSet<String>,
    /// Every `otto.memory_recall` record, anchored when it was appended.
    memory_references: Vec<TurnMemoryReferences>,
}

impl State {
    /// Appends one validated message under the session lock.
    fn append(&mut self, mut message: Message) -> Result<(), SessionError> {
        let state = self;
        if message.id.trim().is_empty() {
            message.id = state.generate_id();
        }
        if state.seen_ids.contains(&message.id) {
            return Err(SessionError::Sequence("duplicate message id"));
        }
        state.check_sequence(&message)?;
        if state
            .latest_compaction
            .as_ref()
            .is_some_and(|latest| latest.first_post_checkpoint_message_id.is_empty())
        {
            if message.id.trim().is_empty() {
                return Err(SessionError::Sequence(
                    "first post-checkpoint message id is required",
                ));
            }
            if !matches!(message.role, Role::User | Role::Assistant | Role::Tool) {
                return Err(SessionError::Sequence(
                    "first post-checkpoint message must have a normal role",
                ));
            }
            let id = message.id.clone();
            if let Some(latest) = state.latest_compaction.as_mut() {
                latest.first_post_checkpoint_message_id = id;
            }
        }
        state.seen_ids.insert(message.id.clone());
        if !message.input_id.is_empty() {
            state.seen_inputs.insert(message.input_id.clone());
        }
        if state.remote_compaction.is_some() {
            state.post_remote_compaction_messages.push(message.clone());
        }
        state.record(message);
        Ok(())
    }

    /// The ordering rule, applied incrementally: a tool-result message must
    /// resolve a call from the immediately preceding assistant message, and no
    /// other message may come between the two.
    fn check_sequence(&self, message: &Message) -> Result<(), SessionError> {
        const UNRESOLVED: &str = "unresolved tool calls must be followed by tool results";
        match message.role {
            Role::Assistant => {
                if !self.pending.is_empty() {
                    return Err(SessionError::Sequence(UNRESOLVED));
                }
                let mut seen_here = HashSet::new();
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    if self.seen_call_ids.contains(&block.tool_call_id)
                        || !seen_here.insert(&block.tool_call_id)
                    {
                        return Err(SessionError::Sequence("duplicate tool-call id"));
                    }
                }
                Ok(())
            }
            Role::Tool => {
                let mut resolved = HashSet::new();
                for block in &message.blocks {
                    let Some(name) = self.pending.get(&block.tool_call_id) else {
                        return Err(SessionError::Sequence("tool result has no pending call"));
                    };
                    if !resolved.insert(&block.tool_call_id) {
                        return Err(SessionError::Sequence("duplicate tool-call id"));
                    }
                    if *name != block.tool_name {
                        return Err(SessionError::Sequence(
                            "tool result name does not match pending call",
                        ));
                    }
                }
                Ok(())
            }
            _ => {
                if self.pending.is_empty() {
                    Ok(())
                } else {
                    Err(SessionError::Sequence(UNRESOLVED))
                }
            }
        }
    }

    /// Replays `messages` through the sequence rule from an empty state,
    /// validating a whole candidate slice.
    fn replay(messages: &[Message]) -> Result<State, SessionError> {
        let mut state = State::default();
        for message in messages {
            state.check_sequence(message)?;
            state.record(message.clone());
        }
        Ok(state)
    }

    /// A fresh identifier for a message appended without one. The counter is
    /// bumped until the id is unused.
    fn generate_id(&mut self) -> String {
        loop {
            self.checkpoint_counter += 1;
            let id = format!("m-{}", self.checkpoint_counter);
            if !self.seen_ids.contains(&id) {
                return id;
            }
        }
    }

    fn record(&mut self, message: Message) {
        match message.role {
            Role::Assistant => {
                for block in &message.blocks {
                    if block.block_type == BlockType::ToolCall {
                        self.seen_call_ids.insert(block.tool_call_id.clone());
                        self.pending
                            .insert(block.tool_call_id.clone(), block.tool_name.clone());
                    }
                }
            }
            Role::Tool => {
                for block in &message.blocks {
                    self.pending.remove(&block.tool_call_id);
                }
            }
            _ => {}
        }
        self.messages.push(message);
    }
}

mod memory_session;
pub use memory_session::MemorySession;

#[cfg(test)]
mod tests;

pub mod local_command;
