use super::*;
use otto_core::{agent::memory::MemoryRecall, memory as port, operation::OperationControl};
use std::sync::Mutex;

#[derive(Default)]
struct ObserveRecorder {
    requests: Mutex<Vec<port::ObserveRequest>>,
}

#[async_trait::async_trait]
impl port::MemoryService for ObserveRecorder {
    fn describe(&self) -> port::ServiceDescription {
        let mut description = port::ServiceDescription::new("independent-service".into());
        description
            .capabilities
            .push(port::Capability::new(port::Operation::Recall));
        let mut observe = port::Capability::new(port::Operation::Observe);
        observe.supports_delegated_learning = true;
        observe.effects = vec![port::Effect::ChangeMemory];
        description.capabilities.push(observe);
        description
    }
    async fn recall(
        &self,
        _: &port::RecallRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::RecallResult> {
        Err(port::Error::new(port::ErrorKind::Unsupported))
    }
    async fn observe(
        &self,
        request: &port::ObserveRequest,
        _: &dyn OperationControl,
    ) -> port::Result<port::Receipt> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(port::Receipt {
            request_id: request.request_id.clone(),
            operation_ref: None,
            admission: port::Admission::Acknowledged,
            completion: port::Completion::Complete,
            visibility: port::Visibility::Ready,
            items: vec![],
            forget_coverage: None,
        })
    }
}

fn learning_binding() -> (ServiceBinding, Arc<ObserveRecorder>) {
    let service = Arc::new(ObserveRecorder::default());
    let workspace = port::Scope {
        namespace: "workspace".into(),
        id: "host-workspace".into(),
    };
    let grant = port::LearningGrant {
        scopes: vec![workspace],
        input_classes: ["message", "tool_result", "explicit_memory"]
            .map(String::from)
            .to_vec(),
        background: false,
    };
    let exact = crate::memory::guard::ExactGuard::new(&[]).unwrap();
    let guard = crate::memory::guard::CompositeGuard::new(vec![
        Box::new(crate::memory::guard::DefaultGuard),
        Box::new(exact),
    ]);
    let client = crate::memory::client::Client::with_learning(service.clone(), grant)
        .unwrap()
        .with_guard(Arc::new(guard))
        .unwrap();
    let observes = client.mode() == port::AuthorizationMode::DelegatedLearning;
    let binding = ServiceBinding {
        client: Arc::new(client),
        context: selected_context(),
        observes,
        closed: AtomicBool::new(false),
    };
    (binding, service)
}

fn selected_context() -> port::MemoryContext {
    let workspace = port::Scope {
        namespace: "workspace".into(),
        id: "host-workspace".into(),
    };
    port::MemoryContext {
        organization_id: None,
        principal: "host-user".into(),
        scopes: vec![workspace.clone()],
        write_scope: Some(workspace),
        workspace_id: Some("host-workspace".into()),
        session_id: Some("host-session".into()),
        task_id: None,
        origin: port::Origin::Model,
    }
}

fn text_turn(texts: &[String]) -> serde_json::Value {
    let blocks: Vec<_> = texts
        .iter()
        .map(|text| serde_json::json!({"type": "text", "text": text}))
        .collect();
    serde_json::json!({"messages": [{"role": "user", "blocks": blocks}]})
}

fn sent(service: &ObserveRecorder) -> Vec<port::ObserveRequest> {
    service.requests.lock().unwrap().clone()
}

fn text_of(observation: &port::Observation) -> &str {
    match &observation.content {
        port::Content::Text(text) => text,
        port::Content::Resource { .. } => panic!("resource content"),
    }
}

#[tokio::test]
async fn a_turn_becomes_one_observation_per_block() {
    let (binding, service) = learning_binding();
    let turn = serde_json::json!({"messages": [
        {"role": "user", "blocks": [{"type": "text", "text": "hello"}]},
        {"role": "assistant", "blocks": [
            {"type": "tool_call", "id": "c1", "name": "echo", "arguments": {"value": 1}}]},
        {"role": "tool", "blocks": [
            {"type": "tool_result", "id": "c1", "name": "echo", "text": "out", "is_error": false}]},
        {"role": "assistant", "blocks": [{"type": "text", "text": "done"}]},
    ]});
    binding
        .observe_turn("op_1", &turn, &CancellationToken::new())
        .await
        .unwrap();
    let requests = sent(&service);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].mode, port::ObserveMode::Learn);
    let seen: Vec<_> = requests[0]
        .observations
        .iter()
        .map(|o| {
            (
                o.event_id.as_str(),
                o.source.id.as_str(),
                o.source.role.as_str(),
                o.source.trust,
                serde_json::from_str::<serde_json::Value>(text_of(o))
                    .unwrap_or_else(|_| serde_json::Value::String(text_of(o).into())),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                "op_1:0:0:0",
                "op_1:0:0:0",
                "user",
                port::Trust::Local,
                serde_json::json!("hello")
            ),
            (
                "op_1:1:0:0",
                "op_1:1:0:0",
                "tool_call",
                port::Trust::Local,
                serde_json::json!({"id":"c1","name":"echo","arguments":{"value":1}})
            ),
            (
                "op_1:2:0:0",
                "op_1:2:0:0",
                "tool_result",
                port::Trust::External,
                serde_json::json!({"id":"c1","name":"echo","text":"out","is_error":false})
            ),
            (
                "op_1:3:0:0",
                "op_1:3:0:0",
                "assistant",
                port::Trust::Local,
                serde_json::json!("done")
            ),
        ]
    );
}

#[tokio::test]
async fn text_the_guard_rejects_is_left_out() {
    let (binding, service) = learning_binding();
    let turn = text_turn(&[
        "keep one".into(),
        "has [REDACTED] marker".into(),
        "keep two".into(),
    ]);
    binding
        .observe_turn("op_1", &turn, &CancellationToken::new())
        .await
        .unwrap();
    let requests = sent(&service);
    let texts: Vec<_> = requests[0].observations.iter().map(text_of).collect();
    assert_eq!(texts, ["keep one", "keep two"]);
}

#[tokio::test]
async fn control_characters_are_removed_from_text() {
    let (binding, service) = learning_binding();
    let turn = text_turn(&["\u{1b}[31mred\u{1b}[0m\tok\n".into()]);
    binding
        .observe_turn("op_1", &turn, &CancellationToken::new())
        .await
        .unwrap();
    let requests = sent(&service);
    let texts: Vec<_> = requests[0].observations.iter().map(text_of).collect();
    assert_eq!(texts, ["[31mred[0m\tok\n"]);
}

#[tokio::test]
async fn text_over_the_content_limit_is_split_into_parts() {
    let (binding, service) = learning_binding();
    let text = "é".repeat(10 * 1024);
    assert_eq!(text.len(), 20 * 1024);
    binding
        .observe_turn(
            "op_1",
            &text_turn(std::slice::from_ref(&text)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let observations = &sent(&service)[0].observations;
    let ids: Vec<_> = observations.iter().map(|o| o.event_id.as_str()).collect();
    assert_eq!(ids, ["op_1:0:0:0", "op_1:0:0:1", "op_1:0:0:2"]);
    let joined: String = observations.iter().map(text_of).collect();
    assert_eq!(joined, text);
}

#[tokio::test]
async fn more_than_64_observations_are_sent_as_two_requests() {
    let (binding, service) = learning_binding();
    let texts: Vec<String> = (0..70).map(|index| format!("note {index}")).collect();
    binding
        .observe_turn("op_1", &text_turn(&texts), &CancellationToken::new())
        .await
        .unwrap();
    let counts: Vec<_> = sent(&service)
        .iter()
        .map(|request| request.observations.len())
        .collect();
    assert_eq!(counts, [64, 6]);
}

#[tokio::test]
async fn batches_stay_within_the_content_byte_limit() {
    let observations = super::turn_observation::turn_observations(
        "op_1",
        &text_turn(&vec!["a".repeat(8 * 1024); 40]),
    );
    let batches = super::turn_observation::observation_batches(observations);
    let sizes: Vec<_> = batches
        .iter()
        .map(|batch| batch.iter().map(|o| o.content.byte_len()).sum::<usize>())
        .collect();
    assert_eq!(sizes, [32 * 8 * 1024, 8 * 8 * 1024]);
}

#[tokio::test]
async fn a_reviewed_mode_client_does_not_observe_turns() {
    let client = crate::memory::client::Client::new(Arc::new(ObserveRecorder::default())).unwrap();
    let observes = client.mode() == port::AuthorizationMode::DelegatedLearning;
    let binding = ServiceBinding {
        client: Arc::new(client),
        context: selected_context(),
        observes,
        closed: AtomicBool::new(false),
    };
    assert!(!binding.observes_turns());
}
