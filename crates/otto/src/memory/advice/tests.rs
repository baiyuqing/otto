use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::faults::http;
use crate::memory::turso::testsupport::open_temp;
use crate::memory::{Origin, Provenance, RememberRequest, decide_default_policy};
use crate::storage::backends::turso::memory_advice::Store as TursoAuditStore;

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

fn workspace_scope() -> Scope {
    Scope::new(NAMESPACE_WORKSPACE, "ws-1")
}

fn standing(service: &Service, key: &str, text: &str) {
    service
        .remember(&RememberRequest {
            scope: user_scope(),
            kind: "preference".into(),
            key: key.into(),
            text: text.into(),
            ..RememberRequest::default()
        })
        .expect("standing record");
}

fn request(scope: Scope, key: &str, text: &str) -> ProposeRequest {
    ProposeRequest {
        scope,
        kind: "preference".into(),
        key: key.into(),
        text: text.into(),
        reason: "the user said so".into(),
        confidence: 0.9,
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    }
}

fn answer(choice: &str, probability: Option<f64>) -> Answer {
    Answer {
        choice: choice.to_owned(),
        probability,
    }
}

fn answers(scope: &str, relation: Option<&str>, durable: &str) -> HashMap<String, Answer> {
    let mut map = HashMap::from([
        (SCOPE.to_owned(), answer(scope, None)),
        (DURABLE.to_owned(), answer(durable, None)),
    ]);
    if let Some(relation) = relation {
        map.insert(RELATION.to_owned(), answer(relation, None));
    }
    map
}

/// A backend that returns one canned exchange and counts its calls.
struct Fake {
    exchange: Mutex<Exchange>,
    calls: Arc<AtomicUsize>,
    states: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(
        result: std::result::Result<HashMap<String, Answer>, Failure>,
    ) -> (Box<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let fake = Box::new(Self {
            exchange: Mutex::new(Exchange {
                request: "the request".into(),
                response: Some("the response".into()),
                result,
            }),
            calls: Arc::clone(&calls),
            states: Arc::default(),
        });
        (fake, calls)
    }
}

#[async_trait::async_trait]
impl Backend for Fake {
    fn model(&self) -> String {
        "gpt-test".into()
    }

    fn auxiliary(&self) -> bool {
        false
    }

    async fn ask(&self, state: &str, _questions: &[Question], _limit: Duration) -> Exchange {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.states.lock().expect("lock").push(state.to_owned());
        self.exchange.lock().expect("lock").clone()
    }
}

fn audited(backend: Box<dyn Backend>) -> (Advisor, Arc<TursoAuditStore>) {
    let audit = Arc::new(TursoAuditStore::open_in_memory().expect("audit"));
    (
        Advisor::new(
            backend,
            Some(Arc::clone(&audit) as Arc<dyn audit::AuditStore>),
        ),
        audit,
    )
}

async fn propose_one(
    advisor: &Advisor,
    service: &Service,
    request: ProposeRequest,
) -> Result<Vec<Candidate>> {
    advisor
        .propose(
            service,
            &user_scope(),
            vec![request],
            &CancellationToken::new(),
        )
        .await
        .expect("not canceled")
        .pop()
        .expect("one result")
}

fn metadata_keys(record: &Record) -> Vec<&str> {
    record
        .metadata
        .keys()
        .filter(|key| key.starts_with("advice."))
        .map(String::as_str)
        .collect()
}

#[tokio::test]
async fn valid_answers_are_stored_as_advice_metadata_and_rendered() {
    let (_directory, service) = service();
    standing(&service, "plan-first", "Plan before editing.");
    let (fake, calls) = Fake::new(Ok(answers(
        ALL_WORKSPACES,
        Some("duplicate:plan-first"),
        NO,
    )));
    let (advisor, audit) = audited(fake);

    let candidates = propose_one(
        &advisor,
        &service,
        request(workspace_scope(), "plan", "Plan first."),
    )
    .await
    .expect("queued");

    let proposed = &candidates[0].proposed;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        metadata_keys(proposed),
        [
            "advice.auxiliary",
            "advice.durable",
            "advice.model",
            "advice.relation",
            "advice.relation_key",
            "advice.scope",
        ]
    );
    assert_eq!(proposed.metadata["advice.relation_key"], "plan-first");
    assert_eq!(
        advice_line(proposed).as_deref(),
        Some(
            "Advice (session model gpt-test): all workspaces (proposed: workspace) · duplicates plan-first · one-off request"
        )
    );
    let rows = audit.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "ok");
    assert_eq!(
        rows[0].candidate_id.as_deref(),
        Some(candidates[0].id.as_str())
    );
    assert_eq!(rows[0].scope, NAMESPACE_WORKSPACE);
    assert_eq!(rows[0].request, "the request");
    assert_eq!(rows[0].response.as_deref(), Some("the response"));
}

#[tokio::test]
async fn the_backend_sees_the_candidate_and_the_user_standing_records() {
    let (_directory, service) = service();
    standing(&service, "plan-first", "Plan before editing.");
    let (fake, _) = Fake::new(Ok(answers(THIS_WORKSPACE, Some(NEW), YES)));
    let states = Arc::clone(&fake.states);
    let (advisor, _) = audited(fake);

    propose_one(
        &advisor,
        &service,
        request(workspace_scope(), "build", "Build with make."),
    )
    .await
    .expect("queued");

    let states = states.lock().expect("lock");
    for expected in [
        "kind: preference",
        "key: build",
        "scope: workspace",
        "text: Build with make.",
        "reason (evidence quote): the user said so",
        "key: plan-first text: Plan before editing.",
    ] {
        assert!(states[0].contains(expected), "{expected} in {}", states[0]);
    }
}

#[tokio::test]
async fn an_invalid_error_or_timed_out_exchange_queues_the_candidate_without_advice() {
    for (failure, outcome) in [
        (Failure::Invalid("not json".into()), "invalid"),
        (Failure::Error("503".into()), "error"),
        (Failure::Timeout, "timeout"),
    ] {
        let (_directory, service) = service();
        let (fake, calls) = Fake::new(Err(failure));
        let (advisor, audit) = audited(fake);

        let candidates = propose_one(
            &advisor,
            &service,
            request(user_scope(), "plan", "Plan first."),
        )
        .await
        .expect("queued");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(metadata_keys(&candidates[0].proposed).is_empty());
        assert_eq!(advice_line(&candidates[0].proposed), None);
        let rows = audit.rows();
        assert_eq!(rows[0].outcome, outcome);
        assert_eq!(
            rows[0].candidate_id.as_deref(),
            Some(candidates[0].id.as_str())
        );
    }
}

#[tokio::test]
async fn an_answer_set_missing_a_question_is_invalid() {
    let (_directory, service) = service();
    let (fake, _) = Fake::new(Ok(HashMap::from([(
        SCOPE.to_owned(),
        answer(ALL_WORKSPACES, None),
    )])));
    let (advisor, audit) = audited(fake);

    let candidates = propose_one(
        &advisor,
        &service,
        request(user_scope(), "plan", "Plan first."),
    )
    .await
    .expect("queued");

    assert!(metadata_keys(&candidates[0].proposed).is_empty());
    assert_eq!(audit.rows()[0].outcome, "invalid");
}

#[tokio::test]
async fn a_proposal_the_service_refuses_reaches_no_model_and_queues_nothing() {
    let (_directory, service) = service();
    let (fake, calls) = Fake::new(Ok(answers(ALL_WORKSPACES, None, YES)));
    let (advisor, audit) = audited(fake);

    let result = propose_one(&advisor, &service, request(user_scope(), "plan", "")).await;

    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(audit.rows().is_empty());
    let pending = service
        .list_candidates(&crate::memory::CandidateListRequest {
            scopes: vec![user_scope()],
            states: vec![crate::memory::CandidateState::Pending],
            limit: 10,
            cursor: String::new(),
        })
        .expect("list");
    assert!(pending.candidates.is_empty());
}

#[tokio::test]
async fn a_forget_request_reaches_no_model() {
    let (_directory, service) = service();
    standing(&service, "plan-first", "Plan before editing.");
    let record = service
        .standing_records(&user_scope())
        .expect("standing")
        .remove(0);
    let (fake, calls) = Fake::new(Ok(answers(ALL_WORKSPACES, None, YES)));
    let (advisor, _) = audited(fake);

    let forget = ProposeRequest {
        action: CandidateAction::Forget,
        scope: user_scope(),
        target_id: record.id,
        base_revision: record.revision,
        source: Provenance {
            origin: Some(Origin::Model),
            ..Provenance::default()
        },
        ..ProposeRequest::default()
    };
    propose_one(&advisor, &service, forget)
        .await
        .expect("queued");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_canceled_run_queues_nothing() {
    let (_directory, service) = service();
    let (fake, _) = Fake::new(Ok(answers(ALL_WORKSPACES, None, YES)));
    let (advisor, audit) = audited(fake);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = advisor
        .propose(
            &service,
            &user_scope(),
            vec![request(user_scope(), "plan", "Plan first.")],
            &cancel,
        )
        .await;

    assert!(outcome.is_none());
    assert!(audit.rows().is_empty());
}

#[tokio::test]
async fn an_audit_log_that_cannot_write_does_not_fail_the_proposal() {
    let (_directory, service) = service();
    let (fake, _) = Fake::new(Ok(answers(ALL_WORKSPACES, None, YES)));
    let audit = Arc::new(TursoAuditStore::open_in_memory().expect("audit"));
    audit.break_for_test();
    let advisor = Advisor::new(fake, Some(audit as Arc<dyn audit::AuditStore>));

    let candidates = propose_one(
        &advisor,
        &service,
        request(user_scope(), "plan", "Plan first."),
    )
    .await
    .expect("queued");

    assert!(advice_line(&candidates[0].proposed).is_some());
}

#[tokio::test]
async fn the_longest_advice_fits_the_metadata_limits() {
    let (_directory, service) = service();
    let (fake, _) = Fake::new(Ok(answers(ALL_WORKSPACES, None, YES)));
    let (advisor, _) = audited(fake);
    let metadata = Advice {
        model: "m".repeat(500),
        auxiliary: true,
        scope: ScopeAdvice::User,
        scope_p: Some(0.5),
        relation: Relation::Conflict("k".repeat(60)),
        relation_p: Some(0.5),
        durable: true,
        durable_p: Some(0.5),
    };
    let mut asked = request(user_scope(), "plan", "Plan first.");
    asked.metadata.extend(metadata.metadata());
    let _ = &advisor;

    let candidates = service.propose(&asked).expect("within limits");

    assert!(advice_line(&candidates[0].proposed).is_some());
}

#[test]
fn advice_line_shows_percentages_and_the_proposed_scope_for_the_auxiliary_model() {
    let advice = Advice {
        model: "jev-latest".into(),
        auxiliary: true,
        scope: ScopeAdvice::User,
        scope_p: Some(0.91),
        relation: Relation::New,
        relation_p: Some(0.84),
        durable: true,
        durable_p: Some(0.77),
    };
    let mut record = Record {
        scope: workspace_scope(),
        metadata: advice.metadata(),
        ..Record::default()
    };
    assert_eq!(
        advice_line(&record).as_deref(),
        Some(
            "Advice (auxiliary model jev-latest): all workspaces 91% (proposed: workspace) · new 84% · durable 77%"
        )
    );
    record.scope = user_scope();
    assert_eq!(
        advice_line(&record).as_deref(),
        Some("Advice (auxiliary model jev-latest): all workspaces 91% · new 84% · durable 77%")
    );
}

#[test]
fn advice_line_words_a_conflict_and_a_workspace_scope() {
    let advice = Advice {
        model: "gpt-test".into(),
        auxiliary: false,
        scope: ScopeAdvice::Workspace,
        scope_p: None,
        relation: Relation::Conflict("tabs".into()),
        relation_p: None,
        durable: true,
        durable_p: None,
    };
    let record = Record {
        scope: workspace_scope(),
        metadata: advice.metadata(),
        ..Record::default()
    };
    assert_eq!(
        advice_line(&record).as_deref(),
        Some("Advice (session model gpt-test): this workspace · conflicts with tabs · durable")
    );
}

#[test]
fn advice_line_is_none_without_advice_or_with_malformed_advice() {
    let mut record = Record::default();
    assert_eq!(advice_line(&record), None);
    record.metadata.insert("advice.model".into(), "x".into());
    assert_eq!(advice_line(&record), None);
    record.metadata.insert("reflection.run".into(), "r".into());
    assert_eq!(advice_line(&record), None);
}

#[test]
fn no_relation_question_is_asked_without_standing_records() {
    let none: Vec<&Record> = Vec::new();
    let ids: Vec<&str> = questions(&none)
        .iter()
        .map(|question| question.id)
        .collect();
    assert_eq!(ids, [SCOPE, DURABLE]);
    let record = Record {
        key: "plan-first".into(),
        ..Record::default()
    };
    let ids: Vec<&str> = questions(&[&record])
        .iter()
        .map(|question| question.id)
        .collect();
    assert_eq!(ids, [SCOPE, RELATION, DURABLE]);
}

#[test]
fn audit_store_records_user_version_one_and_rejects_another() {
    let directory = tempfile::tempdir().expect("dir");
    let path = directory.path().join("memory-advice.db");
    TursoAuditStore::open(&path).expect("open");
    let connection = crate::storage::Connection::open(&path).expect("raw");
    let version: i64 = connection
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .expect("version");
    assert_eq!(version, 1);
    connection
        .pragma_update("user_version", 2_i64)
        .expect("set version");
    drop(connection);
    assert_eq!(
        TursoAuditStore::open(&path).err(),
        Some(audit::Error::UnknownVersion)
    );
}

// ---------------------------------------------------------------------------
// The Jev backend against a loopback server.
// ---------------------------------------------------------------------------

/// Answers every request with `reply`, or never when `reply` is `None`.
async fn serve(reply: Option<String>) -> http::Server {
    let script = match reply {
        Some(reply) => http::Script::status(200, &reply),
        None => http::Script::stall_before_head(),
    };
    http::spawn_repeating(script).await
}

const API_KEY: &str = "tsk-secret-key-123";

fn jev_reply() -> String {
    serde_json::json!({
        "model": "jev-latest",
        "answers": {
            "scope": {"type": "choice", "choice": "all_workspaces", "confidence": 0.91},
            "relation": {"type": "choice", "choice": "new", "confidence": 0.84},
            "durable": {"type": "noul", "noul": 0.77},
        },
    })
    .to_string()
}

#[tokio::test]
async fn jev_advice_carries_percentages_and_the_audit_row_never_holds_the_key() {
    let server = serve(Some(jev_reply())).await;
    let (_directory, service) = service();
    standing(&service, "plan-first", "Plan before editing.");
    let (advisor, audit) = audited(Box::new(jev::Jev::new(&server.url, API_KEY)));
    assert!(advisor.is_auxiliary());

    let candidates = propose_one(
        &advisor,
        &service,
        request(user_scope(), "build", "Build with make."),
    )
    .await
    .expect("queued");

    assert_eq!(
        advice_line(&candidates[0].proposed).as_deref(),
        Some("Advice (auxiliary model jev-latest): all workspaces 91% · new 84% · durable 77%")
    );
    let requests = server.requests();
    let (head, body) = (&requests[0].head, &requests[0].body);
    assert!(head.contains(&format!("Bearer {API_KEY}")), "{head}");
    assert!(!body.contains(API_KEY));
    assert!(body.contains("plan-first"), "standing key in the body");
    assert!(
        body.contains("Build with make."),
        "candidate text in the body"
    );
    let rows = audit.rows();
    assert_eq!(rows[0].outcome, "ok");
    assert!(rows[0].auxiliary);
    assert_eq!(
        &rows[0].request, body,
        "the audit row holds the body as sent"
    );
    assert!(!rows[0].request.contains(API_KEY));
    assert!(!rows[0].request.to_lowercase().contains("authorization"));
    assert_eq!(rows[0].response.as_deref(), Some(jev_reply().as_str()));
}

#[tokio::test]
async fn a_low_noul_answers_no_with_the_complement_probability() {
    let reply = serde_json::json!({
        "model": "jev-latest",
        "answers": {
            "scope": {"type": "choice", "choice": "this_workspace", "confidence": 0.6},
            "durable": {"type": "noul", "noul": 0.2},
        },
    })
    .to_string();
    let server = serve(Some(reply)).await;
    let (_directory, service) = service();
    let (advisor, _) = audited(Box::new(jev::Jev::new(&server.url, API_KEY)));

    let candidates = propose_one(
        &advisor,
        &service,
        request(workspace_scope(), "build", "Build with make."),
    )
    .await
    .expect("queued");

    assert_eq!(
        advice_line(&candidates[0].proposed).as_deref(),
        Some("Advice (auxiliary model jev-latest): this workspace 60% · new · one-off request 80%")
    );
}

#[tokio::test]
async fn a_jev_server_that_does_not_answer_in_time_times_out_without_advice() {
    let server = serve(None).await;
    let (_directory, service) = service();
    let (advisor, audit) = audited(Box::new(jev::Jev::new(&server.url, API_KEY)));
    let advisor = advisor.with_limit(Duration::from_millis(150));

    let candidates = propose_one(
        &advisor,
        &service,
        request(user_scope(), "plan", "Plan first."),
    )
    .await
    .expect("queued");

    assert_eq!(advice_line(&candidates[0].proposed), None);
    let rows = audit.rows();
    assert_eq!(rows[0].outcome, "timeout");
    assert_eq!(rows[0].response, None);
    assert!(!rows[0].request.contains(API_KEY));
}

#[tokio::test]
async fn a_jev_answer_of_the_wrong_type_is_invalid() {
    let reply = serde_json::json!({
        "model": "jev-latest",
        "answers": {
            "scope": {"type": "noul", "noul": 0.9},
            "durable": {"type": "noul", "noul": 0.9},
        },
    })
    .to_string();
    let server = serve(Some(reply)).await;
    let (_directory, service) = service();
    let (advisor, audit) = audited(Box::new(jev::Jev::new(&server.url, API_KEY)));

    let candidates = propose_one(
        &advisor,
        &service,
        request(user_scope(), "plan", "Plan first."),
    )
    .await
    .expect("queued");

    assert_eq!(advice_line(&candidates[0].proposed), None);
    assert_eq!(audit.rows()[0].outcome, "invalid");
}

// ---------------------------------------------------------------------------
// The session-model answer format.
// ---------------------------------------------------------------------------

fn session_questions() -> Vec<Question> {
    let record = Record {
        key: "plan-first".into(),
        ..Record::default()
    };
    questions(&[&record])
}

#[test]
fn session_parse_accepts_exactly_the_requested_json() {
    let parsed = session::parse(
        r#"{"scope":"all_workspaces","relation":"duplicate:plan-first","durable":false}"#,
        &session_questions(),
    )
    .expect("valid");
    assert_eq!(parsed[SCOPE], answer(ALL_WORKSPACES, None));
    assert_eq!(parsed[RELATION], answer("duplicate:plan-first", None));
    assert_eq!(parsed[DURABLE], answer(NO, None));
}

#[test]
fn session_parse_rejects_anything_else() {
    let questions = session_questions();
    for text in [
        "all workspaces",
        "[]",
        r#"{"scope":"all_workspaces","relation":"new"}"#,
        r#"{"scope":"all_workspaces","relation":"new","durable":true,"extra":1}"#,
        r#"{"scope":"everywhere","relation":"new","durable":true}"#,
        r#"{"scope":"all_workspaces","relation":"duplicate:other","durable":true}"#,
        r#"{"scope":"all_workspaces","relation":"new","durable":"yes"}"#,
        "```json\n{\"scope\":\"all_workspaces\",\"relation\":\"new\",\"durable\":true}\n```",
    ] {
        assert!(
            matches!(session::parse(text, &questions), Err(Failure::Invalid(_))),
            "{text}"
        );
    }
}
