use super::*;
use port::MemoryService;
use tokio_util::sync::CancellationToken;

fn setup() -> (
    tempfile::TempDir,
    Arc<Service>,
    BuiltinAdapter,
    port::MemoryContext,
) {
    let (directory, store) = super::super::files::testsupport::open_temp();
    let service = Arc::new(Service::new(store, super::super::decide_default_policy));
    let scope = core_scope(&service.identity().unwrap().user_scope);
    let context = port::MemoryContext {
        organization_id: None,
        principal: "human".into(),
        scopes: vec![scope.clone()],
        write_scope: Some(scope),
        workspace_id: None,
        session_id: None,
        task_id: None,
        origin: port::Origin::Human,
    };
    let adapter = BuiltinAdapter::new(service.clone()).unwrap();
    (directory, service, adapter, context)
}
fn request(context: port::MemoryContext) -> port::RememberRequest {
    port::RememberRequest {
        key: None,
        context,
        request_id: "remember-1".into(),
        content: port::Content::Text("apricot orchard".into()),
        target: None,
        metadata: Default::default(),
    }
}
async fn list(adapter: &BuiltinAdapter, context: &port::MemoryContext) -> Vec<port::RecallItem> {
    adapter
        .inspect(
            &port::InspectRequest {
                context: context.clone(),
                query: port::InspectQuery::List {
                    scope: context.scopes[0].clone(),
                    limit: 10,
                    cursor: String::new(),
                },
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .items
}

#[tokio::test]
async fn recall_inspect_and_shared_close_preserve_domain_records() {
    let (_directory, service, adapter, context) = setup();
    let cancel = CancellationToken::new();
    let written = adapter
        .remember(&request(context.clone()), &cancel)
        .await
        .unwrap();
    let reference = written.items[0].reference.clone().unwrap();
    assert_eq!(reference.service, service.identity().unwrap().database_id);
    assert_eq!(list(&adapter, &context).await.len(), 1);
    let recalled = adapter
        .recall(
            &port::RecallRequest {
                scope_filter: None,
                context: context.clone(),
                query: "apricot".into(),
                limit: 10,
                token_budget: 1000,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(recalled.items.len(), 1);
    assert_eq!(recalled.items[0].metadata["otto.kind"], "fact");
    let mut wrong = context.clone();
    wrong.scopes = vec![port::Scope {
        namespace: "workspace".into(),
        id: "other".into(),
    }];
    wrong.write_scope = Some(wrong.scopes[0].clone());
    assert!(list(&adapter, &wrong).await.is_empty());
    adapter.close(&cancel).await.unwrap();
    assert!(
        service
            .get(&super::super::RecordRef {
                scope: native_scope(&reference.scope),
                id: reference.id
            })
            .is_ok()
    );
}

#[tokio::test]
async fn model_writes_require_persisted_review_and_application() {
    let (_directory, _service, adapter, mut context) = setup();
    let cancel = CancellationToken::new();
    context.origin = port::Origin::Model;
    assert_eq!(
        adapter
            .remember(&request(context.clone()), &cancel)
            .await
            .unwrap_err()
            .kind,
        port::ErrorKind::Unauthorized
    );
    let prepared = adapter
        .prepare(
            &port::PrepareRequest {
                operation: port::MutatingRequest::Remember(request(context.clone())),
            },
            &cancel,
        )
        .await
        .unwrap();
    assert!(list(&adapter, &context).await.is_empty());
    context.origin = port::Origin::Human;
    let mut apply = port::ApplyRequest {
        context: context.clone(),
        request_id: "apply-1".into(),
        preparation: prepared.reference,
    };
    let saved = apply.preparation.clone();
    apply.preparation = format!("other:{}", saved.split_once(':').unwrap().1);
    assert_eq!(
        adapter.apply(&apply, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Conflict
    );
    apply.preparation = saved;
    apply.context.principal = "other-human".into();
    assert_eq!(
        adapter.apply(&apply, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Conflict
    );
    apply.context = context.clone();
    let receipt = adapter.apply(&apply, &cancel).await.unwrap();
    assert_eq!(list(&adapter, &context).await.len(), 1);
    assert_eq!(
        adapter.apply(&apply, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Conflict
    );
    let target = receipt.items[0].reference.clone().unwrap();
    context.origin = port::Origin::Model;
    let forget = port::ForgetRequest {
        context,
        request_id: "forget-model".into(),
        target: port::ForgetTarget::Item(target),
        coverage: port::ForgetLevel::DeleteActive,
    };
    assert_eq!(
        adapter.forget(&forget, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Unauthorized
    );
}

#[tokio::test]
async fn wrong_scope_stale_revision_and_unsupported_forget_have_no_effect() {
    let (_directory, _service, adapter, context) = setup();
    let cancel = CancellationToken::new();
    let written = adapter
        .remember(&request(context.clone()), &cancel)
        .await
        .unwrap();
    let mut target = written.items[0].reference.clone().unwrap();
    let original = target.clone();
    target.revision = Some("999".into());
    let mut forget = port::ForgetRequest {
        context: context.clone(),
        request_id: "forget-1".into(),
        target: port::ForgetTarget::Item(target),
        coverage: port::ForgetLevel::DeleteActive,
    };
    assert_eq!(
        adapter.forget(&forget, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Conflict
    );
    forget.target = port::ForgetTarget::Item(original.clone());
    forget.coverage = port::ForgetLevel::DeleteDerived;
    assert_eq!(
        adapter.forget(&forget, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Unsupported
    );
    forget.target = port::ForgetTarget::Scope(context.scopes[0].clone());
    forget.coverage = port::ForgetLevel::DeleteActive;
    assert_eq!(
        adapter.forget(&forget, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Unsupported
    );
    forget.target = port::ForgetTarget::Source(port::SourceRef {
        id: "source-1".into(),
        role: "human".into(),
        occurred_at: None,
        trust: port::Trust::External,
    });
    assert_eq!(
        adapter.forget(&forget, &cancel).await.unwrap_err().kind,
        port::ErrorKind::Unsupported
    );
    let mut model = context.clone();
    model.origin = port::Origin::Model;
    let prepared = adapter
        .prepare(
            &port::PrepareRequest {
                operation: port::MutatingRequest::Remember(request(model)),
            },
            &cancel,
        )
        .await
        .unwrap();
    let mut wrong = context.clone();
    wrong.scopes[0].id = "different".into();
    wrong.write_scope = Some(wrong.scopes[0].clone());
    assert_eq!(
        adapter
            .apply(
                &port::ApplyRequest {
                    context: wrong,
                    request_id: "wrong-scope".into(),
                    preparation: prepared.reference
                },
                &cancel
            )
            .await
            .unwrap_err()
            .kind,
        port::ErrorKind::Conflict
    );
    assert_eq!(list(&adapter, &context).await.len(), 1);
}

#[tokio::test]
async fn cancellation_and_resource_content_are_rejected_before_effects() {
    let (_directory, _service, adapter, context) = setup();
    let canceled = CancellationToken::new();
    canceled.cancel();
    assert_eq!(
        adapter
            .remember(&request(context.clone()), &canceled)
            .await
            .unwrap_err()
            .kind,
        port::ErrorKind::Canceled
    );
    let mut resource = request(context.clone());
    resource.content = port::Content::Resource {
        uri: "file:///example".into(),
        media_type: "text/plain".into(),
    };
    assert_eq!(
        adapter
            .remember(&resource, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        port::ErrorKind::Unsupported
    );
    assert!(list(&adapter, &context).await.is_empty());
}

fn forget_at(
    context: &port::MemoryContext,
    target: port::MemoryRef,
    coverage: port::ForgetLevel,
) -> port::ForgetRequest {
    port::ForgetRequest {
        context: context.clone(),
        request_id: "forget-level".into(),
        target: port::ForgetTarget::Item(target),
        coverage,
    }
}

#[tokio::test]
async fn describe_lists_suppress_invalidate_and_delete_active_forget_levels() {
    let (_directory, _service, adapter, _context) = setup();
    let description = adapter.describe();
    let forget = description
        .capabilities
        .iter()
        .find(|capability| capability.operation == port::Operation::Forget)
        .unwrap();
    assert_eq!(
        forget.forget_levels,
        vec![
            port::ForgetLevel::Suppress,
            port::ForgetLevel::Invalidate,
            port::ForgetLevel::DeleteActive
        ]
    );
}

// Needs the store implementation of `set_state`.
#[tokio::test]
async fn suppress_and_invalidate_set_record_state_and_report_remaining_levels() {
    use port::ForgetLevel::*;
    let cases = [
        (
            Suppress,
            super::super::RecordState::NeedsReview,
            "suppressed through the memory service",
            vec![Invalidate, DeleteActive, DeleteDerived],
        ),
        (
            Invalidate,
            super::super::RecordState::Retracted,
            "invalidated through the memory service",
            vec![DeleteActive, DeleteDerived],
        ),
    ];
    for (level, state, reason, remaining) in cases {
        let (_directory, service, adapter, context) = setup();
        let cancel = CancellationToken::new();
        let written = adapter
            .remember(&request(context.clone()), &cancel)
            .await
            .unwrap();
        let target = written.items[0].reference.clone().unwrap();
        let receipt = adapter
            .forget(&forget_at(&context, target.clone(), level), &cancel)
            .await
            .unwrap();
        let coverage = receipt.forget_coverage.unwrap();
        assert_eq!(coverage.achieved, vec![level]);
        assert_eq!(coverage.remaining, remaining);
        let record = service
            .get(&super::super::RecordRef {
                scope: native_scope(&target.scope),
                id: target.id.clone(),
            })
            .unwrap();
        assert_eq!(record.state, state);
        assert_eq!(record.state_reason, reason);
        assert_eq!(record.revision, 2);
        assert_eq!(
            receipt.items[0]
                .reference
                .as_ref()
                .unwrap()
                .revision
                .as_deref(),
            Some("2")
        );
        assert!(list(&adapter, &context).await.is_empty());
    }
}

// Needs the store implementation of `set_state`.
#[tokio::test]
async fn delete_active_receipt_still_reports_only_derived_copies_remaining() {
    let (_directory, _service, adapter, context) = setup();
    let cancel = CancellationToken::new();
    let written = adapter
        .remember(&request(context.clone()), &cancel)
        .await
        .unwrap();
    let target = written.items[0].reference.clone().unwrap();
    let receipt = adapter
        .forget(
            &forget_at(&context, target, port::ForgetLevel::DeleteActive),
            &cancel,
        )
        .await
        .unwrap();
    let coverage = receipt.forget_coverage.unwrap();
    assert_eq!(coverage.achieved, vec![port::ForgetLevel::DeleteActive]);
    assert_eq!(coverage.remaining, vec![port::ForgetLevel::DeleteDerived]);
}

#[tokio::test]
async fn suppress_with_missing_or_non_numeric_revision_is_invalid_input_before_effects() {
    let (_directory, service, adapter, context) = setup();
    let cancel = CancellationToken::new();
    let written = adapter
        .remember(&request(context.clone()), &cancel)
        .await
        .unwrap();
    let target = written.items[0].reference.clone().unwrap();
    for revision in [None, Some("abc".to_string()), Some("0".to_string())] {
        let mut stale = target.clone();
        stale.revision = revision;
        let error = adapter
            .forget(
                &forget_at(&context, stale, port::ForgetLevel::Suppress),
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, port::ErrorKind::InvalidInput);
    }
    let record = service
        .get(&super::super::RecordRef {
            scope: native_scope(&target.scope),
            id: target.id,
        })
        .unwrap();
    assert_eq!(record.revision, 1);
}

// Needs the store implementation of `set_state` for the state value.
#[tokio::test]
async fn recall_item_carries_otto_state_metadata() {
    let (_directory, _service, adapter, context) = setup();
    let cancel = CancellationToken::new();
    adapter
        .remember(&request(context.clone()), &cancel)
        .await
        .unwrap();
    let items = list(&adapter, &context).await;
    assert_eq!(items[0].metadata["otto.state"], "active");
}

/// Fails when a superseded record's item lacks `otto.superseded_by`, or when
/// an active record's item carries the key.
#[tokio::test]
async fn inspect_item_carries_the_replacement_id_of_a_superseded_record() {
    let (_directory, service, adapter, context) = setup();
    let scope = service.identity().unwrap().user_scope;
    let remember = |key: &str, text: &str| super::super::RememberRequest {
        scope: scope.clone(),
        kind: "fact".into(),
        key: key.into(),
        text: text.into(),
        ..Default::default()
    };
    let old = service.remember(&remember("old", "old fact")).unwrap();
    let new = service.remember(&remember("new", "new fact")).unwrap();
    service
        .set_state(&super::super::SetStateRequest {
            reference: super::super::RecordRef {
                scope: scope.clone(),
                id: old.id.clone(),
            },
            expected_revision: old.revision,
            state: super::super::RecordState::Superseded,
            superseded_by: new.id.clone(),
            reason: String::new(),
        })
        .unwrap();

    let metadata_of = |id: String| {
        let adapter = &adapter;
        let context = context.clone();
        async move {
            let item = adapter
                .inspect(
                    &port::InspectRequest {
                        query: port::InspectQuery::Get(port::MemoryRef {
                            organization_id: None,
                            service: adapter.identity.clone(),
                            scope: context.scopes[0].clone(),
                            id,
                            revision: None,
                        }),
                        context,
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
                .items
                .remove(0);
            item.metadata
        }
    };
    let old_metadata = metadata_of(old.id.clone()).await;
    assert_eq!(old_metadata["otto.state"], "superseded");
    assert_eq!(old_metadata["otto.superseded_by"], new.id);
    assert!(
        !metadata_of(new.id.clone())
            .await
            .contains_key("otto.superseded_by")
    );
}
