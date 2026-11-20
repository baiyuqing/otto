mod evidence_tests;

use super::*;
use crate::memory::files::testsupport::open_temp;
use crate::memory::{
    CandidateRef, CandidateState, NAMESPACE_USER, NAMESPACE_WORKSPACE, RememberRequest,
    ReviewDecision, ReviewRequest, decide_default_policy,
};
use crate::tool::testutil::{MAX_OUTPUT_BYTES, run};

fn service() -> (tempfile::TempDir, Arc<Service>) {
    let (directory, store) = open_temp();
    (
        directory,
        Arc::new(Service::new(store, decide_default_policy)),
    )
}

fn scope() -> Scope {
    Scope::new(NAMESPACE_USER, "user-1")
}

fn workspace_scope() -> Scope {
    Scope::new(NAMESPACE_WORKSPACE, "ws-1")
}

fn remember_tool(service: &Arc<Service>) -> RememberTool {
    RememberTool::new(Arc::clone(service), scope(), workspace_scope())
}

fn remember_record(service: &Service, key: &str, text: &str) -> String {
    service
        .remember(&RememberRequest {
            scope: scope(),
            kind: "preference".into(),
            key: key.into(),
            text: text.into(),
            ..RememberRequest::default()
        })
        .expect("the record is writable")
        .id
}

/// The candidate ID out of `candidate %s queued for human review (...)`.
fn candidate_id(content: &str) -> String {
    content
        .strip_prefix("candidate ")
        .and_then(|rest| rest.split_once(' '))
        .map(|(id, _)| id.to_owned())
        .unwrap_or_else(|| panic!("unexpected content {content:?}"))
}

#[tokio::test]
async fn remember_records_the_session_and_exact_presentation_origin() {
    let (_directory, service) = service();
    let origin = Arc::new(otto_session::origin::TurnOrigin::default());
    origin.set(Some(
        otto_session::origin::Origin::chat("telegram", "42").unwrap(),
    ));
    let tool = RememberTool::new(Arc::clone(&service), scope(), workspace_scope())
        .with_turn_context("session-1", origin);

    let result = run(&tool, r#"{"kind":"fact","text":"blue","reason":"said so"}"#).await;
    assert!(!result.is_error, "{result:?}");
    let candidate = service
        .get_candidate(&CandidateRef {
            scope: workspace_scope(),
            id: candidate_id(&result.content),
        })
        .unwrap();
    assert_eq!(candidate.proposed.source.session_id, "session-1");
    assert_eq!(
        candidate.proposed.metadata[crate::memory::PRESENTATION_ORIGIN_METADATA],
        "chat:telegram:42"
    );
}

/// The real store mints the record IDs, so the placeholder is checked by
/// count and membership instead of by a literal string.
#[tokio::test]
async fn search_renders_one_line_per_record_and_a_bounded_placeholder() {
    let (_directory, service) = service();
    let first = remember_record(&service, "editor", "prefers vim");
    let second = remember_record(&service, "commits", "always run tests before commit");
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    let result = run(&tool, r#"{"query":""}"#).await;
    assert!(!result.is_error, "{result:?}");
    for (id, text, revision) in [
        (&first, "prefers vim", "revision=1"),
        (&second, "always run tests before commit", "revision=1"),
    ] {
        assert!(result.content.contains(id.as_str()), "{result:?}");
        assert!(result.content.contains(text), "{result:?}");
        assert!(result.content.contains(revision), "{result:?}");
    }
    assert!(
        result
            .content
            .contains("scope=user/user-1 kind=preference key=editor"),
        "{result:?}"
    );

    let persisted = result.persisted_content.expect("a bounded placeholder");
    assert!(persisted.starts_with("2 records: "), "{persisted:?}");
    assert!(
        persisted.contains(&first) && persisted.contains(&second),
        "{persisted:?}"
    );
}

#[tokio::test]
async fn search_with_no_matches_reports_zero_records() {
    let (_directory, service) = service();
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    let result = run(&tool, r#"{"query":"nothing"}"#).await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content, "no matching records");
    assert_eq!(result.persisted_content.as_deref(), Some("0 records"));
}

#[tokio::test]
async fn search_by_id_refreshes_metadata_for_a_reviewed_forget() {
    let (_directory, service) = service();
    let workspace = Scope::new("workspace", "project-1");
    let original = service
        .remember(&RememberRequest {
            scope: workspace.clone(),
            kind: "fact".into(),
            text: "old text".into(),
            ..RememberRequest::default()
        })
        .unwrap();
    let updated = service
        .remember(&RememberRequest {
            id: original.id.clone(),
            scope: workspace.clone(),
            kind: "fact".into(),
            text: "current text".into(),
            expected_revision: Some(original.revision),
            ..RememberRequest::default()
        })
        .unwrap();
    let scopes = vec![scope(), workspace.clone()];
    let search = MemorySearchTool::new(Arc::clone(&service), scopes.clone(), MAX_OUTPUT_BYTES);
    let result = run(&search, &json!({"id": original.id}).to_string()).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("current text"), "{result:?}");
    assert!(!result.content.contains("old text"), "{result:?}");
    assert_eq!(
        result.persisted_content,
        Some(format!("1 records: {}", updated.id))
    );

    let fields: Vec<_> = result.content.split_whitespace().collect();
    let (namespace, scope_id) = fields
        .iter()
        .find_map(|field| field.strip_prefix("scope="))
        .unwrap()
        .split_once('/')
        .unwrap();
    let revision: u64 = fields
        .iter()
        .find_map(|field| field.strip_prefix("revision="))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(revision, updated.revision);
    let forget = ForgetTool::new(Arc::clone(&service), scopes);
    let proposal = run(
        &forget,
        &json!({
            "id": updated.id,
            "scope_namespace": namespace,
            "scope_id": scope_id,
            "revision": revision,
        })
        .to_string(),
    )
    .await;
    assert!(!proposal.is_error, "{proposal:?}");
    let reference = RecordRef {
        scope: workspace.clone(),
        id: updated.id.clone(),
    };
    assert_eq!(service.get(&reference).unwrap().revision, revision);
    let candidate = CandidateRef {
        scope: workspace,
        id: candidate_id(&proposal.content),
    };
    assert_eq!(
        service.get_candidate(&candidate).unwrap().state,
        CandidateState::Pending
    );
    service
        .review(&ReviewRequest {
            reference: candidate,
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
        })
        .unwrap();
    assert!(service.get(&reference).unwrap_err().is(ErrorKind::NotFound));
    let after = run(&search, &json!({"id": updated.id}).to_string()).await;
    assert_eq!(after.content, "no matching records");
}

#[tokio::test]
async fn search_by_id_does_not_disclose_unbound_records() {
    let (_directory, service) = service();
    let foreign = remember_record(&service, "editor", "private preference");
    for scopes in [vec![Scope::new("user", "someone-else")], vec![]] {
        let search = MemorySearchTool::new(Arc::clone(&service), scopes, MAX_OUTPUT_BYTES);
        for id in [&foreign, "missing-record"] {
            let result = run(&search, &json!({"id": id}).to_string()).await;
            assert!(!result.is_error, "{result:?}");
            assert_eq!(result.content, "no matching records");
            assert_eq!(result.persisted_content.as_deref(), Some("0 records"));
        }
    }
}

#[tokio::test]
async fn search_by_id_rejects_invalid_or_conflicting_arguments() {
    let (_directory, service) = service();
    let search = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);
    for args in [
        json!({"id": ""}),
        json!({"id": "bad/id"}),
        json!({"id": "rec-1", "query": "editor"}),
    ] {
        let result = run(&search, &args.to_string()).await;
        assert!(result.is_error, "{args}: {result:?}");
    }
}

/// A closed service stands in for a reader that returns an error.
#[tokio::test]
async fn search_surfaces_a_service_error() {
    let (_directory, service) = service();
    service.close().expect("close");
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    for args in [r#"{"query":"x"}"#, r#"{"id":"rec-1"}"#] {
        let result = run(&tool, args).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("closed"), "{result:?}");
    }
}

#[tokio::test]
async fn remember_queues_a_create_candidate_for_human_review() {
    let (_directory, service) = service();
    let tool = remember_tool(&service);

    let result = run(&tool, r#"{"kind":"preference","text":"likes dark mode"}"#).await;
    assert!(!result.is_error, "{result:?}");
    assert!(
        result.content.contains(
            " queued for approval review (state=pending). You cannot approve it yourself."
        ),
        "{result:?}"
    );

    let candidate = service
        .get_candidate(&CandidateRef {
            scope: workspace_scope(),
            id: candidate_id(&result.content),
        })
        .expect("the candidate is stored");
    assert_eq!(candidate.action, CandidateAction::Create);
    assert_eq!(candidate.state, CandidateState::Pending);
    assert_eq!(candidate.proposed.scope, workspace_scope());
    assert_eq!(candidate.proposed.text, "likes dark mode");
    assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
}

#[tokio::test]
async fn remember_queues_an_update_candidate_when_a_target_id_is_given() {
    let (_directory, service) = service();
    let target = remember_record(&service, "editor", "prefers vim");
    let tool = remember_tool(&service);

    let arguments = format!(
        r#"{{"kind":"preference","scope":"user","text":"prefers neovim","target_id":"{target}","base_revision":1}}"#
    );
    let result = run(&tool, &arguments).await;
    assert!(!result.is_error, "{result:?}");

    let candidate = service
        .get_candidate(&CandidateRef {
            scope: scope(),
            id: candidate_id(&result.content),
        })
        .expect("the candidate is stored");
    assert_eq!(candidate.action, CandidateAction::Update);
    assert_eq!(candidate.target_id, target);
    assert_eq!(candidate.base_revision, 1);
}

#[tokio::test]
async fn remember_scope_selects_the_user_or_workspace_scope() {
    let (_directory, service) = service();
    let tool = remember_tool(&service);

    let user = run(
        &tool,
        r#"{"kind":"preference","key":"tests","text":"run tests first","scope":"user"}"#,
    )
    .await;
    assert!(!user.is_error, "{user:?}");
    let candidate = service
        .get_candidate(&CandidateRef {
            scope: scope(),
            id: candidate_id(&user.content),
        })
        .expect("the candidate is in the user scope");
    assert_eq!(candidate.proposed.scope, scope());

    let omitted = run(&tool, r#"{"kind":"fact","text":"uses cargo"}"#).await;
    assert!(!omitted.is_error, "{omitted:?}");
    let candidate = service
        .get_candidate(&CandidateRef {
            scope: workspace_scope(),
            id: candidate_id(&omitted.content),
        })
        .expect("the candidate is in the workspace scope");
    assert_eq!(candidate.proposed.scope, workspace_scope());
}

#[tokio::test]
async fn remember_rejects_an_unknown_scope_and_queues_nothing() {
    let (_directory, service) = service();
    let tool = remember_tool(&service);

    let result = run(&tool, r#"{"kind":"fact","text":"x","scope":"bogus"}"#).await;
    assert!(result.is_error, "{result:?}");
    assert!(result.content.contains("bogus"), "{result:?}");
    let listed = service
        .search(&SearchRequest {
            scopes: vec![scope(), workspace_scope()],
            include_candidates: true,
            limit: 10,
            token_budget: 1000,
            now: Utc::now(),
            ..SearchRequest::default()
        })
        .expect("search");
    assert!(listed.candidates.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn remember_surfaces_the_key_rule_for_a_user_preference() {
    let (_directory, service) = service();
    let tool = remember_tool(&service);

    let result = run(
        &tool,
        r#"{"kind":"preference","text":"run tests first","scope":"user"}"#,
    )
    .await;
    assert!(result.is_error, "{result:?}");
    assert!(result.content.contains("needs a key"), "{result:?}");
}

#[tokio::test]
async fn remember_requires_a_kind_and_a_text() {
    let (_directory, service) = service();
    let tool = remember_tool(&service);

    let result = run(&tool, r#"{"kind":"preference"}"#).await;
    assert!(result.is_error, "{result:?}");
    assert!(result.content.contains("text"), "{result:?}");
}

#[tokio::test]
async fn forget_queues_a_forget_candidate_for_human_review() {
    let (_directory, service) = service();
    let target = remember_record(&service, "editor", "prefers vim");
    let origin = Arc::new(otto_session::origin::TurnOrigin::default());
    origin.set(Some(
        otto_session::origin::Origin::chat("telegram", "c_42").unwrap(),
    ));
    let tool =
        ForgetTool::new(Arc::clone(&service), vec![scope()]).with_turn_context("session-1", origin);

    let arguments =
        format!(r#"{{"scope_namespace":"user","scope_id":"user-1","id":"{target}","revision":1}}"#);
    let result = run(&tool, &arguments).await;
    assert!(!result.is_error, "{result:?}");

    let candidate = service
        .get_candidate(&CandidateRef {
            scope: scope(),
            id: candidate_id(&result.content),
        })
        .expect("the candidate is stored");
    assert_eq!(candidate.action, CandidateAction::Forget);
    assert_eq!(candidate.target_id, target);
    assert_eq!(candidate.base_revision, 1);
    assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
    assert_eq!(candidate.proposed.source.session_id, "session-1");
    assert_eq!(
        candidate.proposed.metadata[crate::memory::PRESENTATION_ORIGIN_METADATA],
        "chat:telegram:c_42"
    );
}

#[tokio::test]
async fn forget_requires_the_identifying_fields() {
    let (_directory, service) = service();
    let tool = ForgetTool::new(service, vec![scope()]);

    let result = run(&tool, r#"{"id":"rec-1"}"#).await;
    assert!(result.is_error, "{result:?}");
}

/// The rejection must happen before the service is asked, so an unbound
/// scope leaves no candidate behind.
#[tokio::test]
async fn forget_rejects_a_scope_outside_the_bound_scopes() {
    let (_directory, service) = service();
    let tool = ForgetTool::new(Arc::clone(&service), vec![scope()]);

    let result = run(
            &tool,
            r#"{"scope_namespace":"workspace","scope_id":"someone-elses-workspace","id":"rec-1","revision":2}"#,
        )
        .await;
    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.content,
        "scope is not one of the scopes this session is bound to"
    );
}

use crate::memory::advice::{self, Advisor, Answer, Backend, Exchange, Question};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Answers every question the same way and counts its calls.
struct Counting(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Backend for Counting {
    fn model(&self) -> String {
        "gpt-test".into()
    }

    fn auxiliary(&self) -> bool {
        false
    }

    async fn ask(&self, _state: &str, _questions: &[Question], _limit: Duration) -> Exchange {
        self.0.fetch_add(1, Ordering::SeqCst);
        let answer = |choice: &str| Answer {
            choice: choice.to_owned(),
            probability: None,
        };
        Exchange {
            request: "request".into(),
            response: None,
            result: Ok(HashMap::from([
                ("scope".to_owned(), answer("all_workspaces")),
                ("durable".to_owned(), answer("yes")),
            ])),
        }
    }
}

fn advised_tool(service: &Arc<Service>) -> (RememberTool, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let advisor = Advisor::new(Box::new(Counting(Arc::clone(&calls))), None);
    (
        remember_tool(service).with_advisor(Some(Arc::new(advisor))),
        calls,
    )
}

#[tokio::test]
async fn remember_with_an_advisor_queues_the_candidate_with_an_advice_line() {
    let (_directory, service) = service();
    let (tool, calls) = advised_tool(&service);

    let result = run(&tool, r#"{"kind":"preference","text":"likes dark mode"}"#).await;
    assert!(!result.is_error, "{result:?}");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let candidate = service
        .get_candidate(&CandidateRef {
            scope: workspace_scope(),
            id: candidate_id(&result.content),
        })
        .expect("the candidate is stored");
    assert_eq!(candidate.state, CandidateState::Pending);
    assert_eq!(candidate.proposed.scope, workspace_scope());
    assert_eq!(
        advice::advice_line(&candidate.proposed).as_deref(),
        Some(
            "Advice (session model gpt-test): all workspaces (proposed: workspace) · new · durable"
        )
    );
}

#[tokio::test]
async fn remember_with_an_advisor_asks_no_model_for_a_proposal_the_service_refuses() {
    let (_directory, service) = service();
    let (tool, calls) = advised_tool(&service);

    let result = run(&tool, r#"{"kind":"preference","text":""}"#).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remember_with_an_advisor_returns_context_canceled_when_canceled_during_advice() {
    let (_directory, service) = service();
    let (tool, _) = advised_tool(&service);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = tool
        .execute(
            &RawValue::from_string(r#"{"kind":"preference","text":"likes dark mode"}"#.into())
                .expect("raw"),
            &cancel,
        )
        .await;

    assert!(result.is_error);
    assert_eq!(result.content, CONTEXT_CANCELED);
}

fn set_record_state(
    service: &Service,
    id: &str,
    revision: u64,
    state: crate::memory::RecordState,
    superseded_by: &str,
) -> crate::memory::Record {
    service
        .set_state(&crate::memory::SetStateRequest {
            reference: RecordRef {
                scope: scope(),
                id: id.into(),
            },
            expected_revision: revision,
            state,
            superseded_by: superseded_by.into(),
            reason: String::new(),
        })
        .expect("the state change is accepted")
}

#[test]
fn search_definition_states_active_only_search_and_state_in_id_lookup() {
    let definition = memory_search_definition();
    assert!(
        definition.description.contains("active records only"),
        "{}",
        definition.description
    );
    assert!(
        definition.description.contains("any state")
            && definition.description.contains("shows state"),
        "{}",
        definition.description
    );
}

#[tokio::test]
async fn search_output_line_carries_active_state() {
    let (_directory, service) = service();
    remember_record(&service, "editor", "prefers vim");
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    let result = run(&tool, r#"{"query":"vim"}"#).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains(" state=active "), "{result:?}");
    assert!(!result.content.contains("superseded_by="), "{result:?}");
}

// Needs the store implementation of `set_state`.
#[tokio::test]
async fn id_lookup_returns_non_active_records_with_state_and_replacement() {
    let (_directory, service) = service();
    let old = remember_record(&service, "editor", "prefers vim");
    let new = remember_record(&service, "editor-new", "prefers helix");
    set_record_state(
        &service,
        &old,
        1,
        crate::memory::RecordState::Superseded,
        &new,
    );
    let flagged = remember_record(&service, "shell", "prefers fish");
    set_record_state(
        &service,
        &flagged,
        1,
        crate::memory::RecordState::NeedsReview,
        "",
    );
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    let superseded = run(&tool, &format!(r#"{{"id":"{old}"}}"#)).await;
    assert!(!superseded.is_error, "{superseded:?}");
    assert!(
        superseded.content.contains(" state=superseded "),
        "{superseded:?}"
    );
    assert!(
        superseded
            .content
            .contains(&format!(" superseded_by={new} ")),
        "{superseded:?}"
    );
    let review = run(&tool, &format!(r#"{{"id":"{flagged}"}}"#)).await;
    assert!(
        review.content.contains(" state=needs_review "),
        "{review:?}"
    );
    assert!(!review.content.contains("superseded_by="), "{review:?}");
}

// Needs the store implementation of recall exclusion.
#[tokio::test]
async fn term_search_returns_only_active_records() {
    let (_directory, service) = service();
    let kept = remember_record(&service, "editor", "prefers vim");
    let old = remember_record(&service, "editor-old", "prefers vi");
    set_record_state(
        &service,
        &old,
        1,
        crate::memory::RecordState::NeedsReview,
        "",
    );
    let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

    let result = run(&tool, r#"{"query":"prefers"}"#).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains(&kept), "{result:?}");
    assert!(!result.content.contains(&old), "{result:?}");
}
