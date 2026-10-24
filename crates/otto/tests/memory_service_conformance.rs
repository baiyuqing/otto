//! An independent Rust service: no Otto repository or storage implementation.
mod binary_harness;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use otto::memory::client::{Client, PreparationRecovery, prepared_digest};
use otto_core::{memory::*, operation::OperationControl};
use tokio_util::sync::CancellationToken;

struct External {
    description: ServiceDescription,
    calls: Mutex<Vec<Operation>>,
    contexts: Mutex<Vec<MemoryContext>>,
    close_calls: AtomicUsize,
    wrong_scope: bool,
    completion: Completion,
    fail_close: bool,
}

impl External {
    fn new(operations: &[Operation]) -> Self {
        let mut description = ServiceDescription::new("independent-service".into());
        description
            .capabilities
            .push(Capability::new(Operation::Recall));
        for operation in operations {
            let mut capability = Capability::new(*operation);
            capability.supports_reviewed_changes = true;
            capability.supports_delegated_learning = true;
            capability.effects = if *operation == Operation::Prepare {
                vec![Effect::StoreEvidence]
            } else {
                vec![Effect::ChangeMemory]
            };
            if *operation == Operation::Forget {
                capability.forget_targets = vec![ForgetTargetKind::Item];
                capability.forget_levels = vec![ForgetLevel::Suppress];
            }
            description.capabilities.push(capability);
        }
        Self {
            description,
            calls: Mutex::new(vec![]),
            contexts: Mutex::new(vec![]),
            close_calls: AtomicUsize::new(0),
            wrong_scope: false,
            completion: Completion::Unknown,
            fail_close: false,
        }
    }
    fn called(&self, operation: Operation, context: &MemoryContext) {
        self.calls.lock().unwrap().push(operation);
        self.contexts.lock().unwrap().push(context.clone());
    }
    fn count(&self, operation: Operation) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| **op == operation)
            .count()
    }
    fn receipt(&self, id: &str) -> Receipt {
        Receipt {
            request_id: id.into(),
            operation_ref: Some(format!("external-job-{id}")),
            admission: Admission::Acknowledged,
            completion: self.completion,
            visibility: Visibility::Pending,
            items: vec![ItemOutcome {
                reference: None,
                state: ItemState::Skipped,
                reason: None,
            }],
            forget_coverage: None,
        }
    }
}

#[async_trait::async_trait]
impl MemoryService for External {
    fn describe(&self) -> ServiceDescription {
        self.description.clone()
    }
    async fn recall(
        &self,
        request: &RecallRequest,
        _: &dyn OperationControl,
    ) -> Result<RecallResult> {
        self.called(Operation::Recall, &request.context);
        let mut scope = request.context.scopes[0].clone();
        if self.wrong_scope {
            scope.id = "another-user".into();
        }
        Ok(RecallResult {
            items: vec![RecallItem {
                reference: Some(MemoryRef {
                    organization_id: None,
                    service: self.description.identity.clone(),
                    scope,
                    id: "external-item".into(),
                    revision: None,
                }),
                content: Content::Text("neutral external memory".into()),
                sources: vec![],
                effective_at: None,
                metadata: [
                    ("standing".into(), "true".into()),
                    ("human_approved".into(), "true".into()),
                ]
                .into(),
            }],
            used_tokens: Some(5),
            coverage: Coverage::Unknown,
        })
    }
    async fn remember(
        &self,
        request: &RememberRequest,
        _: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.called(Operation::Remember, &request.context);
        Ok(self.receipt(&request.request_id))
    }
    async fn observe(&self, request: &ObserveRequest, _: &dyn OperationControl) -> Result<Receipt> {
        self.called(Operation::Observe, &request.context);
        Ok(self.receipt(&request.request_id))
    }
    async fn forget(&self, request: &ForgetRequest, _: &dyn OperationControl) -> Result<Receipt> {
        self.called(Operation::Forget, &request.context);
        let mut receipt = self.receipt(&request.request_id);
        receipt.forget_coverage = Some(ForgetCoverage {
            achieved: vec![ForgetLevel::Suppress],
            remaining: vec![ForgetLevel::DeleteDerived],
        });
        Ok(receipt)
    }
    async fn prepare(
        &self,
        request: &PrepareRequest,
        _: &dyn OperationControl,
    ) -> Result<PreparedChanges> {
        let MutatingRequest::Remember(request) = &request.operation else {
            panic!("test only prepares remember")
        };
        self.called(Operation::Prepare, &request.context);
        Ok(PreparedChanges {
            reference: "external-preparation".into(),
            context: request.context.clone(),
            changes: vec![Change {
                action: ChangeAction::Update,
                target: Some(reference(&request.context)),
                content: Some(request.content.clone()),
                scope: request.context.scopes[0].clone(),
                sources: vec![],
                metadata: Default::default(),
            }],
            effects: vec![Effect::ChangeMemory],
            expires_at: None,
        })
    }
    async fn apply(&self, request: &ApplyRequest, _: &dyn OperationControl) -> Result<Receipt> {
        self.called(Operation::Apply, &request.context);
        Ok(self.receipt(&request.request_id))
    }
    async fn close(&self, _: &dyn OperationControl) -> Result<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_close {
            Err(Error::new(ErrorKind::Unavailable))
        } else {
            Ok(())
        }
    }
}

fn context() -> MemoryContext {
    let scope = Scope {
        namespace: "user".into(),
        id: "alice".into(),
    };
    MemoryContext {
        organization_id: None,
        principal: "alice".into(),
        scopes: vec![scope.clone()],
        write_scope: Some(scope),
        workspace_id: Some("workspace-a".into()),
        session_id: Some("session-a".into()),
        task_id: None,
        origin: Origin::Model,
    }
}
fn reference(context: &MemoryContext) -> MemoryRef {
    MemoryRef {
        organization_id: None,
        service: "independent-service".into(),
        scope: context.scopes[0].clone(),
        id: "external-item".into(),
        revision: Some("revision-1".into()),
    }
}
fn remember() -> RememberRequest {
    RememberRequest {
        key: None,
        context: context(),
        request_id: "remember-1".into(),
        content: Content::Text("a durable fact".into()),
        target: None,
        metadata: Default::default(),
    }
}
fn observation() -> Observation {
    Observation {
        event_id: "event-1".into(),
        content: Content::Text("task evidence".into()),
        source: SourceRef {
            id: "message-1".into(),
            role: "user".into(),
            occurred_at: None,
            trust: Trust::External,
        },
    }
}
fn grant(background: bool) -> LearningGrant {
    LearningGrant {
        scopes: context().scopes,
        input_classes: vec!["message".into(), "explicit_memory".into()],
        background,
    }
}

#[tokio::test]
async fn recall_only_admits_reads_and_never_submits_unsupported_writes() {
    let service = Arc::new(External::new(&[]));
    let client = Client::new(service.clone()).unwrap();
    assert_eq!(client.mode(), AuthorizationMode::ReviewedChanges);
    let control = CancellationToken::new();
    let result = client
        .recall(
            &RecallRequest {
                scope_filter: None,
                context: context(),
                query: "context".into(),
                limit: 3,
                token_budget: 100,
            },
            &control,
        )
        .await
        .unwrap();
    assert_eq!(
        result.items[0].content,
        Content::Text("neutral external memory".into())
    );
    assert_eq!(
        client
            .remember(&remember(), &control)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(service.count(Operation::Remember), 0);
    let mut hostile = External::new(&[]);
    hostile.wrong_scope = true;
    let client = Client::new(Arc::new(hostile)).unwrap();
    assert_eq!(
        client
            .recall(
                &RecallRequest {
                    scope_filter: None,
                    context: context(),
                    query: "context".into(),
                    limit: 3,
                    token_budget: 100
                },
                &control
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
}

#[tokio::test]
async fn direct_learning_requires_matching_host_grant_and_keeps_partial_receipts() {
    let mut service = External::new(&[Operation::Remember, Operation::Observe]);
    service.completion = Completion::Partial;
    for capability in &mut service.description.capabilities[1..] {
        capability.effects.push(Effect::BackgroundEvolution);
    }
    let service = Arc::new(service);
    let control = CancellationToken::new();
    let reviewed = Client::new(service.clone()).unwrap();
    assert_eq!(
        reviewed
            .remember(&remember(), &control)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    let observe = ObserveRequest {
        context: context(),
        request_id: "observe-1".into(),
        observations: vec![observation()],
        mode: ObserveMode::Learn,
    };
    assert_eq!(
        reviewed.observe(&observe, &control).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    let no_background = Client::with_learning(service.clone(), grant(false)).unwrap();
    assert_eq!(
        no_background
            .remember(&remember(), &control)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    let mut insufficient = grant(true);
    insufficient.input_classes.retain(|c| c != "message");
    let client = Client::with_learning(service.clone(), insufficient).unwrap();
    assert_eq!(
        client.observe(&observe, &control).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    let client = Client::with_learning(service.clone(), grant(true)).unwrap();
    let mut outside = remember();
    outside.context.scopes[0].id = "bob".into();
    outside.context.write_scope = Some(outside.context.scopes[0].clone());
    assert_eq!(
        client.remember(&outside, &control).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    assert!(service.calls.lock().unwrap().is_empty());
    for receipt in [
        client.remember(&remember(), &control).await.unwrap(),
        client.observe(&observe, &control).await.unwrap(),
    ] {
        assert_eq!(receipt.completion, Completion::Partial);
        assert_eq!(receipt.visibility, Visibility::Pending);
        assert_eq!(receipt.items[0].state, ItemState::Skipped);
    }
    for op in [Operation::Remember, Operation::Observe] {
        assert_eq!(service.count(op), 1);
    }
    let mut archive = observe;
    archive.mode = ObserveMode::ArchiveOnly;
    assert_eq!(
        client.observe(&archive, &control).await.unwrap_err().kind,
        ErrorKind::Unsupported
    );
    assert_eq!(service.count(Operation::Observe), 1);
}

#[tokio::test]
async fn forget_target_and_coverage_are_admitted_before_external_effects() {
    let service = Arc::new(External::new(&[Operation::Forget]));
    let client = Client::new(service.clone()).unwrap();
    let mut ctx = context();
    ctx.origin = Origin::Human;
    let mut request = ForgetRequest {
        context: ctx.clone(),
        request_id: "forget-1".into(),
        target: ForgetTarget::Source(observation().source),
        coverage: ForgetLevel::Suppress,
    };
    let control = CancellationToken::new();
    assert_eq!(
        client.forget(&request, &control).await.unwrap_err().kind,
        ErrorKind::Unsupported
    );
    request.target = ForgetTarget::Item(reference(&ctx));
    request.coverage = ForgetLevel::DeleteActive;
    assert_eq!(
        client.forget(&request, &control).await.unwrap_err().kind,
        ErrorKind::Unsupported
    );
    assert_eq!(service.count(Operation::Forget), 0);
    request.coverage = ForgetLevel::Suppress;
    let receipt = client.forget(&request, &control).await.unwrap();
    assert_eq!(receipt.completion, Completion::Unknown);
    assert_eq!(
        receipt.forget_coverage.unwrap().remaining,
        vec![ForgetLevel::DeleteDerived]
    );
    assert_eq!(service.count(Operation::Forget), 1);
}

#[tokio::test]
async fn durable_review_restore_binds_human_digest_context_and_opaque_revision() {
    let service = Arc::new(External::new(&[Operation::Prepare, Operation::Apply]));
    let control = CancellationToken::new();
    let client = Client::new(service.clone()).unwrap();
    let snapshot = client
        .prepare(
            &PrepareRequest {
                operation: MutatingRequest::Remember(remember()),
            },
            &control,
        )
        .await
        .unwrap();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let digest = prepared_digest(&snapshot).unwrap();
    let durable_host = tempfile::tempdir().unwrap();
    let snapshot_path = durable_host.path().join("approved-memory-snapshot.json");
    std::fs::write(&snapshot_path, bytes).unwrap();
    let recovered: PreparedChanges =
        serde_json::from_slice(&std::fs::read(snapshot_path).unwrap()).unwrap();
    let restarted = Client::new(service.clone()).unwrap();
    assert_eq!(
        restarted
            .restore_prepared(
                recovered.clone(),
                &context(),
                &digest,
                PreparationRecovery::NotStarted
            )
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    let mut human = context();
    human.origin = Origin::Human;
    human.session_id = Some("resumed-session".into());
    for invalid in ["principal", "scope", "digest", "revision"] {
        let mut approved = human.clone();
        let mut changed = recovered.clone();
        let mut approved_digest = digest.clone();
        match invalid {
            "principal" => approved.principal = "bob".into(),
            "scope" => {
                approved.scopes[0].id = "bob".into();
                approved.write_scope = Some(approved.scopes[0].clone());
            }
            "digest" => approved_digest = "not-approved".into(),
            "revision" => {
                changed.changes[0].target.as_mut().unwrap().revision = Some("revision-2".into())
            }
            _ => unreachable!(),
        }
        assert!(
            restarted
                .restore_prepared(
                    changed,
                    &approved,
                    &approved_digest,
                    PreparationRecovery::NotStarted
                )
                .is_err(),
            "{invalid}"
        );
    }
    restarted
        .restore_prepared(
            recovered.clone(),
            &human,
            &digest,
            PreparationRecovery::NotStarted,
        )
        .unwrap();
    let request = ApplyRequest {
        context: human,
        request_id: "apply-1".into(),
        preparation: snapshot.reference.clone(),
    };
    let mut changed = recovered.clone();
    changed.changes[0].target.as_mut().unwrap().revision = Some("revision-2".into());
    assert_eq!(
        restarted
            .apply(&changed, &request, &control)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(service.count(Operation::Apply), 0);
    let receipt = restarted
        .apply(&recovered, &request, &control)
        .await
        .unwrap();
    assert_eq!(receipt.completion, Completion::Unknown);
    assert_eq!(receipt.visibility, Visibility::Pending);
    assert_eq!(service.count(Operation::Apply), 1);
    assert!(
        restarted
            .apply(&recovered, &request, &control)
            .await
            .is_err()
    );
    assert_eq!(
        service.count(Operation::Apply),
        1,
        "unknown effects cannot be replayed"
    );
    let uncertain_recovery = Client::new(service.clone()).unwrap();
    uncertain_recovery
        .restore_prepared(
            recovered.clone(),
            &request.context,
            &digest,
            PreparationRecovery::Attempted,
        )
        .unwrap();
    assert!(
        uncertain_recovery
            .apply(&recovered, &request, &control)
            .await
            .is_err()
    );
    assert_eq!(
        service.count(Operation::Apply),
        1,
        "recovered attempted effects cannot be replayed"
    );
}

#[tokio::test]
async fn host_close_is_once_and_rejects_new_requests_without_forgetting() {
    let service = Arc::new(External::new(&[Operation::Forget]));
    let client = Client::new(service.clone())
        .unwrap()
        .with_provider_shutdown(true);
    let control = CancellationToken::new();
    client.close(&control).await.unwrap();
    client.close(&control).await.unwrap();
    assert_eq!(service.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        client
            .recall(
                &RecallRequest {
                    scope_filter: None,
                    context: context(),
                    query: "context".into(),
                    limit: 3,
                    token_budget: 100
                },
                &control
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );
    assert_eq!(service.count(Operation::Recall), 0);
    assert_eq!(service.count(Operation::Forget), 0);
}

#[path = "memory_service_conformance/embedding.rs"]
mod embedding;
