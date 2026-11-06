use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    Unauthorized,
    Unsupported,
    Conflict,
    Rejected,
    Unavailable,
    RateLimited,
    OutcomeUnknown,
    Canceled,
    Closed,
}

/// Safe public error. No external message or transport payload is accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct Error {
    pub kind: ErrorKind,
    pub detail: &'static str,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        let detail = match kind {
            ErrorKind::InvalidInput => "invalid memory input",
            ErrorKind::Unauthorized => "memory scope is not authorized",
            ErrorKind::Unsupported => "memory operation is unsupported",
            ErrorKind::Conflict => "memory precondition conflict",
            ErrorKind::Rejected => "memory request rejected",
            ErrorKind::Unavailable => "memory service unavailable",
            ErrorKind::RateLimited => "memory service rate limited",
            ErrorKind::OutcomeUnknown => "memory operation outcome unknown",
            ErrorKind::Canceled => "memory operation canceled",
            ErrorKind::Closed => "memory service closed",
        };
        Self { kind, detail }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub namespace: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Model,
    Extractor,
}

/// Host-established identity and authority. Never populate this from service
/// metadata or model arguments. A scope names authority; it does not prove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    pub principal: String,
    pub scopes: Vec<Scope>,
    /// Explicit destination for operations that form new memory.
    pub write_scope: Option<Scope>,
    pub workspace_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    pub service: String,
    pub scope: Scope,
    pub id: String,
    pub revision: Option<String>,
}

/// Closed namespace set, in fixed decreasing conflict authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeNamespace {
    Organization,
    Workspace,
    User,
}
impl ScopeNamespace {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Workspace => "workspace",
            Self::User => "user",
        }
    }
    pub fn priority(self) -> u8 {
        match self {
            Self::Workspace => 3,
            Self::Organization => 2,
            Self::User => 1,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Local,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub id: String,
    pub role: String,
    pub occurred_at: Option<i64>,
    pub trust: Trust,
}

/// A resource URI is reference material, never permission to fetch or execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Text(String),
    Resource { uri: String, media_type: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallItem {
    pub reference: Option<MemoryRef>,
    pub content: Content,
    pub sources: Vec<SourceRef>,
    pub effective_at: Option<i64>,
    /// Provider-specific information, never trust, approval or authority.
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_filter: Option<Vec<ScopeNamespace>>,
    pub context: MemoryContext,
    pub query: String,
    pub limit: usize,
    pub token_budget: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallResult {
    pub items: Vec<RecallItem>,
    pub used_tokens: Option<usize>,
    pub coverage: Coverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Recall,
    Observe,
    Remember,
    Forget,
    Inspect,
    Feedback,
    Prepare,
    Apply,
    OperationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    StoreEvidence,
    ChangeMemory,
    BackgroundEvolution,
}

/// Guarantees are independent declarations. Missing declarations guarantee
/// nothing. The host separately admits each requested effect and mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub operation: Operation,
    pub effects: Vec<Effect>,
    pub supports_reviewed_changes: bool,
    pub supports_delegated_learning: bool,
    pub idempotent: bool,
    pub conditional_updates: bool,
    pub tracks_completion: bool,
    /// Exact supported target forms and coverage; empty means no promise.
    #[serde(default)]
    pub forget_targets: Vec<ForgetTargetKind>,
    #[serde(default)]
    pub forget_levels: Vec<ForgetLevel>,
}

impl Capability {
    pub fn new(operation: Operation) -> Self {
        Self {
            operation,
            effects: Vec::new(),
            supports_reviewed_changes: false,
            supports_delegated_learning: false,
            idempotent: false,
            conditional_updates: false,
            tracks_completion: false,
            forget_targets: Vec::new(),
            forget_levels: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDescription {
    pub identity: String,
    pub version: u32,
    pub capabilities: Vec<Capability>,
    /// Unknown protection must never satisfy a host encryption requirement.
    #[serde(default)]
    pub encryption_at_rest: Option<bool>,
}

impl ServiceDescription {
    pub fn new(identity: String) -> Self {
        Self {
            identity,
            version: 1,
            capabilities: Vec::new(),
            encryption_at_rest: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationMode {
    #[default]
    ReviewedChanges,
    DelegatedLearning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningGrant {
    pub scopes: Vec<Scope>,
    pub input_classes: Vec<String>,
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub event_id: String,
    pub content: Content,
    pub source: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    ArchiveOnly,
    Learn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveRequest {
    pub context: MemoryContext,
    pub request_id: String,
    pub observations: Vec<Observation>,
    pub mode: ObserveMode,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub context: MemoryContext,
    pub request_id: String,
    pub content: Content,
    pub target: Option<MemoryRef>,
    pub metadata: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetLevel {
    Suppress,
    Invalidate,
    DeleteActive,
    DeleteDerived,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetTargetKind {
    Item,
    Source,
    Scope,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetTarget {
    Item(MemoryRef),
    Source(SourceRef),
    Scope(Scope),
}
impl ForgetTarget {
    pub fn kind(&self) -> ForgetTargetKind {
        match self {
            Self::Item(_) => ForgetTargetKind::Item,
            Self::Source(_) => ForgetTargetKind::Source,
            Self::Scope(_) => ForgetTargetKind::Scope,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub context: MemoryContext,
    pub request_id: String,
    pub target: ForgetTarget,
    pub coverage: ForgetLevel,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectQuery {
    Get(MemoryRef),
    List {
        scope: Scope,
        limit: usize,
        cursor: String,
    },
    Status,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectRequest {
    pub context: MemoryContext,
    pub query: InspectQuery,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResult {
    pub items: Vec<RecallItem>,
    pub next_cursor: String,
    pub state: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feedback {
    Useful,
    Irrelevant,
    Incorrect,
    Obsolete,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRequest {
    pub context: MemoryContext,
    pub request_id: String,
    pub references: Vec<MemoryRef>,
    pub feedback: Feedback,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutatingRequest {
    Observe(ObserveRequest),
    Remember(RememberRequest),
    Forget(ForgetRequest),
    Feedback(FeedbackRequest),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub operation: MutatingRequest,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    Create,
    Update,
    Forget,
    /// Withdraw the target without asserting its opposite.
    Retract,
    /// Replace the target with the proposed content; the target is marked
    /// superseded and names its replacement.
    Supersede,
    /// Replace an incorrect target with the proposed content; the target is
    /// marked retracted.
    Correct,
    /// Mark the target as needing human review.
    NeedsReview,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub action: ChangeAction,
    pub target: Option<MemoryRef>,
    pub content: Option<Content>,
    pub scope: Scope,
    pub sources: Vec<SourceRef>,
    pub metadata: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedChanges {
    pub reference: String,
    pub context: MemoryContext,
    pub changes: Vec<Change>,
    pub effects: Vec<Effect>,
    pub expires_at: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyRequest {
    pub context: MemoryContext,
    pub request_id: String,
    pub preparation: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationStatusRequest {
    pub context: MemoryContext,
    pub operation_ref: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Acknowledged,
    Rejected,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Pending,
    Running,
    Complete,
    Partial,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Ready,
    Pending,
    Unknown,
    NotApplicable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Applied,
    NoChange,
    Skipped,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub reference: Option<MemoryRef>,
    pub state: ItemState,
    pub reason: Option<ErrorKind>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetCoverage {
    pub achieved: Vec<ForgetLevel>,
    pub remaining: Vec<ForgetLevel>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: String,
    pub operation_ref: Option<String>,
    pub admission: Admission,
    pub completion: Completion,
    pub visibility: Visibility,
    pub items: Vec<ItemOutcome>,
    pub forget_coverage: Option<ForgetCoverage>,
}
