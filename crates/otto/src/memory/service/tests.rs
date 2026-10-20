
use super::*;
use crate::memory::files::testsupport::open_temp;
use crate::memory::{CandidateRef, NAMESPACE_USER, NAMESPACE_WORKSPACE, Provenance};

fn service() -> (tempfile::TempDir, Arc<Service>) {
    let (directory, store) = open_temp();
    (
        directory,
        Arc::new(Service::new(store, decide_default_policy)),
    )
}

struct AlternateStore(crate::memory::files::FileStore);

fn unsupported<T>() -> Result<T> {
    Err(Error::new(ErrorKind::Unsupported))
}

impl MemoryStore for AlternateStore {
    fn identity(&self) -> Result<crate::memory::StoreIdentity> {
        self.0.identity()
    }
    fn close(&self) -> Result<()> {
        self.0.close()
    }
    fn get(&self, reference: &RecordRef) -> Result<Record> {
        self.0.get(reference)
    }
    fn get_by_key(&self, _: &RecordKey) -> Result<Record> {
        unsupported()
    }
    fn list(&self, _: &ListRequest) -> Result<super::super::RecordPage> {
        unsupported()
    }
    fn get_tombstone(&self, _: &RecordRef) -> Result<Tombstone> {
        unsupported()
    }
    fn get_candidate(&self, _: &super::super::CandidateRef) -> Result<Candidate> {
        unsupported()
    }
    fn list_candidates(&self, _: &CandidateListRequest) -> Result<super::super::CandidatePage> {
        unsupported()
    }
    fn retrieve(&self, _: &RetrievalRequest) -> Result<super::super::RetrievalResult> {
        unsupported()
    }
    fn upsert_within_standing_budget(&self, _: &UpsertRequest) -> Result<Record> {
        unsupported()
    }
    fn standing_usage(&self, _: &Scope, _: DateTime<Utc>) -> Result<StandingUsage> {
        unsupported()
    }
    fn standing_records(&self, _: &Scope, _: DateTime<Utc>) -> Result<Vec<Record>> {
        unsupported()
    }
    fn forget(&self, _: &StoreForgetRequest) -> Result<Tombstone> {
        unsupported()
    }
    fn review_within_standing_budget(&self, _: &StoreReviewRequest) -> Result<ReviewResult> {
        unsupported()
    }
    fn propose(&self, _: &[Candidate]) -> Result<Vec<Candidate>> {
        unsupported()
    }
    fn check_proposals(&self, _: &[Candidate]) -> Result<()> {
        unsupported()
    }
}

#[test]
fn service_uses_injected_store_and_preserves_not_found_error() {
    let (_directory, store) = open_temp();
    let service = Service::from_store(Arc::new(AlternateStore(store)), decide_default_policy);

    let error = service
        .get(&RecordRef {
            scope: user_scope(),
            id: "missing".into(),
        })
        .expect_err("missing record");
    assert!(error.is(ErrorKind::NotFound));
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

#[test]
fn recall_ignores_the_message_text_and_puts_workspace_records_first() {
    let (_directory, service) = service();
    let user = user_scope();
    let workspace = Scope::new(NAMESPACE_WORKSPACE, "workspace-1");
    let fact = |scope: &Scope, key: &str, text: &str| RememberRequest {
        kind: "fact".into(),
        ..remember(scope, key, text)
    };
    let user_fact = service
        .remember(&fact(&user, "editor", "prefers vim for editing code"))
        .expect("user fact");
    let workspace_fact = service
        .remember(&fact(&workspace, "build", "run make before committing"))
        .expect("workspace fact");
    let binding = service
        .bind(BindOptions {
            scopes: vec![user, workspace.clone()],
            default_write_scope: workspace,
            ..BindOptions::default()
        })
        .expect("bind");

    let result = binding
        .recall(&RecallRequest {
            query: "unrelated question about weather".into(),
            limit: 10,
            token_budget: 1000,
            ..RecallRequest::default()
        })
        .expect("recall");
    let ids: Vec<&str> = result.records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, [workspace_fact.id.as_str(), user_fact.id.as_str()]);
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
    use crate::memory::files::testsupport::{at, sample_record};
    let (_directory, store) = open_temp();
    let store = Arc::new(store);
    let service = Arc::new(Service::from_store(store.clone(), decide_default_policy));
    let scope = user_scope();
    let mut costs = Vec::new();
    // Inserted newest first, so only created_at can put "a" first.
    for (id, second) in [("c", 3), ("b", 2), ("a", 1)] {
        let mut record = sample_record(id, &scope, id, &id.repeat(2400));
        record.created_at = at(second);
        record.updated_at = at(second);
        costs.push(crate::memory::files::record_cost(None, &record));
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
