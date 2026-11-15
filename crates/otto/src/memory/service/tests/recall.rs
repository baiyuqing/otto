//! Tests for search, recall and standing-record budgets.

use super::*;

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
async fn over_budget_data_is_injected_oldest_first_and_counted_as_omitted() {
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
            .expect("write");
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
