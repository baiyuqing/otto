//! The memory persistence boundary consumed by [`super::Service`].

use chrono::{DateTime, Utc};

use super::{
    Candidate, CandidateListRequest, CandidatePage, CandidateRef, ListRequest, Record, RecordKey,
    RecordPage, RecordRef, Result, RetrievalRequest, RetrievalResult, Scope, StandingUsage,
    StoreForgetRequest, StoreReviewRequest, Tombstone, UpsertRequest,
};

/// Construction inputs owned by the memory domain, independent of a backend.
pub struct MemoryOptions {
    pub busy_timeout: std::time::Duration,
    pub new_id: Box<dyn Fn() -> Result<String> + Send + Sync>,
    pub guard: Box<dyn super::guard::ContentGuard>,
}
impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            busy_timeout: std::time::Duration::from_secs(5),
            new_id: Box::new(super::new_id),
            guard: Box::new(super::guard::DefaultGuard),
        }
    }
}

/// Persists memory domain operations without exposing a backend to the service.
pub trait MemoryStore: Send + Sync {
    fn identity(&self) -> Result<super::StoreIdentity>;
    fn encryption_at_rest(&self) -> bool {
        false
    }
    fn close(&self) -> Result<()>;
    fn get(&self, reference: &RecordRef) -> Result<Record>;
    fn get_by_key(&self, key: &RecordKey) -> Result<Record>;
    fn list(&self, request: &ListRequest) -> Result<RecordPage>;
    fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone>;
    fn get_candidate(&self, reference: &CandidateRef) -> Result<Candidate>;
    fn list_candidates(&self, request: &CandidateListRequest) -> Result<CandidatePage>;
    fn retrieve(&self, request: &RetrievalRequest) -> Result<RetrievalResult>;
    fn upsert_within_standing_budget(&self, request: &UpsertRequest) -> Result<Record>;
    fn standing_usage(&self, user_scope: &Scope, now: DateTime<Utc>) -> Result<StandingUsage>;
    fn standing_records(&self, user_scope: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>>;
    fn forget(&self, request: &StoreForgetRequest) -> Result<Tombstone>;
    fn review_within_standing_budget(
        &self,
        request: &StoreReviewRequest,
    ) -> Result<super::ReviewResult>;
    fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Candidate>>;
    fn check_proposals(&self, candidates: &[Candidate]) -> Result<()>;
}
