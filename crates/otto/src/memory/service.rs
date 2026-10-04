//! The memory service: the single entry point the tools, the REPL commands and
//! the agent binding call.
//!
//! A [`Service`] built without a store answers every operation with the
//! category error it was constructed from, while a binding on it recalls
//! nothing instead of failing.
//!
//! Ownership and concurrency: the service owns the store and closes it in
//! [`Service::close`]. Every operation takes `&self` and serializes on the
//! store's connection mutex, so the service adds no lock of its own.
//!
//! Deliberate absences:
//!
//! - `Observe` and the per-binding content guard are absent, because
//!   automatic extraction through a binding is out of scope. The `reflection`
//!   module queues extractor candidates on demand through [`Service::propose`];
//!   the store still guards written content.
//! - The store is the concrete Turso store and the policy a function, so
//!   there is no missing-dependency case to reject.
//! - `close` waits on the store's connection mutex, which an in-flight
//!   operation already holds; a call that arrives after it sees `Closed`.
//! - The synchronous methods cannot be cancelled; only the async
//!   [`MemoryRecall`] adapter checks a token.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use otto_core::agent::memory as core_memory;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::turso::Store;
use super::validate::{
    provenance_zero, validate_bind_scopes, validate_forget_request, validate_propose_request,
    validate_recall_request, validate_remember_request, validate_search_request,
};
use super::{
    BindOptions, Candidate, CandidateAction, CandidateListRequest, CandidateState, Error,
    ErrorKind, ForgetRequest, ForgetResult, ListRequest, MAX_FTS_TERM_BYTES, MAX_FTS_TERMS,
    MAX_QUERY_BYTES, Origin, PolicyDecision, PolicyRequest, ProposeRequest, Provenance,
    RecallRequest, RecallResult, Record, RecordKey, RecordRef, RememberRequest, Result,
    RetrievalRequest, ReviewDecision, ReviewRequest, ReviewResult, Scope, SearchRequest,
    SearchResult, StandingUsage, StoreForgetRequest, StoreReviewRequest, TokenEstimator, Tombstone,
    UpsertRequest, decide_default_policy, new_id,
};

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
/// [`MAX_FTS_TERMS`] terms of [`MAX_FTS_TERM_BYTES`]. Recall is best effort,
/// so the surplus is dropped rather than failing the turn. Terms are counted
/// the way `build_fts_literal_expression` splits them, and a term over the
/// byte bound is dropped whole because truncating it would match nothing.
fn normalize_query(query: &str) -> String {
    let mut normalized = String::new();
    for term in query
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|term| !term.is_empty() && term.len() <= MAX_FTS_TERM_BYTES)
        .take(MAX_FTS_TERMS)
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
pub struct Service {
    store: Option<Store>,
    policy: Policy,
    /// What a store-less service answers operations with.
    category: ErrorKind,
    /// A content-free generation signal incremented only after a pending
    /// candidate is durably created. Subscribers must query through their own
    /// scoped review path; this signal contains no candidate data.
    review_available: watch::Sender<u64>,
    closed: AtomicBool,
}

impl Service {
    /// Composes a store and a policy: human-authorized writes land directly,
    /// model, extractor and import writes land as pending candidates through
    /// [`Service::propose`].
    pub fn new(store: Store, policy: Policy) -> Self {
        Self {
            store: Some(store),
            policy,
            category: ErrorKind::Disabled,
            review_available: watch::channel(0).0,
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
            closed: AtomicBool::new(false),
        }
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
    fn ready(&self) -> Result<&Store> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed));
        }
        self.store.as_ref().ok_or_else(|| Error::new(self.category))
    }

    pub fn get(&self, reference: &RecordRef) -> Result<Record> {
        self.ready()?.get(reference)
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

    /// Forgetting writes a tombstone. Purging backups is refused outright:
    /// this port has no backups to purge.
    pub fn forget(&self, request: &ForgetRequest) -> Result<ForgetResult> {
        let store = self.ready()?;
        validate_forget_request(request)?;
        if request.purge_backups {
            return Err(Error::new(ErrorKind::Unsupported));
        }
        let tombstone = store.forget(&StoreForgetRequest {
            reference: request.reference.clone(),
            expected_revision: request.expected_revision,
            forgotten_at: now_utc(),
        })?;
        Ok(ForgetResult { tombstone })
    }

    /// Deciding one pending candidate. A review is always a human decision, so
    /// an accepted create mints the record ID here rather than in the store.
    pub fn review(&self, request: &ReviewRequest) -> Result<ReviewResult> {
        let store = self.ready()?;
        let candidate = store.get_candidate(&request.reference)?;
        let mut store_request = StoreReviewRequest {
            reference: request.reference.clone(),
            result_record_id: String::new(),
            decision: request.decision,
            edited: request.edited.clone(),
            target_revision: request.target_revision,
            decision_source: Some(Origin::Human),
            decided_at: now_utc(),
        };
        if request.decision == ReviewDecision::Accept && candidate.action == CandidateAction::Create
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
        let Some(store) = self.service.store.as_ref() else {
            return Ok(RecallResult::default());
        };
        let query = normalize_query(&request.query);
        validate_recall_request(&RecallRequest {
            query: query.clone(),
            ..request.clone()
        })?;
        let result = store.retrieve(&RetrievalRequest {
            query,
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

    fn close(&self) -> std::result::Result<(), core_memory::MemoryError> {
        self.release();
        Ok(())
    }
}

fn core_record(record: Record, standing: bool) -> core_memory::Record {
    core_memory::Record {
        id: record.id,
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

/// A provenance that names a human author, the default for a `/remember`.
pub fn human_provenance(session_id: &str) -> Provenance {
    Provenance {
        origin: Some(Origin::Human),
        session_id: session_id.to_string(),
        ..Provenance::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::turso::testsupport::open_temp;
    use crate::memory::{CandidateRef, NAMESPACE_USER, NAMESPACE_WORKSPACE};

    fn service() -> (tempfile::TempDir, Arc<Service>) {
        let (directory, store) = open_temp();
        (
            directory,
            Arc::new(Service::new(store, decide_default_policy)),
        )
    }

    fn user_scope() -> Scope {
        Scope::new(NAMESPACE_USER, "user-1")
    }

    fn remember(scope: &Scope, key: &str, text: &str) -> RememberRequest {
        RememberRequest {
            scope: scope.clone(),
            kind: "preference".into(),
            key: key.into(),
            text: text.into(),
            ..RememberRequest::default()
        }
    }

    #[test]
    fn pending_summary_counts_pending_in_scope_and_finds_the_oldest() {
        let (_directory, service) = service();
        let user = user_scope();
        let here = Scope::new(NAMESPACE_WORKSPACE, "here");
        let elsewhere = Scope::new(NAMESPACE_WORKSPACE, "elsewhere");
        let scopes = [user.clone(), here.clone()];
        assert_eq!(
            service.pending_summary(&scopes).expect("empty"),
            PendingSummary::default()
        );
        let first = service
            .propose(&model_proposal(&user, "a", "one"))
            .expect("a")
            .remove(0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        service
            .propose(&model_proposal(&here, "b", "two"))
            .expect("b");
        service
            .propose(&model_proposal(&elsewhere, "c", "three"))
            .expect("c");
        let accepted = service
            .propose(&model_proposal(&here, "d", "four"))
            .expect("d")
            .remove(0);
        service
            .review(&ReviewRequest {
                reference: CandidateRef {
                    scope: here,
                    id: accepted.id,
                },
                decision: ReviewDecision::Accept,
                edited: None,
                target_revision: None,
            })
            .expect("accept");
        let summary = service.pending_summary(&scopes).expect("summary");
        assert_eq!(summary.count, 2);
        assert_eq!(summary.oldest, Some(first.created_at));
    }

    fn model_proposal(scope: &Scope, key: &str, text: &str) -> ProposeRequest {
        ProposeRequest {
            action: CandidateAction::Create,
            scope: scope.clone(),
            kind: "preference".into(),
            key: key.into(),
            text: text.into(),
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        }
    }

    /// A keyed remember updates in place whether or not a revision is given,
    /// and never leaves a duplicate behind.
    #[test]
    fn a_keyed_remember_creates_once_and_then_updates_in_place() {
        let (_directory, service) = service();
        let scope = user_scope();

        let created = service
            .remember(&remember(&scope, "editor", "vim"))
            .expect("create");
        assert_eq!(created.revision, 1);
        assert!(!created.id.is_empty());
        assert_eq!(created.source.origin, Some(Origin::Human));

        let conditional = service
            .remember(&RememberRequest {
                id: created.id.clone(),
                expected_revision: Some(created.revision),
                ..remember(&scope, "editor", "neovim")
            })
            .expect("conditional update");
        assert_eq!(
            (conditional.revision, conditional.text.as_str()),
            (2, "neovim")
        );
        assert_eq!(conditional.created_at, created.created_at);

        let by_key = service
            .remember(&remember(&scope, "editor", "emacs"))
            .expect("keyed update");
        assert_eq!(by_key.id, created.id);
        assert_eq!((by_key.revision, by_key.text.as_str()), (3, "emacs"));

        let listed = service
            .search(&SearchRequest {
                scopes: vec![scope],
                kinds: vec!["preference".into()],
                limit: 10,
                token_budget: 1000,
                now: Utc::now(),
                ..SearchRequest::default()
            })
            .expect("search");
        assert_eq!(
            listed.records.len(),
            1,
            "the keyed update duplicated the record"
        );
    }

    /// The guard runs inside the store, so this checks the service does not
    /// route around it.
    #[test]
    fn a_remember_carrying_a_secret_is_refused() {
        let (_directory, service) = service();
        let error = service
            .remember(&RememberRequest {
                scope: user_scope(),
                kind: "secret".into(),
                text: "api_key: sk-abcdef123456".into(),
                ..RememberRequest::default()
            })
            .expect_err("a secret must not be stored");
        assert!(error.is(ErrorKind::SensitiveMemory), "error = {error}");
    }

    #[test]
    fn forgetting_writes_a_tombstone_and_purging_backups_is_refused() {
        let (_directory, service) = service();
        let scope = user_scope();
        let created = service
            .remember(&remember(&scope, "editor", "vim"))
            .expect("create");
        let reference = RecordRef {
            scope: scope.clone(),
            id: created.id.clone(),
        };

        let purge = service
            .forget(&ForgetRequest {
                reference: reference.clone(),
                expected_revision: created.revision,
                purge_backups: true,
                confirm_purge: true,
            })
            .expect_err("purge is unsupported");
        assert!(purge.is(ErrorKind::Unsupported), "error = {purge}");

        let result = service
            .forget(&ForgetRequest {
                reference: reference.clone(),
                expected_revision: created.revision,
                purge_backups: false,
                confirm_purge: false,
            })
            .expect("forget");
        assert_eq!(result.tombstone.id, created.id);
        assert!(
            service
                .get(&reference)
                .expect_err("record is gone")
                .is(ErrorKind::NotFound)
        );
        assert_eq!(
            service.get_tombstone(&reference).expect("tombstone").id,
            created.id
        );
    }

    #[test]
    fn reading_a_record_that_was_never_written_is_not_found() {
        let (_directory, service) = service();
        let reference = RecordRef {
            scope: user_scope(),
            id: "0123456789abcdef".into(),
        };
        assert!(
            service
                .get(&reference)
                .expect_err("no record")
                .is(ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn a_successful_proposal_signals_review_availability_without_payload() {
        let (_directory, service) = service();
        let mut changed = service.review_available_changed();
        let candidates = service
            .propose(&model_proposal(
                &user_scope(),
                "editor",
                "private candidate text",
            ))
            .expect("proposal");

        changed.changed().await.expect("signal sender remains live");
        assert_eq!(*changed.borrow(), 1);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn a_proposal_is_queued_only_when_the_policy_pends() {
        let (_directory, service) = service();
        let scope = user_scope();

        let human = service
            .propose(&ProposeRequest {
                source: Provenance {
                    origin: Some(Origin::Human),
                    ..Provenance::default()
                },
                ..model_proposal(&scope, "editor", "vim")
            })
            .expect_err("a human write is not a proposal");
        assert!(human.is(ErrorKind::InvalidRequest), "error = {human}");

        for decision in [PolicyDecision::Reject, PolicyDecision::Accept] {
            let (_directory, store) = open_temp();
            let fixed: Policy = match decision {
                PolicyDecision::Reject => |_| PolicyDecision::Reject,
                _ => |_| PolicyDecision::Accept,
            };
            let service = Service::new(store, fixed);
            let error = service
                .propose(&model_proposal(&scope, "editor", "vim"))
                .expect_err("a non-pending decision never queues");
            assert!(error.is(ErrorKind::PolicyDecision), "error = {error}");
            assert_eq!(
                error.to_string(),
                format!(
                    "memory proposal was not queued for review: policy decision {:?}",
                    decision.as_str()
                )
            );
        }
    }

    #[test]
    fn an_accepted_proposal_becomes_a_record_and_a_rejected_one_does_not() {
        let (_directory, service) = service();
        let scope = user_scope();

        let queued = service
            .propose(&model_proposal(&scope, "editor", "vim"))
            .expect("propose");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].state, CandidateState::Pending);

        let accepted = service
            .review(&ReviewRequest {
                reference: CandidateRef {
                    scope: scope.clone(),
                    id: queued[0].id.clone(),
                },
                decision: ReviewDecision::Accept,
                edited: None,
                target_revision: None,
            })
            .expect("accept");
        let record = accepted.record.expect("an accepted create writes a record");
        assert_eq!(record.text, "vim");
        assert_eq!(accepted.candidate.decision_source, Some(Origin::Human));
        assert_eq!(
            service
                .get(&RecordRef {
                    scope: scope.clone(),
                    id: record.id
                })
                .expect("read")
                .text,
            "vim"
        );

        let second = service
            .propose(&model_proposal(&scope, "shell", "fish"))
            .expect("propose");
        let rejected = service
            .review(&ReviewRequest {
                reference: CandidateRef {
                    scope: scope.clone(),
                    id: second[0].id.clone(),
                },
                decision: ReviewDecision::Reject,
                edited: None,
                target_revision: None,
            })
            .expect("reject");
        assert!(
            rejected.record.is_none(),
            "a rejected candidate must write no record"
        );
        assert_eq!(rejected.candidate.state, CandidateState::Rejected);
    }

    /// An empty query lists, a text query retrieves, and candidates come back
    /// only when asked for.
    #[test]
    fn search_lists_retrieves_and_reports_candidates() {
        let (_directory, service) = service();
        let scope = user_scope();
        let created = service
            .remember(&remember(&scope, "editor", "prefers vim for editing"))
            .expect("create");
        let now = Utc::now();
        let base = SearchRequest {
            scopes: vec![scope.clone()],
            limit: 10,
            token_budget: 1000,
            now,
            ..SearchRequest::default()
        };

        let listed = service.search(&base).expect("list");
        assert_eq!(listed.records.len(), 1);
        assert_eq!(listed.records[0].id, created.id);

        let searched = service
            .search(&SearchRequest {
                query: "vim".into(),
                ..base.clone()
            })
            .expect("retrieve");
        assert_eq!(searched.records.len(), 1);
        assert_eq!(searched.records[0].id, created.id);

        assert!(
            listed.candidates.is_empty(),
            "candidates were not requested"
        );
        service
            .propose(&model_proposal(&scope, "shell", "loves emacs"))
            .expect("propose");
        let with_candidates = service
            .search(&SearchRequest {
                include_candidates: true,
                candidate_states: vec![CandidateState::Pending],
                ..base
            })
            .expect("search with candidates");
        assert_eq!(with_candidates.candidates.len(), 1);
    }

    #[tokio::test]
    async fn a_binding_recalls_the_records_in_its_scopes() {
        let (_directory, service) = service();
        let scope = user_scope();
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "workspace-1");
        let created = service
            .remember(&remember(&scope, "editor", "prefers vim for editing code"))
            .expect("create");

        let binding = service
            .bind(BindOptions {
                scopes: vec![scope, workspace.clone()],
                default_write_scope: workspace.clone(),
                ..BindOptions::default()
            })
            .expect("bind");
        assert_eq!(binding.default_write_scope(), &workspace);

        let result = binding
            .recall(&RecallRequest {
                query: "vim".into(),
                limit: 10,
                token_budget: 1000,
                ..RecallRequest::default()
            })
            .expect("recall");
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].id, created.id);

        let through_agent = core_memory::MemoryRecall::recall(
            &binding,
            &core_memory::RecallRequest {
                query: "vim".into(),
                limit: 10,
                token_budget: 1000,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("agent recall");
        assert_eq!(through_agent.records.len(), 1);
        assert_eq!(through_agent.records[0].id, created.id);
        assert_eq!(through_agent.records[0].scope.namespace, NAMESPACE_USER);
    }

    #[tokio::test]
    async fn recall_marks_only_the_baseline_records_as_standing() {
        let (_directory, service) = service();
        let user = user_scope();
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "workspace-1");
        let preference = service
            .remember(&remember(&user, "tests", "run the tests before committing"))
            .expect("create preference");
        let fact = service
            .remember(&RememberRequest {
                kind: "fact".into(),
                ..remember(&workspace, "build", "the build uses cargo")
            })
            .expect("create fact");
        let binding = service
            .bind(BindOptions {
                scopes: vec![user, workspace.clone()],
                default_write_scope: workspace,
                ..BindOptions::default()
            })
            .expect("bind");

        let result = core_memory::MemoryRecall::recall(
            &binding,
            &core_memory::RecallRequest {
                query: "cargo".into(),
                limit: 10,
                token_budget: 1000,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("agent recall");
        let standing = |id: &str| {
            result
                .records
                .iter()
                .find(|record| record.id == id)
                .unwrap_or_else(|| panic!("{id} was not recalled"))
                .standing
        };
        assert!(standing(&preference.id));
        assert!(!standing(&fact.id));
    }

    fn bind_both(service: &Arc<Service>, workspace: &Scope) -> Binding {
        service
            .bind(BindOptions {
                scopes: vec![user_scope(), workspace.clone()],
                default_write_scope: workspace.clone(),
                ..BindOptions::default()
            })
            .expect("bind")
    }

    async fn agent_recall(binding: &Binding, query: &str, limit: i64) -> Vec<core_memory::Record> {
        core_memory::MemoryRecall::recall(
            binding,
            &core_memory::RecallRequest {
                query: query.into(),
                limit,
                token_budget: 1000,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("agent recall")
        .records
    }

    #[tokio::test]
    async fn recall_injects_every_standing_record_outside_the_limit() {
        let (_directory, service) = service();
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "workspace-1");
        for index in 0..20 {
            service
                .remember(&remember(
                    &user_scope(),
                    &format!("rule-{index}"),
                    &format!("standing rule number {index}"),
                ))
                .expect("standing");
        }
        for index in 0..15 {
            service
                .remember(&RememberRequest {
                    kind: "fact".into(),
                    ..remember(
                        &workspace,
                        &format!("fact-{index}"),
                        &format!("cargo fact {index}"),
                    )
                })
                .expect("fact");
        }
        let binding = bind_both(&service, &workspace);

        let records = agent_recall(&binding, "cargo", 12).await;
        assert_eq!(records.iter().filter(|r| r.standing).count(), 20);
        assert_eq!(records.iter().filter(|r| !r.standing).count(), 12);
        assert!(records[..20].iter().all(|r| r.standing), "standing leads");
    }

    fn rejection_detail(error: &Error) -> &str {
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        let detail = error.detail.as_deref().expect("detail");
        assert!(
            detail.contains("standing preferences are full")
                && detail.contains("of 2000 tokens used")
                && detail.contains("this record needs"),
            "{detail}"
        );
        detail
    }

    #[test]
    fn remember_and_accept_are_rejected_when_the_standing_set_is_full() {
        let (_directory, service) = service();
        let scope = user_scope();
        let big = service
            .remember(&remember(&scope, "big", &"x".repeat(5500)))
            .expect("fits alone");

        let error = service
            .remember(&remember(&scope, "more", &"y".repeat(500)))
            .expect_err("full");
        rejection_detail(&error);
        // Other kinds and workspace scope are not counted.
        service
            .remember(&RememberRequest {
                kind: "fact".into(),
                ..remember(&scope, "", &"z".repeat(500))
            })
            .expect("a fact is not standing");

        let queued = service
            .propose(&model_proposal(&scope, "more", &"y".repeat(500)))
            .expect("propose is never blocked");
        let review = |service: &Service| {
            service.review(&ReviewRequest {
                reference: CandidateRef {
                    scope: scope.clone(),
                    id: queued[0].id.clone(),
                },
                decision: ReviewDecision::Accept,
                edited: None,
                target_revision: None,
            })
        };
        rejection_detail(&review(&service).expect_err("full"));
        let candidate = service
            .get_candidate(&CandidateRef {
                scope: scope.clone(),
                id: queued[0].id.clone(),
            })
            .expect("candidate");
        assert_eq!(candidate.state, CandidateState::Pending);

        service
            .forget(&ForgetRequest {
                reference: RecordRef {
                    scope: scope.clone(),
                    id: big.id,
                },
                expected_revision: big.revision,
                purge_backups: false,
                confirm_purge: false,
            })
            .expect("forget");
        review(&service).expect("accepts once space is free");
    }

    #[test]
    fn growing_a_standing_record_does_not_count_its_old_version() {
        let (_directory, service) = service();
        let scope = user_scope();
        service
            .remember(&remember(&scope, "big", &"x".repeat(5500)))
            .expect("create");
        service
            .remember(&remember(&scope, "big", &"x".repeat(5700)))
            .expect("grows within the budget");
        let error = service
            .remember(&remember(&scope, "big", &"x".repeat(6400)))
            .expect_err("grows past the budget");
        rejection_detail(&error);
    }

    #[tokio::test]
    async fn legacy_over_budget_data_is_injected_oldest_first_and_counted_as_omitted() {
        use crate::memory::turso::records::testsupport::{at, sample_record};
        let (_directory, service) = service();
        let scope = user_scope();
        let store = service.store.as_ref().expect("store");
        let mut costs = Vec::new();
        // Inserted newest first, so only created_at can put "a" first.
        for (id, second) in [("c", 3), ("b", 2), ("a", 1)] {
            let mut record = sample_record(id, &scope, id, &id.repeat(2400));
            record.created_at = at(second);
            record.updated_at = at(second);
            costs.push(crate::memory::turso::retriever::record_cost(None, &record));
            store
                .upsert(&UpsertRequest {
                    record,
                    expected_revision: None,
                })
                .expect("legacy write");
        }
        let workspace = Scope::new(NAMESPACE_WORKSPACE, "workspace-1");
        let binding = bind_both(&service, &workspace);

        let records = agent_recall(&binding, "unmatched", 12).await;
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(records.iter().all(|r| r.standing));

        let usage = service.standing_usage(&scope).expect("usage");
        assert_eq!(usage.records, 3);
        assert_eq!(usage.omitted, 1);
        assert_eq!(usage.tokens, costs[2] + costs[1]);
        assert_eq!(usage.budget, 2000);
    }

    /// Turn text is raw user input: multi-line, untrimmed and unbounded,
    /// while the store's query is single-line, trimmed and bounded. The
    /// binding normalizes instead of failing the recall.
    #[tokio::test]
    async fn a_binding_normalizes_raw_turn_text_into_a_query() {
        let (_directory, service) = service();
        let scope = user_scope();
        let created = service
            .remember(&remember(&scope, "editor", "prefers vim for editing code"))
            .expect("create");
        let binding = service
            .bind(BindOptions {
                scopes: vec![scope.clone()],
                default_write_scope: scope,
                ..BindOptions::default()
            })
            .expect("bind");

        let query = format!("  vim\n{}\n", "padding ".repeat(2000));
        let result = binding
            .recall(&RecallRequest {
                query,
                limit: 10,
                token_budget: 1000,
                ..RecallRequest::default()
            })
            .expect("recall");
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].id, created.id);
    }

    #[test]
    fn closing_the_service_invalidates_it_and_every_binding() {
        let (_directory, service) = service();
        let scope = user_scope();
        let binding = service
            .bind(BindOptions {
                scopes: vec![scope.clone()],
                default_write_scope: scope.clone(),
                ..BindOptions::default()
            })
            .expect("bind");

        service.close().expect("close");
        service.close().expect("a second close is a no-op");

        assert!(
            service
                .remember(&remember(&scope, "editor", "vim"))
                .expect_err("closed")
                .is(ErrorKind::Closed)
        );
        assert!(
            binding
                .recall(&RecallRequest::default())
                .expect_err("closed")
                .is(ErrorKind::Closed)
        );
        assert!(
            service
                .bind(BindOptions {
                    scopes: vec![scope.clone()],
                    default_write_scope: scope,
                    ..BindOptions::default()
                })
                .expect_err("closed")
                .is(ErrorKind::Closed)
        );
    }

    /// A null service answers with a safe category, but its binding still
    /// recalls nothing rather than failing.
    #[test]
    fn a_null_service_answers_with_its_category_and_recalls_nothing() {
        let scope = user_scope();
        for (reason, want) in [
            (None, ErrorKind::Disabled),
            (Some(ErrorKind::Disabled), ErrorKind::Disabled),
            (Some(ErrorKind::Unavailable), ErrorKind::Unavailable),
            (Some(ErrorKind::Corrupt), ErrorKind::Unavailable),
        ] {
            let service = Arc::new(Service::null(reason));
            assert!(service.is_null());
            let error = service
                .get(&RecordRef {
                    scope: scope.clone(),
                    id: "abc".into(),
                })
                .expect_err("null");
            assert!(error.is(want), "reason {reason:?} gave {error}");
            assert!(
                service
                    .remember(&remember(&scope, "editor", "vim"))
                    .expect_err("null")
                    .is(want)
            );

            let binding = service
                .bind(BindOptions {
                    scopes: vec![scope.clone()],
                    default_write_scope: scope.clone(),
                    ..BindOptions::default()
                })
                .expect("a null service still binds");
            assert_eq!(
                binding
                    .recall(&RecallRequest::default())
                    .expect("null recall")
                    .records
                    .len(),
                0
            );

            service.close().expect("close");
            assert!(
                service
                    .get(&RecordRef {
                        scope: scope.clone(),
                        id: "abc".into()
                    })
                    .expect_err("closed")
                    .is(ErrorKind::Closed)
            );
            assert!(
                binding
                    .recall(&RecallRequest::default())
                    .expect_err("closed")
                    .is(ErrorKind::Closed)
            );
        }
    }

    /// Binding still validates its scopes, on the null service as on a real
    /// one.
    #[test]
    fn binding_rejects_a_write_scope_it_does_not_read() {
        let service = Arc::new(Service::null(None));
        let error = service
            .bind(BindOptions {
                scopes: vec![user_scope()],
                default_write_scope: Scope::new(NAMESPACE_WORKSPACE, "workspace-1"),
                ..BindOptions::default()
            })
            .expect_err("the write scope is not readable");
        assert!(error.is(ErrorKind::InvalidRequest), "error = {error}");
    }
}
