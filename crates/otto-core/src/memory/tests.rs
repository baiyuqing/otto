use super::*;
use crate::operation::OperationControl;
use tokio_util::sync::CancellationToken;

fn context() -> MemoryContext {
    MemoryContext {
        organization_id: None,
        principal: "user-1".into(),
        scopes: vec![Scope {
            namespace: "user".into(),
            id: "user-1".into(),
        }],
        write_scope: None,
        workspace_id: None,
        session_id: None,
        task_id: None,
        origin: Origin::Model,
    }
}

#[test]
fn organization_scope_accepts_internal_spaces_only() {
    let mut context = context();
    context.organization_id = Some("bai's org".into());
    context.scopes.push(Scope {
        namespace: "organization".into(),
        id: "bai's org".into(),
    });
    assert!(context.validate().is_ok());
    context.scopes[1].id = "bad\torg".into();
    assert!(context.validate().is_err());
    context.scopes[1].id = "   ".into();
    assert!(context.validate().is_err());
    context.scopes[1].namespace = "workspace".into();
    context.scopes[1].id = "has space".into();
    assert!(context.validate().is_err());
}
fn request() -> RecallRequest {
    RecallRequest {
        scope_filter: None,
        context: context(),
        query: "project context".into(),
        limit: 3,
        token_budget: 100,
    }
}
fn reference() -> MemoryRef {
    MemoryRef {
        organization_id: None,
        service: "test-service".into(),
        scope: context().scopes[0].clone(),
        id: "item-1".into(),
        revision: None,
    }
}
fn item() -> RecallItem {
    RecallItem {
        remote: None,
        reference: Some(reference()),
        content: Content::Text("reference material".into()),
        sources: vec![],
        effective_at: None,
        metadata: Default::default(),
    }
}

struct RecallOnly;
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl MemoryService for RecallOnly {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            identity: "test-service".into(),
            version: 1,
            capabilities: vec![Capability::new(Operation::Recall)],
            encryption_at_rest: None,
        }
    }
    async fn recall(
        &self,
        request: &RecallRequest,
        _control: &dyn OperationControl,
    ) -> Result<RecallResult> {
        request.validate()?;
        Ok(RecallResult {
            items: vec![item()],
            used_tokens: Some(5),
            coverage: Coverage::Complete,
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
async fn recall_only_service_works_and_optional_write_is_unsupported() {
    let service: &dyn MemoryService = &RecallOnly;
    let control = CancellationToken::new();
    service.describe().validate().unwrap();
    let request = request();
    let result = service.recall(&request, &control).await.unwrap();
    result.validate(&request, "test-service").unwrap();
    let remember = RememberRequest {
        key: None,
        context: context(),
        request_id: "req-1".into(),
        content: Content::Text("hello".into()),
        target: None,
        metadata: Default::default(),
    };
    assert_eq!(
        service
            .remember(&remember, &control)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(
        service
            .describe()
            .capability(Operation::Remember)
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    service.close(&control).await.unwrap();
}

#[test]
fn returned_references_cannot_expand_authority_or_cross_service() {
    let request = request();
    let mut result = RecallResult {
        items: vec![item()],
        used_tokens: None,
        coverage: Coverage::Unknown,
    };
    result.items[0].reference.as_mut().unwrap().scope.id = "other-user".into();
    assert_eq!(
        result.validate(&request, "test-service").unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    result.items[0] = item();
    result.items[0].reference.as_mut().unwrap().service = "other-service".into();
    assert_eq!(
        result.validate(&request, "test-service").unwrap_err().kind,
        ErrorKind::InvalidInput
    );
    result.items[0] = item();
    result.items[0].reference.as_mut().unwrap().id = "bad\nref".into();
    assert!(result.validate(&request, "test-service").is_err());
}

#[test]
fn requests_and_results_reject_oversized_values() {
    let mut request = request();
    request.query = "x".repeat(MAX_QUERY_BYTES + 1);
    assert!(request.validate().is_err());
    let request = super::tests::request();
    let mut result = RecallResult {
        items: vec![item()],
        used_tokens: Some(101),
        coverage: Coverage::Complete,
    };
    assert!(result.validate(&request, "test-service").is_err());
    result.used_tokens = Some(1);
    result.items[0].content = Content::Text("x".repeat(MAX_CONTENT_BYTES + 1));
    assert!(result.validate(&request, "test-service").is_err());
    result.items[0] = item();
    result.items[0].metadata = (0..65)
        .map(|i| (format!("key-{i}"), "value".into()))
        .collect();
    assert!(result.validate(&request, "test-service").is_err());
}

#[test]
fn descriptors_reject_duplicate_operations_and_read_effects() {
    let mut descriptor = RecallOnly.describe();
    descriptor
        .capabilities
        .push(Capability::new(Operation::Recall));
    assert!(descriptor.validate().is_err());
    descriptor.capabilities.pop();
    descriptor.capabilities[0]
        .effects
        .push(Effect::ChangeMemory);
    assert!(descriptor.validate().is_err());
    descriptor.capabilities[0].effects.clear();
    let mut prepare = Capability::new(Operation::Prepare);
    prepare.effects.push(Effect::StoreEvidence);
    descriptor.capabilities.push(prepare);
    descriptor.validate().unwrap();
    descriptor.capabilities[1]
        .effects
        .push(Effect::ChangeMemory);
    assert!(descriptor.validate().is_err());
    let capability = Capability::new(Operation::Remember);
    assert!(
        !capability.idempotent && !capability.conditional_updates && !capability.tracks_completion
    );
    assert!(!capability.supports_reviewed_changes && !capability.supports_delegated_learning);
}

#[test]
fn output_metadata_has_room_for_bounded_semantic_projections() {
    let mut item = item();
    item.metadata = (0..35)
        .map(|i| (format!("key-{i}"), "value".into()))
        .collect();
    item.validate(&context(), "test-service").unwrap();
    let request = RememberRequest {
        key: None,
        context: context(),
        request_id: "req-1".into(),
        content: Content::Text("hello".into()),
        target: Some(reference()),
        metadata: item.metadata,
    };
    assert!(request.validate("test-service").is_err());
}

#[test]
fn newly_formed_memory_requires_an_explicit_admitted_destination() {
    let mut remember = RememberRequest {
        key: None,
        context: context(),
        request_id: "req-1".into(),
        content: Content::Text("hello".into()),
        target: None,
        metadata: Default::default(),
    };
    assert!(remember.validate("test-service").is_err());
    remember.context.write_scope = Some(remember.context.scopes[0].clone());
    remember.validate("test-service").unwrap();
    remember.context.write_scope.as_mut().unwrap().id = "other-user".into();
    assert_eq!(
        remember.validate("test-service").unwrap_err().kind,
        ErrorKind::Unauthorized
    );
}

#[test]
fn prepared_changes_bind_context_targets_and_metadata() {
    let mut context = context();
    context.write_scope = Some(context.scopes[0].clone());
    let request = PrepareRequest {
        operation: MutatingRequest::Remember(RememberRequest {
            key: None,
            context: context.clone(),
            request_id: "req-1".into(),
            content: Content::Text("new fact".into()),
            target: None,
            metadata: Default::default(),
        }),
    };
    let mut prepared = PreparedChanges {
        reference: "preparation-1".into(),
        context: context.clone(),
        changes: vec![Change {
            action: ChangeAction::Create,
            target: None,
            content: Some(Content::Text("new fact".into())),
            scope: context.scopes[0].clone(),
            sources: vec![],
            metadata: Default::default(),
        }],
        effects: vec![Effect::ChangeMemory],
        expires_at: None,
    };
    prepared.validate(&request, "test-service").unwrap();
    let mut apply_context = context.clone();
    apply_context.origin = Origin::Human;
    apply_context.session_id = Some("resumed-session".into());
    prepared
        .validate_snapshot(&apply_context, "test-service")
        .unwrap();
    apply_context.principal = "other-principal".into();
    assert_eq!(
        prepared
            .validate_snapshot(&apply_context, "test-service")
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    apply_context.principal = context.principal.clone();
    apply_context.scopes[0].id = "other-user".into();
    apply_context.write_scope = None;
    assert_eq!(
        prepared
            .validate_snapshot(&apply_context, "test-service")
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    prepared.context.origin = Origin::Human;
    assert!(prepared.validate(&request, "test-service").is_err());
    prepared.context = context;
    prepared.changes[0].action = ChangeAction::Update;
    assert!(prepared.validate(&request, "test-service").is_err());
    prepared.changes[0].target = Some(reference());
    prepared.validate(&request, "test-service").unwrap();
    prepared.changes[0]
        .metadata
        .insert("key".into(), "x".repeat(513));
    assert!(prepared.validate(&request, "test-service").is_err());
}

#[test]
fn forgetting_capabilities_require_exact_target_and_level_declarations() {
    let mut descriptor = RecallOnly.describe();
    descriptor
        .capabilities
        .push(Capability::new(Operation::Forget));
    assert!(descriptor.validate().is_err());
    descriptor.capabilities[1].forget_targets = vec![ForgetTargetKind::Item];
    descriptor.capabilities[1].forget_levels = vec![ForgetLevel::DeleteActive];
    descriptor.validate().unwrap();
    descriptor.capabilities[1]
        .forget_levels
        .push(ForgetLevel::DeleteActive);
    assert!(descriptor.validate().is_err());
    descriptor.capabilities[1].forget_levels.pop();
    descriptor.capabilities[1]
        .forget_targets
        .push(ForgetTargetKind::Item);
    assert!(descriptor.validate().is_err());
    descriptor.capabilities[1].forget_targets.pop();
    descriptor.capabilities[0]
        .forget_targets
        .push(ForgetTargetKind::Scope);
    assert!(descriptor.validate().is_err());
}

#[test]
fn missing_encryption_declaration_remains_unknown() {
    let description = RecallOnly.describe();
    assert_eq!(description.encryption_at_rest, None);
    let mut encoded = serde_json::to_value(description).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .remove("encryption_at_rest");
    let decoded: ServiceDescription = serde_json::from_value(encoded).unwrap();
    decoded.validate().unwrap();
    assert_ne!(decoded.encryption_at_rest, Some(true));
    assert_eq!(decoded.encryption_at_rest, None);
}

#[test]
fn forgetting_receipts_cannot_upgrade_suppression_into_deletion() {
    let request = ForgetRequest {
        context: context(),
        request_id: "forget-1".into(),
        target: ForgetTarget::Item(reference()),
        coverage: ForgetLevel::DeleteActive,
    };
    let mut receipt = Receipt {
        request_id: request.request_id.clone(),
        operation_ref: None,
        admission: Admission::Acknowledged,
        completion: Completion::Complete,
        visibility: Visibility::NotApplicable,
        items: vec![],
        forget_coverage: Some(ForgetCoverage {
            achieved: vec![ForgetLevel::Suppress],
            remaining: vec![ForgetLevel::DeleteActive],
        }),
    };
    assert!(receipt.validate_forget(&request, "test-service").is_err());
    receipt.completion = Completion::Partial;
    receipt.validate_forget(&request, "test-service").unwrap();
    receipt.completion = Completion::Complete;
    receipt.forget_coverage = Some(ForgetCoverage {
        achieved: vec![ForgetLevel::DeleteActive],
        remaining: vec![ForgetLevel::DeleteDerived],
    });
    receipt.validate_forget(&request, "test-service").unwrap();
    receipt.forget_coverage = None;
    assert!(receipt.validate_forget(&request, "test-service").is_err());
}

#[test]
fn receipts_keep_completion_visibility_and_forgetting_distinct() {
    let mut receipt = Receipt {
        request_id: "req-1".into(),
        operation_ref: Some("job-1".into()),
        admission: Admission::Acknowledged,
        completion: Completion::Complete,
        visibility: Visibility::Pending,
        items: vec![ItemOutcome {
            reference: Some(reference()),
            state: ItemState::Applied,
            reason: None,
        }],
        forget_coverage: Some(ForgetCoverage {
            achieved: vec![ForgetLevel::Suppress],
            remaining: vec![ForgetLevel::DeleteDerived],
        }),
    };
    receipt
        .validate(&context(), "req-1", "test-service")
        .unwrap();
    receipt.items[0].state = ItemState::Failed;
    assert!(
        receipt
            .validate(&context(), "req-1", "test-service")
            .is_err()
    );
    receipt.completion = Completion::Partial;
    receipt
        .validate(&context(), "req-1", "test-service")
        .unwrap();
    receipt
        .forget_coverage
        .as_mut()
        .unwrap()
        .remaining
        .push(ForgetLevel::Suppress);
    assert!(
        receipt
            .validate(&context(), "req-1", "test-service")
            .is_err()
    );
}

#[test]
fn inspection_can_return_current_revision_without_a_requested_revision() {
    let mut request = InspectRequest {
        context: context(),
        query: InspectQuery::Get(reference()),
    };
    let mut returned = item();
    returned.reference.as_mut().unwrap().revision = Some("revision-2".into());
    let result = InspectResult {
        items: vec![returned],
        next_cursor: String::new(),
        state: String::new(),
    };
    result.validate(&request, "test-service").unwrap();
    if let InspectQuery::Get(reference) = &mut request.query {
        reference.revision = Some("revision-1".into());
    }
    assert!(result.validate(&request, "test-service").is_err());
}

#[test]
fn organization_references_are_bound_not_rebound_and_local_json_stays_readable() {
    let legacy = r#"{"service":"test-service","scope":{"namespace":"user","id":"user-1"},"id":"item-1","revision":null}"#;
    let local: MemoryRef = serde_json::from_str(legacy).unwrap();
    assert!(local.validate(&context(), "test-service").is_ok());
    assert!(
        !serde_json::to_string(&local)
            .unwrap()
            .contains("organization_id")
    );
    let mut context = context();
    context.organization_id = Some("acme".into());
    let mut reference = reference();
    reference.service = "remote".into();
    reference.revision = Some("opaque-r4".into());
    assert!(reference.validate(&context, "remote").is_err());
    reference.organization_id = Some("other".into());
    assert_eq!(
        reference.validate(&context, "remote").unwrap_err().kind,
        ErrorKind::Unauthorized
    );
    reference.organization_id = Some("acme".into());
    assert!(reference.validate(&context, "remote").is_ok());
    reference.revision = None;
    assert!(reference.validate(&context, "remote").is_err());
}
#[test]
fn semantic_key_and_effective_scope_filter_have_closed_contracts() {
    for key in ["a", "response.language", "editor_default-2", "123"] {
        assert!(valid_memory_key(key), "{key}");
    }
    for key in [
        "",
        "Upper",
        ".leading",
        "trailing-",
        "two..dots",
        "a b",
        "中文",
    ] {
        assert!(!valid_memory_key(key), "{key}");
    }
    assert!(!valid_memory_key(&"a".repeat(129)));
    let mut request = request();
    request.scope_filter = Some(vec![ScopeNamespace::User]);
    assert!(request.validate().is_ok());
    for filter in [
        vec![],
        vec![ScopeNamespace::User, ScopeNamespace::User],
        vec![ScopeNamespace::Workspace],
    ] {
        request.scope_filter = Some(filter);
        assert!(request.validate().is_err());
    }
    assert!(serde_json::from_str::<ScopeNamespace>("\"global\"").is_err());
}
