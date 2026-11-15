//! Store, retrieval, search, remember, propose, review and recall request and result types.

use super::*;

#[derive(Debug, Clone)]
pub struct ListRequest {
    /// An explicit administrative request to enumerate every stored scope.
    /// An empty `scopes` list without this flag remains an empty result.
    pub all_scopes: bool,
    pub scopes: Vec<Scope>,
    pub kinds: Vec<String>,
    pub labels: Vec<String>,
    pub limit: usize,
    pub cursor: String,
    pub now: DateTime<Utc>,
    pub include_expired: bool,
    /// The states to list; empty lists `Active` only.
    pub states: Vec<RecordState>,
    /// Only records whose content update is at or after this time.
    pub updated_after: Option<DateTime<Utc>>,
    /// Only records verified at or after this time.
    pub verified_after: Option<DateTime<Utc>>,
    /// Only records that were never verified.
    pub unverified_only: bool,
    /// Only records whose review deadline has passed at `now`.
    pub review_due_only: bool,
}

/// Moves a record between lifecycle states by a human decision.
#[derive(Debug, Clone)]
pub struct SetStateRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub state: RecordState,
    /// The replacement record id; required for `Superseded`, empty otherwise.
    pub superseded_by: String,
    /// Why the state changes; empty when not given.
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct StoreSetStateRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub state: RecordState,
    pub superseded_by: String,
    pub reason: String,
    pub changed_at: DateTime<Utc>,
}

/// Records a human verification: `verified_at` is set to now, a
/// `NeedsReview` record returns to `Active`, and `review_at` is replaced by
/// `review_at` (cleared when `None`).
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub review_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct StoreVerifyRequest {
    pub reference: RecordRef,
    pub expected_revision: u64,
    pub review_at: Option<DateTime<Utc>>,
    pub verified_at: DateTime<Utc>,
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

/// Counts the tokens one candidate would cost. A plain function pointer keeps
/// the request `Clone` and `Debug` without an allocation.
pub(crate) type TokenEstimator = fn(&str) -> usize;

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

/// How much of the standing token budget one user scope uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandingUsage {
    /// Records in the standing set.
    pub records: usize,
    /// Tokens of the records recall injects, oldest first.
    pub tokens: usize,
    pub budget: usize,
    /// Records in the set that recall does not inject because they do not fit
    /// in `budget`. Non-zero only for data written before the budget existed.
    pub omitted: usize,
}

#[derive(Debug, Clone)]
pub struct RetrievalMatch {
    pub record: Record,
    /// True when the record is in the standing set, including a workspace
    /// record that replaced its user-scope twin by key.
    pub standing: bool,
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
    /// The record states an empty-query listing includes; empty means
    /// `Active` only. A query search always retrieves active records only.
    pub states: Vec<RecordState>,
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
            states: Vec::new(),
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
    /// The next review deadline of the remembered record, if any.
    pub review_at: Option<DateTime<Utc>>,
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
            review_at: None,
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

/// What forget did about one copy of the record's text, in the four classes
/// of #591 §6: removed, only suppressed, still present, could not be verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStatus {
    Removed,
    Suppressed,
    Present,
    Unverified,
}

impl CopyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyStatus::Removed => "removed",
            CopyStatus::Suppressed => "suppressed",
            CopyStatus::Present => "present",
            CopyStatus::Unverified => "unverified",
        }
    }
}

/// One copy of a forgotten record's text that forget looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetCleanupEntry {
    /// The copy: a path relative to the store root, or a description of a
    /// place outside the store.
    pub copy: String,
    pub status: CopyStatus,
    /// What was done, or why the status is not `Removed`. Never record text.
    pub detail: String,
}

/// What the store did for one forget: the tombstone and the copies it handled.
#[derive(Debug, Clone)]
pub struct ForgetOutcome {
    pub tombstone: Tombstone,
    pub cleanup: Vec<ForgetCleanupEntry>,
}

/// The service's forget result: the store's outcome plus the copies the
/// store cannot see (advice audit rows, session files).
#[derive(Debug, Clone)]
pub struct ForgetResult {
    pub tombstone: Tombstone,
    pub cleanup: Vec<ForgetCleanupEntry>,
}

/// Restores `revision` of a record as a new revision; `expected_revision` is
/// the current revision the caller read.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub reference: RecordRef,
    pub revision: u64,
    pub expected_revision: u64,
}

#[derive(Debug, Clone)]
pub struct StoreRestoreRequest {
    pub reference: RecordRef,
    pub revision: u64,
    pub expected_revision: u64,
    pub restored_at: DateTime<Utc>,
}

/// The metadata name a restored revision carries; its value is the decimal
/// revision number the content was taken from.
pub(crate) const RESTORED_FROM_REVISION_METADATA: &str = "otto.restored_from_revision";

/// A history file the store could not read; `file` is its path relative to
/// the store root and `reason` names the problem without file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRevision {
    pub file: String,
    pub reason: String,
}

/// The revisions of one record, ascending, the current record last.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordHistory {
    pub revisions: Vec<Record>,
    pub skipped: Vec<SkippedRevision>,
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
    /// IDs of the entries in `records` the store selected from its standing set.
    pub standing: std::collections::BTreeSet<String>,
    pub used_tokens: usize,
}

/// What one [`crate::memory::Binding`] reads and writes.
///
/// Both serve automatic extraction, which this port does not implement, so the
/// binding takes neither.
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
