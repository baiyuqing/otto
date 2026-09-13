//! Memory domain contracts ported from Go `internal/memory` (`types.go`,
//! `limits.go`, `errors.go`, `id.go`).
//!
//! The types stay value-only so the store, the service, and the tools can all
//! agree on one shape. Every size ceiling is the Go constant with the same
//! name; the SQLite store depends on them to build its defensive projections,
//! so they must not drift.

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
pub const MAX_FTS_TERMS: usize = 64;
pub const MAX_FTS_TERM_BYTES: usize = 256;
pub const MAX_BASELINE_RECORDS: usize = 16;
pub const MAX_RETRIEVAL_CANDIDATES: usize = 256;
pub const MAX_CANDIDATE_SCAN: usize = 500;
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

/// Sentinel category. Mirrors the `Err*` package variables in Go; `errors.Is`
/// becomes a `kind` comparison here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Disabled,
    Unavailable,
    Conflict,
    SensitiveMemory,
    Unsupported,
    MemoryInUse,
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
    IncompleteForget,
    Canceled,
    /// Go's `PolicyDecisionError`. It carries the decision as its detail.
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
            ErrorKind::MemoryInUse => "memory is in use",
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
            ErrorKind::IncompleteForget => {
                "memory was forgotten but tombstone recording is incomplete"
            }
            ErrorKind::Canceled => "context canceled",
            ErrorKind::PolicyDecision => "memory proposal was not queued for review",
        }
    }
}

/// One memory error. `detail` reproduces Go's `fmt.Errorf("%w: detail", ...)`
/// suffix so operator-facing messages stay byte-identical.
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

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
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
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRef {
    pub scope: Scope,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreIdentity {
    pub database_id: String,
    pub user_scope: Scope,
    pub schema_version: i64,
    pub generation: u64,
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

#[derive(Debug, Clone)]
pub struct ListRequest {
    pub scopes: Vec<Scope>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub limit: usize,
    pub cursor: String,
    pub now: DateTime<Utc>,
    pub include_expired: bool,
}

#[derive(Debug, Clone)]
pub struct CandidateListRequest {
    pub scopes: Vec<Scope>,
    pub states: Vec<CandidateState>,
    pub limit: usize,
    pub cursor: String,
}

#[derive(Debug, Clone)]
pub struct UpsertRequest {
    pub record: Record,
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct StoreForgetRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub forgotten_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoreReviewRequest {
    pub reference: CandidateRef,
    pub result_record_id: String,
    pub decision: ReviewDecision,
    pub edited: Option<Record>,
    pub target_revision: Option<u64>,
    pub decision_source: Option<Origin>,
    pub decided_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub candidate: Candidate,
    pub record: Option<Record>,
    pub tombstone: Option<Tombstone>,
}

/// Counts the tokens one candidate would cost. Go carries a `func(string) int`
/// on the request; a plain function pointer keeps the request `Clone` and
/// `Debug` without an allocation.
pub type TokenEstimator = fn(&str) -> usize;

#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    pub query: String,
    pub scopes: Vec<Scope>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub include_expired: bool,
    pub include_baseline: bool,
    pub limit: usize,
    pub token_budget: usize,
    pub cursor: String,
    pub now: DateTime<Utc>,
    pub estimate_tokens: Option<TokenEstimator>,
}

#[derive(Debug, Clone)]
pub struct RetrievalMatch {
    pub record: Record,
    pub rank: usize,
}

#[derive(Debug, Clone, Default)]
pub struct RetrievalResult {
    pub matches: Vec<RetrievalMatch>,
    pub used_tokens: usize,
    pub next_cursor: String,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub scopes: Vec<Scope>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub include_expired: bool,
    pub include_candidates: bool,
    pub candidate_states: Vec<CandidateState>,
    pub limit: usize,
    pub token_budget: usize,
    pub cursor: String,
    pub now: DateTime<Utc>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scopes: Vec::new(),
            kinds: Vec::new(),
            labels: Vec::new(),
            include_expired: false,
            include_candidates: false,
            candidate_states: Vec::new(),
            limit: 0,
            token_budget: 0,
            cursor: String::new(),
            now: DateTime::UNIX_EPOCH,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub records: Vec<Record>,
    pub candidates: Vec<Candidate>,
    pub next_cursor: String,
}

#[derive(Debug, Clone)]
pub struct RememberRequest {
    pub id: String,
    pub scope: Scope,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub labels: Vec<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub confidence: f64,
    pub expires_at: Option<DateTime<Utc>>,
    pub expected_revision: Option<u64>,
    pub source: Provenance,
}

impl Default for RememberRequest {
    fn default() -> Self {
        Self {
            id: String::new(),
            scope: Scope::default(),
            kind: String::new(),
            key: String::new(),
            text: String::new(),
            labels: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
            confidence: 0.0,
            expires_at: None,
            expected_revision: None,
            source: Provenance::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ForgetRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub purge_backups: bool,
    pub confirm_purge: bool,
}

#[derive(Debug, Clone)]
pub struct ForgetResult {
    pub tombstone: Tombstone,
}

#[derive(Debug, Clone)]
pub struct ProposeRequest {
    pub action: CandidateAction,
    pub scope: Scope,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub labels: Vec<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub confidence: f64,
    pub expires_at: Option<DateTime<Utc>>,
    pub target_id: String,
    pub base_revision: u64,
    pub reason: String,
    pub source: Provenance,
}

impl Default for ProposeRequest {
    fn default() -> Self {
        Self {
            action: CandidateAction::Create,
            scope: Scope::default(),
            kind: String::new(),
            key: String::new(),
            text: String::new(),
            labels: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
            confidence: 0.0,
            expires_at: None,
            target_id: String::new(),
            base_revision: 0,
            reason: String::new(),
            source: Provenance::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub reference: CandidateRef,
    pub decision: ReviewDecision,
    pub edited: Option<Record>,
    pub target_revision: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RecallRequest {
    pub query: String,
    pub kinds: Vec<String>,
    pub limit: usize,
    pub token_budget: usize,
}

#[derive(Debug, Clone, Default)]
pub struct RecallResult {
    pub records: Vec<Record>,
    pub used_tokens: usize,
}

/// What one [`crate::memory::Binding`] reads and writes.
///
/// Go also carries an `Extractor` and a `ContentGuard` here. Both serve
/// automatic extraction, which this port does not implement, so the binding
/// takes neither.
#[derive(Debug, Clone, Default)]
pub struct BindOptions {
    pub scopes: Vec<Scope>,
    pub default_write_scope: Scope,
    pub estimate_tokens: Option<TokenEstimator>,
    pub now: Option<fn() -> DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct PolicyRequest {
    pub origin: Option<Origin>,
    pub action: CandidateAction,
    pub scope: Scope,
    pub kind: String,
    pub confidence: f64,
    pub source: Provenance,
    pub valid: bool,
    pub sensitive: bool,
}

#[derive(Debug, Clone)]
pub struct GuardField {
    pub name: String,
    pub value: String,
    pub opaque: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GuardInput {
    pub fields: Vec<GuardField>,
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

/// 16 random bytes, lowercase hex. The SQLite bootstrap rejects anything else
/// for the database and user scope IDs.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_matches_go_wrapping() {
        assert_eq!(
            Error::new(ErrorKind::Disabled).to_string(),
            "memory is disabled"
        );
        assert_eq!(
            invalid_request("installation ID").to_string(),
            "invalid memory request: installation ID"
        );
    }

    #[test]
    fn opaque_ids_accept_the_go_alphabet() {
        assert!(valid_opaque_id("abc.DEF_09:-", MAX_ID_BYTES));
        assert!(!valid_opaque_id("", MAX_ID_BYTES));
        assert!(!valid_opaque_id("has space", MAX_ID_BYTES));
        assert!(!valid_opaque_id(
            &"a".repeat(MAX_ID_BYTES + 1),
            MAX_ID_BYTES
        ));
    }

    #[test]
    fn default_policy_matches_go_rules() {
        let base = PolicyRequest {
            origin: Some(Origin::Human),
            action: CandidateAction::Create,
            scope: Scope::new(NAMESPACE_USER, "install"),
            kind: "preference".into(),
            confidence: 1.0,
            source: Provenance::default(),
            valid: true,
            sensitive: false,
        };
        assert_eq!(decide_default_policy(&base), PolicyDecision::Accept);

        let mut migration = base.clone();
        migration.origin = Some(Origin::Migration);
        assert_eq!(decide_default_policy(&migration), PolicyDecision::Accept);

        for origin in [Origin::Model, Origin::Extractor, Origin::Import] {
            let mut pending = base.clone();
            pending.origin = Some(origin);
            assert_eq!(decide_default_policy(&pending), PolicyDecision::Pending);
        }

        let mut invalid = base.clone();
        invalid.valid = false;
        assert_eq!(decide_default_policy(&invalid), PolicyDecision::Reject);

        let mut sensitive_request = base.clone();
        sensitive_request.sensitive = true;
        assert_eq!(
            decide_default_policy(&sensitive_request),
            PolicyDecision::Reject
        );

        let mut mismatched = base.clone();
        mismatched.source.origin = Some(Origin::Model);
        assert_eq!(decide_default_policy(&mismatched), PolicyDecision::Reject);

        let mut unset = base;
        unset.origin = None;
        assert_eq!(decide_default_policy(&unset), PolicyDecision::Reject);
    }

    #[test]
    fn new_id_is_thirty_two_hex_characters() {
        let id = new_id().expect("id");
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn distinct_ids_retry_past_duplicates() {
        let counter = std::cell::Cell::new(0usize);
        let generate = || {
            let index = counter.get();
            counter.set(index + 1);
            Ok(format!("id{}", index / 2))
        };
        let ids = generate_distinct_ids(3, &generate).expect("ids");
        assert_eq!(ids, vec!["id0", "id1", "id2"]);
    }
}
