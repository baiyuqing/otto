//! Memory domain contracts: the request and record types, the size limits, the
//! error kinds and the ID rules.
//!
//! The types stay value-only so the store, the service, and the tools can all
//! agree on one shape. The file store validates every record against the size ceilings, so they
//! must not drift.

use std::fmt;

use chrono::{DateTime, Utc};

pub const MAX_RECORD_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_NAMESPACE_BYTES: usize = 32;
pub const MAX_KIND_BYTES: usize = 32;
pub const MAX_ID_BYTES: usize = 64;
pub const MAX_SCOPE_ID_BYTES: usize = 128;
pub const MAX_SESSION_ID_BYTES: usize = 128;
pub const MAX_MESSAGE_ID_BYTES: usize = 128;
pub const MAX_SEMANTIC_KEY_BYTES: usize = 256;
pub const MAX_LABELS: usize = 32;
pub const MAX_LABEL_BYTES: usize = 64;
pub const MAX_METADATA_ENTRIES: usize = 32;
pub const MAX_METADATA_KEY_BYTES: usize = 64;
pub const MAX_METADATA_VALUE_BYTES: usize = 512;
/// Bounds the canonical JSON object wire bytes for record metadata.
pub const MAX_METADATA_BYTES: usize = 4 * 1024;
pub const MAX_REASON_BYTES: usize = 2 * 1024;
pub const MAX_PROVENANCE_MESSAGE_IDS: usize = 32;
pub const MAX_QUERY_BYTES: usize = 8 * 1024;
/// Bounds the whitespace-separated terms of a retrieval query.
pub const MAX_QUERY_TERMS: usize = 64;
/// Bounds the bytes of one query term.
pub const MAX_QUERY_TERM_BYTES: usize = 256;
/// The token budget of the standing set: active, unexpired, user-scope
/// `preference` and `instruction` records with a key. Recall injects the whole
/// set outside the per-turn limit and budget, and a write that would push the
/// set over this budget is rejected. Not configurable.
pub const MAX_STANDING_TOKENS: usize = 2000;
/// A row cap on the recall query for the standing set. Every record costs at
/// least one token, so only legacy data written past [`MAX_STANDING_TOKENS`]
/// can reach it, and [`StandingUsage::omitted`] counts what it cuts.
pub const STANDING_ROW_GUARD: usize = 128;
pub const MAX_RETRIEVAL_CANDIDATES: usize = 256;
pub const MAX_REQUEST_SCOPES: usize = 16;
pub const MAX_REQUEST_KINDS: usize = 16;
pub const MAX_REQUEST_LABELS: usize = 16;
pub const MAX_PAGE_SIZE: usize = 100;
pub const MAX_RECALL_RECORDS: usize = 64;
pub const MAX_TOKEN_BUDGET: usize = 8192;
pub const MAX_CANDIDATE_BATCH: usize = 8;
pub const MAX_CANDIDATE_BATCH_BYTES: usize = 256 * 1024;
pub const MAX_GUARD_FIELDS: usize = 512;
pub const MAX_GUARD_BYTES: usize = 64 * 1024;
pub const MAX_EXACT_GUARD_SPANS: usize = 8192;
pub const MAX_EXACT_GUARD_VALUES: usize = 64;
pub const MAX_EXACT_GUARD_VALUE_BYTES: usize = 8 * 1024;
pub const MAX_CURSOR_BYTES: usize = 4 * 1024;
pub const MAX_DUPLICATE_ID_RETRIES: usize = 8;

pub const NAMESPACE_USER: &str = "user";
pub const NAMESPACE_WORKSPACE: &str = "workspace";

/// Sentinel category. Errors are compared by `kind`, never by message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Disabled,
    Unavailable,
    Conflict,
    SensitiveMemory,
    Unsupported,
    PersistenceDisabled,
    Busy,
    CommitUnknown,
    Corrupt,
    IncompatibleSchema,
    InvalidRecord,
    InvalidRequest,
    NotFound,
    Closed,
    InvalidCursor,
    Canceled,
    /// It carries the decision as its detail.
    PolicyDecision,
}

impl ErrorKind {
    pub fn message(self) -> &'static str {
        match self {
            ErrorKind::Disabled => "memory is disabled",
            ErrorKind::Unavailable => "memory is unavailable",
            ErrorKind::Conflict => "memory revision conflict",
            ErrorKind::SensitiveMemory => "memory contains sensitive data",
            ErrorKind::Unsupported => "memory operation is unsupported",
            ErrorKind::PersistenceDisabled => "memory persistence is disabled",
            ErrorKind::Busy => "memory store is busy",
            ErrorKind::CommitUnknown => "memory commit outcome is unknown",
            ErrorKind::Corrupt => "memory data is corrupt",
            ErrorKind::IncompatibleSchema => "memory schema is incompatible",
            ErrorKind::InvalidRecord => "invalid memory record",
            ErrorKind::InvalidRequest => "invalid memory request",
            ErrorKind::NotFound => "memory entity not found",
            ErrorKind::Closed => "memory is closed",
            ErrorKind::InvalidCursor => "invalid memory cursor",
            ErrorKind::Canceled => "context canceled",
            ErrorKind::PolicyDecision => "memory proposal was not queued for review",
        }
    }
}

/// One memory error. `detail` is appended after the category as
/// `category: detail`, so operator-facing messages stay byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub detail: Option<String>,
    /// Set for `ErrorKind::Conflict` raised from a revision mismatch.
    pub conflict: Option<Conflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub entity_kind: String,
    pub id: String,
    pub expected_revision: u64,
    pub actual_revision: u64,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            detail: None,
            conflict: None,
        }
    }

    pub fn detailed(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
            conflict: None,
        }
    }

    pub fn conflict(entity_kind: &str, id: &str, expected: u64, actual: u64) -> Self {
        Self {
            kind: ErrorKind::Conflict,
            detail: None,
            conflict: Some(Conflict {
                entity_kind: entity_kind.to_string(),
                id: id.to_string(),
                expected_revision: expected,
                actual_revision: actual,
            }),
        }
    }

    pub fn is(&self, kind: ErrorKind) -> bool {
        self.kind == kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(formatter, "{}: {detail}", self.kind.message()),
            None => formatter.write_str(self.kind.message()),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub fn invalid_request(detail: &str) -> Error {
    Error::detailed(ErrorKind::InvalidRequest, detail)
}

pub fn sensitive(category: &str) -> Error {
    Error::detailed(ErrorKind::SensitiveMemory, category)
}

/// `validOpaqueID`: non-empty, within `max_bytes`, ASCII alphanumerics plus
/// `.`, `_`, `:` and `-`.
pub fn valid_opaque_id(id: &str, max_bytes: usize) -> bool {
    if id.is_empty() || id.len() > max_bytes {
        return false;
    }
    id.bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Human,
    Model,
    Extractor,
    Import,
    Migration,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Human => "human",
            Origin::Model => "model",
            Origin::Extractor => "extractor",
            Origin::Import => "import",
            Origin::Migration => "migration",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Origin::Human),
            "model" => Some(Origin::Model),
            "extractor" => Some(Origin::Extractor),
            "import" => Some(Origin::Import),
            "migration" => Some(Origin::Migration),
            _ => None,
        }
    }
}

/// The lifecycle state of a record (#591 §4). A forgotten record is a
/// tombstone, not a state. Only `Active` records that are not expired and
/// whose review is not due take part in normal recall and in the standing set.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    #[default]
    Active,
    /// Uncertain or awaiting a human review; listed with a label, not injected.
    NeedsReview,
    /// Replaced by the record named in `Record::superseded_by`.
    Superseded,
    /// Withdrawn or established as incorrect; `Record::state_reason` may say why.
    Retracted,
}

impl RecordState {
    pub const ALL: [RecordState; 4] = [
        RecordState::Active,
        RecordState::NeedsReview,
        RecordState::Superseded,
        RecordState::Retracted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RecordState::Active => "active",
            RecordState::NeedsReview => "needs_review",
            RecordState::Superseded => "superseded",
            RecordState::Retracted => "retracted",
        }
    }

    /// Accepts the stored name and its hyphenated command-line form.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(RecordState::Active),
            "needs_review" | "needs-review" => Some(RecordState::NeedsReview),
            "superseded" => Some(RecordState::Superseded),
            "retracted" => Some(RecordState::Retracted),
            _ => None,
        }
    }
}

/// Longest `state_reason`, in bytes.
pub const MAX_STATE_REASON_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateAction {
    Create,
    Update,
    Forget,
}

impl CandidateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateAction::Create => "create",
            CandidateAction::Update => "update",
            CandidateAction::Forget => "forget",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "create" => Some(CandidateAction::Create),
            "update" => Some(CandidateAction::Update),
            "forget" => Some(CandidateAction::Forget),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateState {
    Pending,
    Accepted,
    Rejected,
}

impl CandidateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateState::Pending => "pending",
            CandidateState::Accepted => "accepted",
            CandidateState::Rejected => "rejected",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(CandidateState::Pending),
            "accepted" => Some(CandidateState::Accepted),
            "rejected" => Some(CandidateState::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Accept,
    Reject,
}

impl ReviewDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewDecision::Accept => "accept",
            ReviewDecision::Reject => "reject",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "accept" => Some(ReviewDecision::Accept),
            "reject" => Some(ReviewDecision::Reject),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Accept,
    Pending,
    Reject,
}

impl PolicyDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyDecision::Accept => "accept",
            PolicyDecision::Pending => "pending",
            PolicyDecision::Reject => "reject",
        }
    }
}

#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Scope {
    pub namespace: String,
    pub id: String,
}

impl Scope {
    pub fn new(namespace: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            id: id.into(),
        }
    }
}

pub const PRESENTATION_ORIGIN_METADATA: &str = "otto.presentation_origin";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub origin: Option<Origin>,
    pub session_id: String,
    pub message_ids: Vec<String>,
    pub observation_id: String,
    pub decision_at: Option<DateTime<Utc>>,
    pub decision_source: Option<Origin>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub scope: Scope,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub labels: Vec<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub source: Provenance,
    pub confidence: f64,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    /// The last content update. A lifecycle write (state, verification) is a
    /// new revision but leaves this time unchanged.
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub state: RecordState,
    /// The last explicit verification by a human or an evidence-backed
    /// action. Reading, recall and listing never set it.
    pub verified_at: Option<DateTime<Utc>>,
    /// The next review deadline. Once passed, the record leaves normal recall
    /// until it is verified.
    pub review_at: Option<DateTime<Utc>>,
    /// The replacement record id when `state` is `Superseded`, else "".
    pub superseded_by: String,
    /// Why the record left the active state, else "".
    pub state_reason: String,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            id: String::new(),
            scope: Scope::default(),
            kind: String::new(),
            key: String::new(),
            text: String::new(),
            labels: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
            source: Provenance::default(),
            confidence: 0.0,
            revision: 0,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            expires_at: None,
            state: RecordState::Active,
            verified_at: None,
            review_at: None,
            superseded_by: String::new(),
            state_reason: String::new(),
        }
    }
}

impl Record {
    /// Whether the review deadline has passed at `now`.
    pub fn review_due(&self, now: DateTime<Utc>) -> bool {
        self.review_at.is_some_and(|review_at| review_at <= now)
    }

    /// Whether the record is expired at `now`.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    /// Whether normal recall and the standing set may use the record at
    /// `now`: active, not expired, review not due.
    pub fn effective(&self, now: DateTime<Utc>) -> bool {
        self.state == RecordState::Active && !self.expired(now) && !self.review_due(now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub id: String,
    pub scope: Scope,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub forgotten_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub proposed: Record,
    pub action: CandidateAction,
    pub target_id: String,
    pub base_revision: u64,
    pub reason: String,
    pub state: CandidateState,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_source: Option<Origin>,
    pub result_record_id: String,
    pub result_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    pub scope: Scope,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordKey {
    pub scope: Scope,
    pub kind: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidateRef {
    pub scope: Scope,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreIdentity {
    pub database_id: String,
    pub user_scope: Scope,
    pub schema_version: i64,
}

#[derive(Debug, Clone, Default)]
pub struct RecordPage {
    pub records: Vec<Record>,
    pub next_cursor: String,
}

#[derive(Debug, Clone, Default)]
pub struct CandidatePage {
    pub candidates: Vec<Candidate>,
    pub next_cursor: String,
}

/// `DefaultPolicy.Decide`: human and migration are accepted, model, extractor
/// and import queue for review, everything else is rejected. An invalid or
/// sensitive proposal, or one whose recorded source origin disagrees with the
/// caller origin, is always rejected.
pub fn decide_default_policy(request: &PolicyRequest) -> PolicyDecision {
    let mismatched_source = request
        .source
        .origin
        .is_some_and(|origin| Some(origin) != request.origin);
    if !request.valid || request.sensitive || mismatched_source {
        return PolicyDecision::Reject;
    }
    match request.origin {
        Some(Origin::Human) | Some(Origin::Migration) => PolicyDecision::Accept,
        Some(Origin::Model) | Some(Origin::Extractor) | Some(Origin::Import) => {
            PolicyDecision::Pending
        }
        None => PolicyDecision::Reject,
    }
}

/// 16 random bytes, lowercase hex, used for the database and user scope IDs.
pub fn new_id() -> Result<String> {
    let mut raw = [0u8; 16];
    getrandom(&mut raw)?;
    Ok(raw.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn getrandom(buffer: &mut [u8]) -> Result<()> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|_| Error::detailed(ErrorKind::Unavailable, "generate memory ID"))?;
    file.read_exact(buffer)
        .map_err(|_| Error::detailed(ErrorKind::Unavailable, "generate memory ID"))
}

/// Calls `generate` until it has `count` distinct IDs.
pub fn generate_distinct_ids(
    count: usize,
    generate: &dyn Fn() -> Result<String>,
) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::with_capacity(count);
    while ids.len() < count {
        let mut retries = 0usize;
        loop {
            let id = generate().map_err(|_| {
                Error::detailed(ErrorKind::Unavailable, "generate distinct memory ID")
            })?;
            if ids.contains(&id) {
                if retries == MAX_DUPLICATE_ID_RETRIES {
                    return Err(Error::detailed(
                        ErrorKind::Unavailable,
                        "generate distinct memory ID",
                    ));
                }
                retries += 1;
                continue;
            }
            ids.push(id);
            break;
        }
    }
    Ok(ids)
}

mod requests;

pub use requests::*;

#[cfg(test)]
mod tests;
