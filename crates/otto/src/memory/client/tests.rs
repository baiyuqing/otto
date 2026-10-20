use super::*;
use std::sync::atomic::AtomicUsize;
use tokio_util::sync::CancellationToken;

struct Fake {
    description: ServiceDescription,
    calls: AtomicUsize,
    close_calls: AtomicUsize,
    recall: Option<RecallResult>,
    completion: Completion,
    fail_close: bool,
    bad_receipt: bool,
    remember_reference: Option<MemoryRef>,
    remember_contexts: Mutex<Vec<MemoryContext>>,
}
impl Fake {
    fn new(operations: &[Operation]) -> Self {
        let mut capabilities = vec![Capability::new(Operation::Recall)];
        capabilities.extend(
            operations
                .iter()
                .copied()
                .filter(|operation| *operation != Operation::Recall)
                .map(|operation| {
                    let mut capability = Capability::new(operation);
                    capability.supports_reviewed_changes = true;
                    capability.supports_delegated_learning = true;
                    if matches!(operation, Operation::Remember | Operation::Apply) {
                        capability.effects = vec![Effect::ChangeMemory];
                    }
                    capability
                }),
        );
        Self {
            description: ServiceDescription {
                identity: "fake-service".into(),
                encryption_at_rest: None,
                version: 1,
                capabilities,
            },
            calls: AtomicUsize::new(0),
            close_calls: AtomicUsize::new(0),
            recall: None,
            completion: Completion::Complete,
            fail_close: false,
            bad_receipt: false,
            remember_reference: None,
            remember_contexts: Mutex::new(vec![]),
        }
    }
    fn receipt(&self, request_id: &str) -> Receipt {
        Receipt {
            request_id: request_id.into(),
            operation_ref: None,
            admission: Admission::Acknowledged,
            completion: self.completion,
            visibility: Visibility::Unknown,
            items: if self.bad_receipt {
                vec![ItemOutcome {
                    reference: None,
                    state: ItemState::Unknown,
                    reason: None,
                }]
            } else if let Some(reference) = &self.remember_reference {
                vec![ItemOutcome {
                    reference: Some(reference.clone()),
                    state: ItemState::Applied,
                    reason: None,
                }]
            } else {
                vec![]
            },
            forget_coverage: None,
        }
    }
}
#[async_trait::async_trait]
impl MemoryService for Fake {
    fn describe(&self) -> ServiceDescription {
        self.description.clone()
    }
    async fn recall(&self, _: &RecallRequest, _: &dyn OperationControl) -> Result<RecallResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.recall.clone().unwrap_or(RecallResult {
            items: vec![],
            used_tokens: Some(0),
            coverage: Coverage::Unknown,
        }))
    }
    async fn remember(
        &self,
        request: &RememberRequest,
        _: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.remember_contexts
            .lock()
            .unwrap()
            .push(request.context.clone());
        Ok(self.receipt(&request.request_id))
    }
    async fn observe(&self, request: &ObserveRequest, _: &dyn OperationControl) -> Result<Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.receipt(&request.request_id))
    }
    async fn prepare(
        &self,
        request: &PrepareRequest,
        _: &dyn OperationControl,
    ) -> Result<PreparedChanges> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let MutatingRequest::Remember(remember) = &request.operation else {
            return Err(err(ErrorKind::Unsupported));
        };
        Ok(PreparedChanges {
            reference: "prepared-1".into(),
            context: remember.context.clone(),
            changes: vec![Change {
                action: ChangeAction::Create,
                target: None,
                content: Some(remember.content.clone()),
                scope: remember.context.write_scope.clone().unwrap(),
                sources: vec![],
                metadata: remember.metadata.clone(),
            }],
            effects: vec![Effect::ChangeMemory],
            expires_at: None,
        })
    }
    async fn apply(&self, request: &ApplyRequest, _: &dyn OperationControl) -> Result<Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.receipt(&request.request_id))
    }
    async fn close(&self, _: &dyn OperationControl) -> Result<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_close {
            Err(err(ErrorKind::OutcomeUnknown))
        } else {
            Ok(())
        }
    }
}
fn context(origin: Origin) -> MemoryContext {
    let scope = Scope {
        namespace: "user".into(),
        id: "u1".into(),
    };
    MemoryContext {
        organization_id: None,
        principal: "human".into(),
        scopes: vec![scope.clone()],
        write_scope: Some(scope),
        workspace_id: None,
        session_id: None,
        task_id: None,
        origin,
    }
}
fn remember(origin: Origin) -> RememberRequest {
    RememberRequest {
        key: None,
        context: context(origin),
        request_id: "r1".into(),
        content: Content::Text("use tabs".into()),
        target: None,
        metadata: BTreeMap::new(),
    }
}
fn grant() -> LearningGrant {
    LearningGrant {
        scopes: context(Origin::Human).scopes,
        input_classes: vec!["explicit_memory".into()],
        background: false,
    }
}
fn recall_request() -> RecallRequest {
    RecallRequest {
        scope_filter: None,
        context: context(Origin::Model),
        query: "tabs".into(),
        limit: 10,
        token_budget: 1000,
    }
}

#[test]
fn injected_exact_guard_rechecks_description_without_remote_effects() {
    let captured_secret = "captured-secret-value".to_owned();
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.description.identity = captured_secret.clone();
    let fake = Arc::new(raw);
    let client = Client::new(fake.clone()).unwrap();
    let guard = Arc::new(
        super::super::guard::ExactGuard::new(std::slice::from_ref(&captured_secret)).unwrap(),
    );
    let error = match client.with_guard(guard) {
        Ok(_) => panic!("captured secret must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert_eq!(error.detail, Error::new(ErrorKind::Rejected).detail);
    assert!(!error.to_string().contains(&captured_secret));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fake.close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_unsupported_model_background_and_canceled_calls_before_service() {
    let token = CancellationToken::new();
    let fake = Arc::new(Fake::new(&[]));
    let client = Client::new(fake.clone()).unwrap();
    assert_eq!(
        client
            .remember(&remember(Origin::Human), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.description.capabilities[1]
        .effects
        .push(Effect::BackgroundEvolution);
    let fake = Arc::new(raw);
    let client = Client::new(fake.clone()).unwrap();
    let mut request = remember(Origin::Model);
    request.metadata.insert("approved".into(), "true".into());
    assert_eq!(
        client.remember(&request, &token).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    assert_eq!(
        client
            .remember(&remember(Origin::Human), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    token.cancel();
    assert_eq!(
        client
            .recall(&recall_request(), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Canceled
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn learning_requires_declared_input_scopes_and_background_permission() {
    let token = CancellationToken::new();
    let fake = Arc::new(Fake::new(&[Operation::Remember, Operation::Observe]));
    let client = Client::with_learning(fake.clone(), grant()).unwrap();
    client
        .remember(&remember(Origin::Model), &token)
        .await
        .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let mut wrong = remember(Origin::Human);
    wrong.context.scopes[0].id = "other".into();
    wrong.context.write_scope = Some(wrong.context.scopes[0].clone());
    assert_eq!(
        client.remember(&wrong, &token).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    let observe = ObserveRequest {
        context: context(Origin::Model),
        request_id: "o1".into(),
        mode: ObserveMode::Learn,
        observations: vec![Observation {
            event_id: "event-1".into(),
            content: Content::Text("hello".into()),
            source: SourceRef {
                id: "source-1".into(),
                role: "user".into(),
                occurred_at: None,
                trust: Trust::Local,
            },
        }],
    };
    assert_eq!(
        client.observe(&observe, &token).await.unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.description.capabilities[1]
        .effects
        .push(Effect::BackgroundEvolution);
    let fake = Arc::new(raw);
    let client = Client::with_learning(fake.clone(), grant()).unwrap();
    assert_eq!(
        client
            .remember(&remember(Origin::Model), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn validates_returned_scopes_and_rejects_secrets() {
    let token = CancellationToken::new();
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.recall = Some(RecallResult {
        items: vec![RecallItem {
            reference: Some(MemoryRef {
                organization_id: None,
                service: "fake-service".into(),
                scope: Scope {
                    namespace: "user".into(),
                    id: "other".into(),
                },
                id: "item-1".into(),
                revision: None,
            }),
            content: Content::Text("plain".into()),
            sources: vec![],
            effective_at: None,
            metadata: BTreeMap::new(),
        }],
        used_tokens: None,
        coverage: Coverage::Complete,
    });
    let fake = Arc::new(raw);
    let client = Client::new(fake.clone()).unwrap();
    assert_eq!(
        client
            .recall(&recall_request(), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    let mut secret = remember(Origin::Human);
    secret
        .metadata
        .insert("note".into(), "password=hunter2".into());
    assert_eq!(
        client.remember(&secret, &token).await.unwrap_err().kind,
        ErrorKind::Rejected
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn application_binds_original_snapshot_and_consumes_completed_reference() {
    let fake = Arc::new(Fake::new(&[Operation::Prepare, Operation::Apply]));
    let client = Client::new(fake.clone()).unwrap();
    let token = CancellationToken::new();
    let snapshot = client
        .prepare(
            &PrepareRequest {
                operation: MutatingRequest::Remember(remember(Origin::Model)),
            },
            &token,
        )
        .await
        .unwrap();
    let apply = ApplyRequest {
        context: context(Origin::Human),
        request_id: "a1".into(),
        preparation: snapshot.reference.clone(),
    };
    let mut changed = snapshot.clone();
    changed.changes[0].content = Some(Content::Text("changed after review".into()));
    assert_eq!(
        client
            .apply(&changed, &apply, &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    client.apply(&snapshot, &apply, &token).await.unwrap();
    assert_eq!(
        client
            .apply(&snapshot, &apply, &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    client
        .restore_prepared(
            snapshot.clone(),
            &context(Origin::Human),
            "incorrect-digest",
            PreparationRecovery::default(),
        )
        .unwrap_err();
    client
        .restore_prepared(
            snapshot.clone(),
            &context(Origin::Human),
            &prepared_digest(&snapshot).unwrap(),
            PreparationRecovery::default(),
        )
        .unwrap();
}

#[tokio::test]
async fn unknown_partial_results_and_failed_close_keep_their_actual_state() {
    for completion in [Completion::Partial, Completion::Unknown] {
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.completion = completion;
        raw.fail_close = true;
        let fake = Arc::new(raw);
        let client = Client::new(fake.clone())
            .unwrap()
            .with_provider_shutdown(true);
        let token = CancellationToken::new();
        let receipt = client
            .remember(&remember(Origin::Human), &token)
            .await
            .unwrap();
        assert_eq!(receipt.completion, completion);
        assert_eq!(receipt.visibility, Visibility::Unknown);
        assert_eq!(
            client.close(&token).await.unwrap_err().kind,
            ErrorKind::OutcomeUnknown
        );
        assert_eq!(
            client.close(&token).await.unwrap_err().kind,
            ErrorKind::OutcomeUnknown
        );
        assert_eq!(fake.close_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            client
                .recall(&recall_request(), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Closed
        );
    }
}

#[tokio::test]
async fn invalid_completion_and_secret_output_are_not_accepted() {
    let token = CancellationToken::new();
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.bad_receipt = true;
    let fake = Arc::new(raw);
    let client = Client::new(fake.clone()).unwrap();
    assert_eq!(
        client
            .remember(&remember(Origin::Human), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let mut raw = Fake::new(&[]);
    raw.recall = Some(RecallResult {
        items: vec![RecallItem {
            reference: None,
            content: Content::Text("password=hunter2".into()),
            sources: vec![],
            effective_at: None,
            metadata: BTreeMap::new(),
        }],
        used_tokens: None,
        coverage: Coverage::Unknown,
    });
    let client = Client::new(Arc::new(raw)).unwrap();
    assert_eq!(
        client
            .recall(&recall_request(), &token)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Rejected
    );
}

#[tokio::test]
async fn workspace_learning_grant_preserves_read_authority_and_narrows_write() {
    let workspace = Scope {
        namespace: "workspace".into(),
        id: "project-1".into(),
    };
    let fake = Arc::new(Fake::new(&[Operation::Remember]));
    let client = Client::with_learning(
        fake.clone(),
        LearningGrant {
            scopes: vec![workspace.clone()],
            input_classes: vec!["explicit_memory".into()],
            background: false,
        },
    )
    .unwrap();
    let mut request = remember(Origin::Model);
    request.context.scopes.push(workspace.clone());
    request.context.write_scope = Some(workspace.clone());
    client
        .remember(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.remember_contexts.lock().unwrap()[0].scopes,
        vec![workspace]
    );
    assert_eq!(request.context.scopes.len(), 2);
}

#[tokio::test]
async fn workspace_write_cannot_return_user_scope_effect() {
    let workspace = Scope {
        namespace: "workspace".into(),
        id: "project-1".into(),
    };
    let mut raw = Fake::new(&[Operation::Remember]);
    raw.remember_reference = Some(MemoryRef {
        organization_id: None,
        service: "fake-service".into(),
        scope: context(Origin::Human).scopes[0].clone(),
        id: "user-effect".into(),
        revision: Some("1".into()),
    });
    let fake = Arc::new(raw);
    let client = Client::with_learning(
        fake.clone(),
        LearningGrant {
            scopes: vec![workspace.clone()],
            input_classes: vec!["explicit_memory".into()],
            background: false,
        },
    )
    .unwrap();
    let mut request = remember(Origin::Model);
    request.context.scopes.push(workspace.clone());
    request.context.write_scope = Some(workspace.clone());
    assert_eq!(
        client
            .remember(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.remember_contexts.lock().unwrap()[0].scopes,
        vec![workspace]
    );
}
