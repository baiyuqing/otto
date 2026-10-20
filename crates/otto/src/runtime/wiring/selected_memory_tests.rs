use super::*;
use otto_core::{agent::memory::MemoryRecall, memory as port, operation::OperationControl};

#[derive(Default)]
struct RecallOnly {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl port::MemoryService for RecallOnly {
    fn describe(&self) -> port::ServiceDescription {
        let mut description = port::ServiceDescription::new("independent-service".into());
        description
            .capabilities
            .push(port::Capability::new(port::Operation::Recall));
        description
    }
    async fn recall(
        &self,
        request: &port::RecallRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::RecallResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(port::RecallResult {
            items: vec![port::RecallItem {
                remote: None,
                reference: Some(port::MemoryRef {
                    organization_id: None,
                    service: "independent-service".into(),
                    scope: request.context.scopes[0].clone(),
                    id: "opaque-item".into(),
                    revision: None,
                }),
                content: port::Content::Text("Useful context".into()),
                sources: vec![],
                effective_at: None,
                metadata: [("standing".into(), "true".into())].into(),
            }],
            used_tokens: None,
            coverage: port::Coverage::Complete,
        })
    }
}

fn context() -> port::MemoryContext {
    let user = port::Scope {
        namespace: "user".into(),
        id: "host-user".into(),
    };
    let workspace = port::Scope {
        namespace: "workspace".into(),
        id: "host-workspace".into(),
    };
    port::MemoryContext {
        organization_id: None,
        principal: "host-user".into(),
        scopes: vec![user, workspace.clone()],
        write_scope: Some(workspace),
        workspace_id: Some("host-workspace".into()),
        session_id: Some("host-session".into()),
        task_id: None,
        origin: port::Origin::Model,
    }
}

#[tokio::test]
async fn selected_readonly_service_recalls_without_granting_standing_authority() {
    let provider = Arc::new(RecallOnly::default());
    let client = Arc::new(crate::memory::client::Client::new(provider.clone()).unwrap());
    let binding = ServiceBinding {
        client: client.clone(),
        context: context(),
        closed: AtomicBool::new(false),
    };
    let result = binding
        .recall(
            &otto_core::agent::memory::RecallRequest {
                query: "context".into(),
                limit: 4,
                token_budget: 1000,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.records.len(), 1);
    assert!(!result.records[0].standing);
    binding.close().unwrap();
    let request = otto_core::agent::memory::RecallRequest {
        query: "context".into(),
        limit: 4,
        token_budget: 1000,
    };
    let error = binding
        .recall(&request, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "memory service closed");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "closed binding must not contact the provider"
    );
    let other = ServiceBinding {
        client: client.clone(),
        context: context(),
        closed: AtomicBool::new(false),
    };
    assert_eq!(
        other
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .records
            .len(),
        1
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        !client.is_closed(),
        "closing one session must preserve shared memory"
    );
    let tools = crate::tool::memory_service::tools(client, context(), 4096);
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<Vec<_>>(),
        ["memory_search"]
    );
}

#[tokio::test]
async fn selected_recall_enforces_host_budget_even_without_provider_token_count() {
    let client =
        Arc::new(crate::memory::client::Client::new(Arc::new(RecallOnly::default())).unwrap());
    let binding = ServiceBinding {
        client,
        context: context(),
        closed: AtomicBool::new(false),
    };
    let result = binding
        .recall(
            &otto_core::agent::memory::RecallRequest {
                query: "context".into(),
                limit: 4,
                token_budget: 1,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.records.is_empty());
    assert_eq!(result.used_tokens, 0);
}

#[test]
fn extensions_require_explicit_matching_mode_and_grant() {
    let mut extension =
        MemoryExtension::new(Arc::new(RecallOnly::default()), context().scopes[0].clone());
    assert!(!extension.owned);
    assert!(extension.validate().is_ok());
    extension.mode = port::AuthorizationMode::DelegatedLearning;
    assert!(extension.validate().is_err());
}
