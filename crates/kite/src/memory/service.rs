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
//! - `Observe`, the extractor and the per-binding content guard are absent,
//!   because automatic memory extraction is out of scope. The store still
//!   guards written content.
//! - The store is the concrete SQLite store and the policy a function, so
//!   there is no missing-dependency case to reject.
//! - `close` waits on the store's connection mutex, which an in-flight
//!   operation already holds; a call that arrives after it sees `Closed`.
//! - The synchronous methods cannot be cancelled; only the async
//!   [`MemoryRecall`] adapter checks a token.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use kite_core::agent::memory as core_memory;
use tokio_util::sync::CancellationToken;

use super::sqlite::Store;
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
    SearchResult, StoreForgetRequest, StoreReviewRequest, TokenEstimator, Tombstone, UpsertRequest,
    decide_default_policy, new_id,
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
            closed: AtomicBool::new(false),
        }
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

    pub fn get_tombstone(&self, reference: &RecordRef) -> Result<Tombstone> {
        self.ready()?.get_tombstone(reference)
    }

    pub fn get_candidate(&self, reference: &super::CandidateRef) -> Result<Candidate> {
        self.ready()?.get_candidate(reference)
    }

    /// An empty query lists, a non-empty one retrieves. Candidates are read
    /// only when the caller asked for them, and they page on the same cursor.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        let store = self.ready()?;
        validate_search_request(request)?;

        let (records, next_cursor) = if request.query.is_empty() {
            let page = store.list(&ListRequest {
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
                        return store.upsert(&UpsertRequest {
                            record,
                            expected_revision: Some(existing.revision),
                        });
                    }
                    Err(error) if error.is(ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            let record = compose(new_id()?, 0, now);
            return store.upsert(&UpsertRequest {
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
        store.upsert(&UpsertRequest {
            record,
            expected_revision: Some(expected),
        })
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
        store.review(&store_request)
    }

    /// The non-human write. It only ever queues a pending candidate, so a
    /// policy that accepts or rejects outright is an error rather than a
    /// silently empty batch.
    pub fn propose(&self, request: &ProposeRequest) -> Result<Vec<Candidate>> {
        let store = self.ready()?;
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
        let candidate = Candidate {
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
        };
        store.propose(std::slice::from_ref(&candidate))
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
        Ok(RecallResult {
            records: result
                .matches
                .into_iter()
                .map(|entry| entry.record)
                .collect(),
            used_tokens: result.used_tokens,
        })
    }

    /// Releases the binding. The shared store stays open.
    pub fn release(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// The agent seam. The agent knows only the neutral record shape in
/// `kite_core`, so the conversion lives here rather than in the agent.
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
            records: result.records.into_iter().map(core_record).collect(),
            used_tokens: result.used_tokens as i64,
        })
    }

    fn close(&self) -> std::result::Result<(), core_memory::MemoryError> {
        self.release();
        Ok(())
    }
}

fn core_record(record: Record) -> core_memory::Record {
    core_memory::Record {
        id: record.id,
        scope: core_memory::Scope {
            namespace: record.scope.namespace,
            id: record.scope.id,
        },
        kind: record.kind,
        key: record.key,
        text: record.text,
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
    use crate::memory::sqlite::testsupport::open_temp;
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
