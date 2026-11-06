use super::{Reply, Request};
use otto_core::{memory as port, operation::OperationControl};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<port::RememberRequest>>,
}

#[async_trait::async_trait]
impl port::MemoryService for Recorder {
    fn describe(&self) -> port::ServiceDescription {
        let mut description = port::ServiceDescription::new("recorder".into());
        description
            .capabilities
            .push(port::Capability::new(port::Operation::Recall));
        let mut capability = port::Capability::new(port::Operation::Remember);
        capability.supports_reviewed_changes = true;
        capability.effects = vec![port::Effect::ChangeMemory];
        description.capabilities.push(capability);
        description
    }
    async fn recall(
        &self,
        _: &port::RecallRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::RecallResult> {
        Err(port::Error::new(port::ErrorKind::Unsupported))
    }
    async fn remember(
        &self,
        request: &port::RememberRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::Receipt> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(port::Receipt {
            request_id: request.request_id.clone(),
            operation_ref: None,
            admission: port::Admission::Acknowledged,
            completion: port::Completion::Complete,
            visibility: port::Visibility::Unknown,
            items: vec![port::ItemOutcome {
                reference: Some(port::MemoryRef {
                    organization_id: request.context.organization_id.clone(),
                    service: "recorder".into(),
                    scope: request.context.write_scope.clone().unwrap(),
                    id: "item-1".into(),
                    revision: None,
                }),
                state: port::ItemState::Applied,
                reason: None,
            }],
            forget_coverage: None,
        })
    }
}

async fn controller_with_service(service: Arc<Recorder>) -> crate::app::Controller {
    let workspace = tempfile::tempdir().unwrap().keep();
    let sessions = tempfile::tempdir().unwrap().keep();
    let mut builder = crate::fixtures::builder(&workspace, &sessions);
    builder.workspace_scope = crate::memory::Scope {
        namespace: "workspace".into(),
        id: "w1".into(),
    };
    let extension = crate::runtime::wiring::MemoryExtension::new(
        service,
        port::Scope {
            namespace: "user".into(),
            id: "u1".into(),
        },
    )
    .with_organization("acme");
    builder.shared_mut().memory = crate::runtime::wiring::MemoryWiring {
        client: Some(Arc::new(extension.client().unwrap())),
        extension: Some(extension),
        usable: true,
        ..Default::default()
    };
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    let info = builder.runtime_info(&runtime);
    crate::app::Controller::new(builder, true, session, runner, info)
}

#[tokio::test(flavor = "multi_thread")]
async fn remember_reaches_the_memory_service_and_other_memory_requests_stay_unsupported() {
    let service = Arc::new(Recorder::default());
    let controller = controller_with_service(service.clone()).await;
    let run = |args: &'static str| {
        let controller = &controller;
        move || controller.management(Request::parse("remember", args).unwrap())
    };
    let line = match run("--scope user --kind fact --key name hello")() {
        Ok(Reply::Notice(line)) => line,
        other => panic!("expected a notice, got {:?}", other.map(|_| ())),
    };
    assert_eq!(line, "remembered item-1 (scope=user/u1, state=applied)");
    assert!(matches!(run("plain text")(), Ok(Reply::Notice(_))));
    assert!(matches!(
        run("--scope organization --key baseline use tabs")(),
        Ok(Reply::Notice(_))
    ));
    assert!(matches!(run("--scope team x")(), Ok(Reply::Usage(_))));
    let requests = service.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let organization = &requests[2];
    assert_eq!(organization.key.as_deref(), Some("baseline"));
    assert_eq!(
        organization.context.organization_id.as_deref(),
        Some("acme")
    );
    assert_eq!(organization.context.origin, port::Origin::Human);
    assert_eq!(
        organization.context.write_scope.as_ref().unwrap().namespace,
        "organization"
    );
    let user = &requests[0];
    assert_eq!(user.context.origin, port::Origin::Human);
    assert_eq!(
        user.context.write_scope,
        Some(user.context.scopes[0].clone())
    );
    assert_eq!(user.metadata["kind"], "fact");
    assert!(!user.metadata.contains_key("key"));
    assert_eq!(user.key.as_deref(), Some("name"));
    assert!(matches!(&user.content, port::Content::Text(text) if text == "hello"));
    let workspace = &requests[1];
    assert_eq!(
        workspace.context.write_scope,
        Some(workspace.context.scopes[1].clone())
    );
    drop(requests);
    let error = controller
        .management(Request::MemoryReviewList)
        .unwrap_err();
    assert!(error.ends_with("the selected memory service manages its records"));
}

#[test]
fn task_parser_unifies_list_detail_and_cancel() {
    assert!(matches!(Request::parse("task", ""), Ok(Request::Tasks)));
    assert!(matches!(
        Request::parse("task", "review"),
        Ok(Request::Task(id)) if id == "review"
    ));
    assert!(matches!(
        Request::parse("task", "cancel review"),
        Ok(Request::TaskCancel(id)) if id == "review"
    ));
    assert!(matches!(Request::parse("tasks", ""), Ok(Request::Tasks)));
}

#[test]
fn remember_parser_preserves_text_and_defaults() {
    assert!(matches!(
        Request::parse("remember", "--scope user --kind fact --key name hello  there"),
        Ok(Request::Remember { scope, kind, key, text, review_at: None })
            if scope == "user" && kind == "fact" && key == "name" && text == "hello  there"
    ));
    assert!(matches!(
        Request::parse("remember", "hello"),
        Ok(Request::Remember { scope, kind, .. }) if scope == "workspace" && kind == "note"
    ));
}

#[test]
fn removed_reflection_commands_are_unavailable() {
    assert!(Request::parse("reflect", "list").is_err());
    assert!(Request::parse("reflect", "show run-1").is_err());
    assert!(Request::parse("skill", "revert example").is_err());
}

const MEMORY_USAGE_TEXT: &str = "usage: /memory list";

#[test]
fn history_diff_and_restore_parse_their_arguments() {
    assert!(matches!(
        Request::parse("memory", "history r1"),
        Ok(Request::MemoryHistory { id }) if id == "r1"
    ));
    assert!(matches!(
        Request::parse("memory", "diff r1 1"),
        Ok(Request::MemoryDiff { id, from: 1, to: None }) if id == "r1"
    ));
    assert!(matches!(
        Request::parse("memory", "diff r1 1 3"),
        Ok(Request::MemoryDiff { id, from: 1, to: Some(3) }) if id == "r1"
    ));
    assert!(matches!(
        Request::parse("memory", "restore r1 2"),
        Ok(Request::MemoryRestore { id, revision: 2 }) if id == "r1"
    ));
}

#[test]
fn history_diff_and_restore_reject_bad_arguments_with_the_usage_text() {
    for args in [
        "history",
        "history r1 extra",
        "diff",
        "diff r1",
        "diff r1 x",
        "diff r1 1 y",
        "diff r1 1 2 3",
        "diff r1 -1",
        "restore",
        "restore r1",
        "restore r1 latest",
        "restore r1 2 3",
    ] {
        let usage = match Request::parse("memory", args) {
            Err(usage) => usage,
            Ok(request) => panic!("`{args}` parsed as {request:?}"),
        };
        assert!(usage.starts_with(MEMORY_USAGE_TEXT), "{args}: {usage}");
        for word in [
            "/memory history <id>",
            "/memory diff <id>",
            "/memory restore <id>",
        ] {
            assert!(usage.contains(word), "{args}: usage lacks {word}: {usage}");
        }
    }
}

struct MemoryFixture {
    controller: crate::app::Controller,
    _directories: [tempfile::TempDir; 3],
}

async fn memory_fixture() -> MemoryFixture {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let controller = crate::fixtures::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    MemoryFixture {
        controller,
        _directories: [workspace, sessions, store],
    }
}

fn remember_text(controller: &crate::app::Controller, text: &str) -> crate::memory::Record {
    let request = Request::parse(
        "remember",
        &format!("--scope user --kind note --key editor {text}"),
    )
    .expect("parse remember");
    match controller.management(request) {
        Ok(Reply::Remembered(record)) => record,
        other => panic!("expected a remembered record, got {:?}", other.map(|_| ())),
    }
}

fn run_memory(controller: &crate::app::Controller, args: &str) -> Result<Reply, String> {
    controller.management(Request::parse("memory", args).expect("parse memory"))
}

#[tokio::test(flavor = "multi_thread")]
async fn history_of_a_new_record_has_one_revision() {
    let fixture = memory_fixture().await;
    let record = remember_text(&fixture.controller, "vim");

    let Ok(Reply::MemoryHistory(history)) =
        run_memory(&fixture.controller, &format!("history {}", record.id))
    else {
        panic!("expected a history reply")
    };

    assert_eq!(history.revisions.len(), 1);
    assert_eq!(history.revisions[0].revision, 1);
    assert_eq!(history.revisions[0].text, "vim");
}

#[tokio::test(flavor = "multi_thread")]
async fn history_diff_and_restore_report_an_unknown_record() {
    let fixture = memory_fixture().await;
    for args in ["history missing", "diff missing 1", "restore missing 1"] {
        let error = run_memory(&fixture.controller, args)
            .map(|_| ())
            .unwrap_err();
        assert!(error.contains("not found"), "{args}: {error}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_of_a_single_revision_record_compares_it_with_itself() {
    let fixture = memory_fixture().await;
    let record = remember_text(&fixture.controller, "vim");

    let Ok(Reply::MemoryDiff { from, to }) =
        run_memory(&fixture.controller, &format!("diff {} 1", record.id))
    else {
        panic!("expected a diff reply")
    };

    assert_eq!((from.revision, to.revision), (1, 1));
    assert_eq!(from.text, to.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_names_a_revision_the_record_does_not_have() {
    let fixture = memory_fixture().await;
    let record = remember_text(&fixture.controller, "vim");

    let error = run_memory(&fixture.controller, &format!("diff {} 5", record.id))
        .map(|_| ())
        .unwrap_err();

    assert!(error.contains("has no revision 5"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_of_the_only_revision_is_an_error() {
    let fixture = memory_fixture().await;
    let record = remember_text(&fixture.controller, "vim");

    let error = run_memory(&fixture.controller, &format!("restore {} 1", record.id))
        .map(|_| ())
        .unwrap_err();

    assert!(
        error.contains("must precede the current revision"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn update_history_diff_restore_and_forget_through_the_management_requests() {
    let fixture = memory_fixture().await;
    let controller = &fixture.controller;
    let created = remember_text(controller, "vim");
    let updated = remember_text(controller, "emacs");
    assert_eq!(
        (updated.id.as_str(), updated.revision),
        (created.id.as_str(), 2)
    );

    let Ok(Reply::MemoryHistory(history)) =
        run_memory(controller, &format!("history {}", created.id))
    else {
        panic!("expected a history reply")
    };
    let texts: Vec<&str> = history.revisions.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["vim", "emacs"]);

    let Ok(Reply::MemoryDiff { from, to }) =
        run_memory(controller, &format!("diff {} 1 2", created.id))
    else {
        panic!("expected a diff reply")
    };
    assert_eq!((from.text.as_str(), to.text.as_str()), ("vim", "emacs"));

    let Ok(Reply::MemoryRestored(restored)) =
        run_memory(controller, &format!("restore {} 1", created.id))
    else {
        panic!("expected a restored reply")
    };
    assert_eq!(restored.revision, 3);
    assert_eq!(restored.text, "vim");
    assert_eq!(
        restored
            .metadata
            .get(crate::memory::RESTORED_FROM_REVISION_METADATA)
            .map(String::as_str),
        Some("1")
    );

    let Ok(Reply::MemoryForgotten {
        tombstone,
        previous_revision,
        cleanup,
    }) = run_memory(controller, &format!("forget {}", created.id))
    else {
        panic!("expected a forgotten reply")
    };
    assert_eq!(
        (tombstone.id.as_str(), previous_revision),
        (created.id.as_str(), 3)
    );
    use crate::memory::CopyStatus;
    let store_entries = &cleanup[..cleanup.len() - 2];
    assert!(
        !store_entries.is_empty(),
        "the store reports the copies it handled: {cleanup:?}"
    );
    assert!(
        store_entries
            .iter()
            .any(|entry| entry.copy.contains("history")),
        "{cleanup:?}"
    );
    assert_eq!(cleanup[cleanup.len() - 2].status, CopyStatus::Unverified);
    assert_eq!(cleanup[cleanup.len() - 1].status, CopyStatus::Present);
}
