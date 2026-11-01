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
        Ok(Request::Remember { scope, kind, key, text })
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
