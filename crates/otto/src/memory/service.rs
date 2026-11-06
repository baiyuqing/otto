//! The memory service: the single entry point the tools, the REPL commands and
//! the agent binding call.
//!
//! A [`Service`] built without a store answers every operation with the
//! category error it was constructed from, while a binding on it recalls
//! nothing instead of failing.
//!
//! The service owns a repository port and closes it in [`Service::close`].
//! Backend concurrency is enforced by the repository implementation.
//!
//! Deliberate absences:
//!
//! - `Observe` and the per-binding content guard are absent, because
//!   automatic extraction through a binding is out of scope.
//!   The store still guards written content.
//! - The policy is a function, so it cannot fail.
//! - `close` rejects later service operations; each adapter owns its in-flight
//!   operation lifecycle.
//! - The synchronous methods cannot be cancelled; only the async
//!   [`MemoryRecall`] adapter checks a token.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use otto_core::agent::memory as core_memory;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::MemoryStore;
use super::validate::{
    action_writes_new_record, provenance_zero, validate_bind_scopes, validate_forget_request,
    validate_propose_request, validate_recall_request, validate_record_ref,
    validate_remember_request, validate_restore_request, validate_search_request,
    validate_set_state_request, validate_verify_request,
};
use super::{
    BindOptions, Candidate, CandidateListRequest, CandidateState, CopyStatus, Error, ErrorKind,
    ForgetCleanupEntry, ForgetRequest, ForgetResult, ListRequest, MAX_QUERY_BYTES,
    MAX_QUERY_TERM_BYTES, MAX_QUERY_TERMS, Origin, PolicyDecision, PolicyRequest, ProposeRequest,
    RecallRequest, RecallResult, Record, RecordHistory, RecordKey, RecordRef, RecordState,
    RememberRequest, RestoreRequest, Result, RetrievalRequest, ReviewDecision, ReviewRequest,
    ReviewResult, Scope, SearchRequest, SearchResult, SetStateRequest, StandingUsage,
    StoreForgetRequest, StoreRestoreRequest, StoreReviewRequest, StoreSetStateRequest,
    StoreVerifyRequest, TokenEstimator, Tombstone, UpsertRequest, VerifyRequest,
    decide_default_policy, new_id,
};

/// The copies of a forgotten record's text that live outside the store and
/// that the service does not scan. Reported with every forget.
fn cleanup_outside_the_store() -> Vec<ForgetCleanupEntry> {
    vec![
        ForgetCleanupEntry {
            copy: "memory advice audit log (~/.otto/memory-advice.db)".into(),
            status: CopyStatus::Unverified,
            detail: "rows are keyed by candidate id, not record id, and were not scanned; \
                     an advice request may hold the record text"
                .into(),
        },
        ForgetCleanupEntry {
            copy: "session files".into(),
            status: CopyStatus::Present,
            detail: "recalled text is never written to a session; user input and tool \
                     output that repeat it are independent history and are not deleted"
                .into(),
        },
    ]
}

fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// The one implementation is [`decide_default_policy`], so a function replaces
/// the interface and cannot fail.
pub type Policy = fn(&PolicyRequest) -> PolicyDecision;

/// Turns one turn's user text into a query the store accepts.
///
/// The caller's text is whatever the user typed: multiple lines, leading and
/// trailing space, and no length bound, while a query must be trimmed,
/// control-character free, at most [`MAX_QUERY_BYTES`] and at most
/// [`MAX_QUERY_TERMS`] terms of [`MAX_QUERY_TERM_BYTES`]. Recall is best effort,
/// so the surplus is dropped rather than failing the turn. Terms are counted
/// the way retrieval splits them (on whitespace), and a term over the
/// byte bound is dropped whole because truncating it would match nothing.
fn normalize_query(query: &str) -> String {
    let mut normalized = String::new();
    for term in query
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|term| !term.is_empty() && term.len() <= MAX_QUERY_TERM_BYTES)
        .take(MAX_QUERY_TERMS)
    {
        let separator = usize::from(!normalized.is_empty());
        if normalized.len() + separator + term.len() > MAX_QUERY_BYTES {
            break;
        }
        if separator == 1 {
            normalized.push(' ');
        }
        normalized.push_str(term);
    }
    normalized
}

/// The working memory service, or the no-resource stand-in for disabled or
/// unavailable memory when built with [`Service::null`].
type ReviewPublisher = Arc<dyn Fn(&[Candidate]) -> Result<()> + Send + Sync>;

pub struct Service {
    store: Option<Arc<dyn MemoryStore>>,
    policy: Policy,
    /// What a store-less service answers operations with.
    category: ErrorKind,
    /// A content-free generation signal incremented only after a pending
    /// candidate is durably created. Subscribers must query through their own
    /// scoped review path; this signal contains no candidate data.
    review_available: watch::Sender<u64>,
    publishers: std::sync::Mutex<std::collections::BTreeMap<String, ReviewPublisher>>,
    closed: AtomicBool,
}

impl Service {
    /// Composes a store and a policy: human-authorized writes land directly,
    /// model, extractor and import writes land as pending candidates through
    /// [`Service::propose`].
    pub fn new(store: impl MemoryStore + 'static, policy: Policy) -> Self {
        Self::from_store(Arc::new(store), policy)
    }

    /// Builds a service over a shared repository selected by runtime wiring.
    pub fn from_store(store: Arc<dyn MemoryStore>, policy: Policy) -> Self {
        Self {
            store: Some(store),
            policy,
            category: ErrorKind::Disabled,
            review_available: watch::channel(0).0,
            publishers: std::sync::Mutex::new(Default::default()),
            closed: AtomicBool::new(false),
        }
    }

    /// The reason is reduced to a safe public category: nothing and `Disabled`
    /// stay `Disabled`, everything else becomes `Unavailable`, so an internal
    /// failure never leaks through the category.
    pub fn null(reason: Option<ErrorKind>) -> Self {
        let category = match reason {
            None | Some(ErrorKind::Disabled) => ErrorKind::Disabled,
            Some(_) => ErrorKind::Unavailable,
        };
        Self {
            store: None,
            policy: decide_default_policy,
            category,
            review_available: watch::channel(0).0,
            publishers: std::sync::Mutex::new(Default::default()),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_review_publisher(&self, workspace: &str, publisher: ReviewPublisher) {
        self.publishers
            .lock()
            .expect("memory publishers")
            .insert(workspace.into(), publisher);
    }

    /// A content-free signal that changes after a pending candidate was
    /// durably created. The receiver has no candidate id, text, reason, or
    /// scope; consumers must use their existing scoped review query.
    pub fn review_available_changed(&self) -> watch::Receiver<u64> {
        self.review_available.subscribe()
    }

    /// Reports whether this service can reach a store at all. A null service
    /// still binds and still answers, it just answers with its category.
    pub fn is_null(&self) -> bool {
        self.store.is_none()
    }

    /// The operation error and the store lookup in one: the closed check wins
    /// over the category.
    fn ready(&self) -> Result<&dyn MemoryStore> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed));
        }
        self.store
            .as_deref()
            .ok_or_else(|| Error::new(self.category))
    }

    pub fn get(&self, reference: &RecordRef) -> Result<Record> {
        self.ready()?.get(reference)
    }

    /// The stable identity of this service's repository.
    pub fn identity(&self) -> Result<super::StoreIdentity> {
        self.ready()?.identity()
    }

    pub fn encryption_at_rest(&self) -> Result<bool> {
        Ok(self.ready()?.encryption_at_rest())
    }

    pub fn get_by_key(&self, key: &RecordKey) -> Result<Record> {
        self.ready()?.get_by_key(key)
    }

    /// Lists active records with a stable, generation-bound cursor. `all_scopes`
    /// is explicit so an accidentally empty scope set cannot disclose the store.
    pub fn list(&self, request: &ListRequest) -> Result<super::RecordPage> {
        self.ready()?.list(request)
    }

    pub fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
        self.ready()?.get_tombstone(reference)
    }

    pub fn get_candidate(&self, reference: &super::CandidateRef) -> Result<Candidate> {
        self.ready()?.get_candidate(reference)
    }

    /// One page of candidates in the given states, newest first, with the
    /// cursor of the next page ("" at the end).
    pub fn list_candidates(&self, request: &CandidateListRequest) -> Result<super::CandidatePage> {
        self.ready()?.list_candidates(request)
    }

    /// An empty query lists, a non-empty one retrieves. Candidates are read
    /// only when the caller asked for them, and they page on the same cursor.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        let store = self.ready()?;
        validate_search_request(request)?;

        let (records, next_cursor) = if request.query.is_empty() {
            let page = store.list(&ListRequest {
                all_scopes: false,
                scopes: request.scopes.clone(),
                kinds: request.kinds.clone(),
                labels: request.labels.clone(),
                limit: request.limit,
                cursor: request.cursor.clone(),
                now: request.now,
                include_expired: request.include_expired,
                states: request.states.clone(),
                updated_after: None,
                verified_after: None,
                unverified_only: false,
                review_due_only: false,
            })?;
            (page.records, page.next_cursor)
        } else {
            let result = store.retrieve(&RetrievalRequest {
                query: request.query.clone(),
                scopes: request.scopes.clone(),
                kinds: request.kinds.clone(),
                labels: request.labels.clone(),
                include_expired: request.include_expired,
                include_baseline: false,
                limit: request.limit,
                token_budget: request.token_budget,
                cursor: request.cursor.clone(),
                now: request.now,
                estimate_tokens: None,
            })?;
            let records = result
                .matches
                .into_iter()
                .map(|entry| entry.record)
                .collect();
            (records, result.next_cursor)
        };

        let candidates = if request.include_candidates {
            store
                .list_candidates(&CandidateListRequest {
                    scopes: request.scopes.clone(),
                    states: request.candidate_states.clone(),
                    limit: request.limit,
                    cursor: request.cursor.clone(),
                })?
                .candidates
        } else {
            Vec::new()
        };

        Ok(SearchResult {
            records,
            candidates,
            next_cursor,
        })
    }

    /// The human-authorized write. Without an expected revision a keyed
    /// request updates the record that key already names, or creates one; with
    /// an expected revision the write is conditional on it.
    pub fn remember(&self, request: &RememberRequest) -> Result<Record> {
        let store = self.ready()?;
        validate_remember_request(request)?;
        let mut source = request.source.clone();
        if provenance_zero(&source) {
            source.origin = Some(Origin::Human);
        }
        let now = now_utc();
        let compose = |id: String, revision: u64, created_at: DateTime<Utc>| Record {
            id,
            scope: request.scope.clone(),
            kind: request.kind.clone(),
            key: request.key.clone(),
            text: request.text.clone(),
            labels: request.labels.clone(),
            metadata: request.metadata.clone(),
            source: source.clone(),
            confidence: request.confidence,
            revision,
            created_at,
            updated_at: now,
            expires_at: request.expires_at,
            state: RecordState::Active,
            verified_at: None,
            review_at: request.review_at,
            superseded_by: String::new(),
            state_reason: String::new(),
        };

        let Some(expected) = request.expected_revision else {
            if !request.key.is_empty() {
                let key = RecordKey {
                    scope: request.scope.clone(),
                    kind: request.kind.clone(),
                    key: request.key.clone(),
                };
                match store.get_by_key(&key) {
                    Ok(existing) => {
                        let record = compose(existing.id, existing.revision, existing.created_at);
                        return store.upsert_within_standing_budget(&UpsertRequest {
                            record,
                            expected_revision: Some(existing.revision),
                        });
                    }
                    Err(error) if error.is(ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            let record = compose(new_id()?, 0, now);
            return store.upsert_within_standing_budget(&UpsertRequest {
                record,
                expected_revision: None,
            });
        };

        let existing = if request.id.is_empty() {
            store.get_by_key(&RecordKey {
                scope: request.scope.clone(),
                kind: request.kind.clone(),
                key: request.key.clone(),
            })?
        } else {
            store.get(&RecordRef {
                scope: request.scope.clone(),
                id: request.id.clone(),
            })?
        };
        let record = compose(existing.id, expected, existing.created_at);
        store.upsert_within_standing_budget(&UpsertRequest {
            record,
            expected_revision: Some(expected),
        })
    }

    /// How much of the standing token budget `user_scope` uses, counted the
    /// way recall injects the set.
    pub fn standing_usage(&self, user_scope: &Scope) -> Result<StandingUsage> {
        self.ready()?.standing_usage(user_scope, now_utc())
    }

    /// The standing records of `user_scope`, oldest first.
    pub fn standing_records(&self, user_scope: &Scope) -> Result<Vec<Record>> {
        self.ready()?.standing_records(user_scope, now_utc())
    }

    /// Forgetting writes a tombstone and removes the text copies the store
    /// holds; the result lists every copy with what happened to it, including
    /// the places outside the store the service does not scan. Purging
    /// backups is refused outright: this port has no backups to purge.
    pub fn forget(&self, request: &ForgetRequest) -> Result<ForgetResult> {
        let store = self.ready()?;
        validate_forget_request(request)?;
        if request.purge_backups {
            return Err(Error::new(ErrorKind::Unsupported));
        }
        let outcome = store.forget(&StoreForgetRequest {
            reference: request.reference.clone(),
            expected_revision: request.expected_revision,
            forgotten_at: now_utc(),
        })?;
        let mut cleanup = outcome.cleanup;
        cleanup.extend(cleanup_outside_the_store());
        Ok(ForgetResult {
            tombstone: outcome.tombstone,
            cleanup,
        })
    }

    /// The revisions of an active record, ascending, the current one last.
    pub fn history(&self, reference: &RecordRef) -> Result<RecordHistory> {
        let store = self.ready()?;
        validate_record_ref(reference)?;
        store.history(reference)
    }

    /// Moves a record between lifecycle states by a human decision. The
    /// content fields and `updated_at` are unchanged; the write is a new
    /// revision.
    pub fn set_state(&self, request: &SetStateRequest) -> Result<Record> {
        let store = self.ready()?;
        validate_set_state_request(request)?;
        store.set_state(&StoreSetStateRequest {
            reference: request.reference.clone(),
            expected_revision: request.expected_revision,
            state: request.state,
            superseded_by: request.superseded_by.clone(),
            reason: request.reason.clone(),
            changed_at: now_utc(),
        })
    }

    /// Records a human verification: `verified_at` becomes now, a record in
    /// `NeedsReview` returns to `Active`, and `review_at` is replaced.
    pub fn verify(&self, request: &VerifyRequest) -> Result<Record> {
        let store = self.ready()?;
        validate_verify_request(request)?;
        store.verify(&StoreVerifyRequest {
            reference: request.reference.clone(),
            expected_revision: request.expected_revision,
            review_at: request.review_at,
            verified_at: now_utc(),
        })
    }

    /// Restores an earlier revision as a new revision. A restore is a human
    /// action: it never goes through the candidate queue.
    pub fn restore(&self, request: &RestoreRequest) -> Result<Record> {
        let store = self.ready()?;
        validate_restore_request(request)?;
        store.restore(&StoreRestoreRequest {
            reference: request.reference.clone(),
            revision: request.revision,
            expected_revision: request.expected_revision,
            restored_at: now_utc(),
        })
    }

    /// Deciding one pending candidate. A review is always a human decision, so
    /// an accepted create mints the record ID here rather than in the store.
    pub fn review(&self, request: &ReviewRequest) -> Result<ReviewResult> {
        let store = self.ready()?;
        let candidate = store.get_candidate(&request.reference)?;
        let mut edited = request.edited.clone();
        if request.decision == ReviewDecision::Accept
            && candidate
                .proposed
                .metadata
                .contains_key(super::PRESENTATION_ORIGIN_METADATA)
        {
            let record = edited.get_or_insert_with(|| candidate.proposed.clone());
            record.metadata.remove(super::PRESENTATION_ORIGIN_METADATA);
        }
        let mut store_request = StoreReviewRequest {
            reference: request.reference.clone(),
            result_record_id: String::new(),
            decision: request.decision,
            edited,
            target_revision: request.target_revision,
            decision_source: Some(Origin::Human),
            decided_at: now_utc(),
        };
        if request.decision == ReviewDecision::Accept && action_writes_new_record(candidate.action)
        {
            store_request.result_record_id = new_id()?;
        }
        store.review_within_standing_budget(&store_request)
    }

    /// The non-human write. It only ever queues a pending candidate, so a
    /// policy that accepts or rejects outright is an error rather than a
    /// silently empty batch.
    pub fn propose(&self, request: &ProposeRequest) -> Result<Vec<Candidate>> {
        let store = self.ready()?;
        let candidate = self.checked_candidate(request)?;
        let candidates = store.propose(std::slice::from_ref(&candidate))?;
        if !candidates.is_empty() {
            self.review_available
                .send_modify(|generation| *generation += 1);
        }
        let publishers: Vec<_> = self
            .publishers
            .lock()
            .expect("memory publishers")
            .values()
            .cloned()
            .collect();
        for publish in publishers {
            publish(&candidates)?;
        }
        Ok(candidates)
    }

    /// Runs the validation, policy and content-guard checks [`Service::propose`]
    /// runs, and writes nothing. A caller that spends something before
    /// proposing (a model call) uses it to stop early on a request that would
    /// be refused.
    pub fn check_proposal(&self, request: &ProposeRequest) -> Result<()> {
        let store = self.ready()?;
        let candidate = self.checked_candidate(request)?;
        store.check_proposals(std::slice::from_ref(&candidate))
    }

    fn checked_candidate(&self, request: &ProposeRequest) -> Result<Candidate> {
        validate_propose_request(request)?;
        let decision = (self.policy)(&PolicyRequest {
            origin: request.source.origin,
            action: request.action,
            scope: request.scope.clone(),
            kind: request.kind.clone(),
            confidence: request.confidence,
            source: request.source.clone(),
            valid: true,
            sensitive: false,
        });
        if decision != PolicyDecision::Pending {
            return Err(Error::detailed(
                ErrorKind::PolicyDecision,
                format!("policy decision {:?}", decision.as_str()),
            ));
        }
        Ok(Candidate {
            id: new_id()?,
            proposed: Record {
                scope: request.scope.clone(),
                kind: request.kind.clone(),
                key: request.key.clone(),
                text: request.text.clone(),
                labels: request.labels.clone(),
                metadata: request.metadata.clone(),
                confidence: request.confidence,
                expires_at: request.expires_at,
                source: request.source.clone(),
                ..Record::default()
            },
            action: request.action,
            target_id: request.target_id.clone(),
            base_revision: request.base_revision,
            reason: request.reason.clone(),
            state: CandidateState::Pending,
            created_at: now_utc(),
            decided_at: None,
            decision_source: None,
            result_record_id: String::new(),
            result_revision: 0,
        })
    }

    /// Opens one scoped view for a session. The scopes are fixed for the life
    /// of the binding, which is what keeps a workspace session from reading
    /// another workspace's records.
    pub fn bind(self: &Arc<Self>, options: BindOptions) -> Result<Binding> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed));
        }
        validate_bind_scopes(&options.scopes, &options.default_write_scope)?;
        Ok(Binding {
            service: Arc::clone(self),
            scopes: options.scopes,
            default_write_scope: options.default_write_scope,
            estimate_tokens: options.estimate_tokens,
            now: options.now.unwrap_or(now_utc),
            closed: AtomicBool::new(false),
        })
    }

    /// Closes the store. Repeated calls are no-ops.
    pub fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match &self.store {
            Some(store) => store.close(),
            None => Ok(()),
        }
    }

    /// The pending candidates in `scopes`: how many, and when the oldest was
    /// created (`None` when there are none). Pages through every page.
    pub fn pending_summary(&self, scopes: &[Scope]) -> Result<PendingSummary> {
        let mut summary = PendingSummary::default();
        let mut cursor = String::new();
        loop {
            let page = self.list_candidates(&CandidateListRequest {
                scopes: scopes.to_vec(),
                states: vec![CandidateState::Pending],
                limit: super::MAX_PAGE_SIZE,
                cursor,
            })?;
            summary.count += page.candidates.len();
            for candidate in &page.candidates {
                if summary
                    .oldest
                    .is_none_or(|oldest| candidate.created_at < oldest)
                {
                    summary.oldest = Some(candidate.created_at);
                }
            }
            if page.next_cursor.is_empty() {
                return Ok(summary);
            }
            cursor = page.next_cursor;
        }
    }
}

/// What [`Service::pending_summary`] reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingSummary {
    pub count: usize,
    pub oldest: Option<DateTime<Utc>>,
}

/// One session's scoped view of memory.
pub struct Binding {
    service: Arc<Service>,
    scopes: Vec<Scope>,
    default_write_scope: Scope,
    estimate_tokens: Option<TokenEstimator>,
    now: fn() -> DateTime<Utc>,
    closed: AtomicBool,
}

impl std::fmt::Debug for Binding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Binding")
            .field("scopes", &self.scopes)
            .field("default_write_scope", &self.default_write_scope)
            .field("closed", &self.closed)
            .finish()
    }
}

impl Binding {
    /// The scopes this binding reads, in priority order.
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    /// Where this binding's writes land when the caller names no scope.
    pub fn default_write_scope(&self) -> &Scope {
        &self.default_write_scope
    }

    /// The service behind this binding, for the tools that write as well as
    /// recall.
    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) || self.service.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed));
        }
        Ok(())
    }

    /// The turn-scoped read. A null service recalls nothing rather than
    /// failing, so a disabled memory never fails a turn.
    pub fn recall(&self, request: &RecallRequest) -> Result<RecallResult> {
        self.check_open()?;
        let Some(store) = self.service.store.as_deref() else {
            return Ok(RecallResult::default());
        };
        validate_recall_request(&RecallRequest {
            query: normalize_query(&request.query),
            ..request.clone()
        })?;
        // Recall does not match on the message text; the store lists every
        // in-scope record, workspace first and newest first, within the budget.
        let result = store.retrieve(&RetrievalRequest {
            query: String::new(),
            scopes: self.scopes.clone(),
            kinds: request.kinds.clone(),
            labels: Vec::new(),
            include_expired: false,
            include_baseline: true,
            limit: request.limit,
            token_budget: request.token_budget,
            cursor: String::new(),
            now: (self.now)(),
            estimate_tokens: self.estimate_tokens,
        })?;
        let standing = result
            .matches
            .iter()
            .filter(|entry| entry.standing)
            .map(|entry| entry.record.id.clone())
            .collect();
        Ok(RecallResult {
            records: result
                .matches
                .into_iter()
                .map(|entry| entry.record)
                .collect(),
            standing,
            used_tokens: result.used_tokens,
        })
    }

    /// Releases the binding. The shared store stays open.
    pub fn release(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// The agent seam. The agent knows only the neutral record shape in
/// `otto_core`, so the conversion lives here rather than in the agent.
#[async_trait::async_trait]
impl core_memory::MemoryRecall for Binding {
    async fn recall(
        &self,
        request: &core_memory::RecallRequest,
        cancel: &CancellationToken,
    ) -> std::result::Result<core_memory::RecallResult, core_memory::MemoryError> {
        if cancel.is_cancelled() {
            return Err(core_memory::MemoryError(
                ErrorKind::Canceled.message().into(),
            ));
        }
        let result = Binding::recall(
            self,
            &RecallRequest {
                query: request.query.clone(),
                kinds: Vec::new(),
                limit: request.limit.max(0) as usize,
                token_budget: request.token_budget.max(0) as usize,
            },
        )
        .map_err(|error| core_memory::MemoryError(error.to_string()))?;
        Ok(core_memory::RecallResult {
            records: result
                .records
                .into_iter()
                .map(|record| {
                    let standing = result.standing.contains(&record.id);
                    core_record(record, standing)
                })
                .collect(),
            used_tokens: result.used_tokens as i64,
        })
    }

    async fn lifecycle(
        &self,
        ids: &[String],
        cancel: &CancellationToken,
    ) -> std::result::Result<Vec<core_memory::RecordLifecycle>, core_memory::MemoryError> {
        if cancel.is_cancelled() {
            return Err(core_memory::MemoryError(
                ErrorKind::Canceled.message().into(),
            ));
        }
        self.record_lifecycles(ids)
            .map_err(binding_lifecycle::memory_error)
    }

    fn close(&self) -> std::result::Result<(), core_memory::MemoryError> {
        self.release();
        Ok(())
    }
}

fn core_record(record: Record, standing: bool) -> core_memory::Record {
    core_memory::Record {
        id: record.id,
        revision: Some(record.revision.to_string()),
        scope: core_memory::Scope {
            namespace: record.scope.namespace,
            id: record.scope.id,
        },
        kind: record.kind,
        key: record.key,
        text: record.text,
        standing,
    }
}

mod binding_lifecycle;

#[cfg(test)]
mod tests;
