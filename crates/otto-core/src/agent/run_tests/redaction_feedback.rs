//! Exercise feedback at the actual provider/execution boundary, not recording copies.
use super::*;

struct OperationalExecutor {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl ToolExecutor for OperationalExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["grep", "find", "read", "ls", "edit", "write", "bash"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                ..ToolDefinition::default()
            })
            .collect()
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().into(),
            ..ToolResult::default()
        })
    }
}

#[derive(Default)]
struct RewriteHook {
    before: AtomicUsize,
    rewrite: bool,
}
#[async_trait::async_trait]
impl crate::agent::hooks::Hooks for RewriteHook {
    async fn run(
        &self,
        event: &str,
        _: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<crate::agent::hooks::Response, String> {
        if event == "before_tool" {
            self.before.fetch_add(1, Ordering::SeqCst);
            if self.rewrite {
                return Ok(crate::agent::hooks::Response {
                    arguments: Some(serde_json::json!({"pattern":"\u{E000}"})),
                    ..Default::default()
                });
            }
        }
        Ok(Default::default())
    }
}

#[tokio::test]
async fn placeholders_are_rejected_before_hooks_or_effects_and_safe_replanning_can_continue() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(RewriteHook::default());
    let provider = FakeProvider::new(vec![
        Turn::named_tool_call("grep", "bad", r#"{"pattern":"\ue000"}"#),
        Turn::named_tool_call("grep", "safe", r#"{"pattern":"api_key_env"}"#),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        OperationalExecutor {
            calls: calls.clone(),
        },
        MemorySession::new(),
        Options {
            hooks: Some(hooks.clone()),
            ..options()
        },
    );
    let mut outcomes = Vec::new();
    agent
        .run(
            "inspect source",
            &mut |event| {
                if let Event::ToolCallFinished {
                    result, outcome, ..
                } = event
                {
                    outcomes.push((result, outcome));
                }
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.before.load(Ordering::SeqCst), 1);
    assert!(outcomes[0].0.is_error);
    assert!(outcomes[0].0.content.starts_with("redacted_input:"));
    assert_eq!(
        outcomes[0].1.effect_certainty,
        crate::model::EffectCertainty::NotStarted
    );
    assert!(!outcomes[1].0.is_error);
    assert_eq!(outcomes[1].0.content, r#"{"pattern":"api_key_env"}"#);
    assert!(agent.provider().requests()[1].messages.iter().flat_map(|m| &m.blocks)
        .any(|b| b.block_type == BlockType::ToolResult && b.text.starts_with("redacted_input:")));
}

#[tokio::test]
async fn a_hook_cannot_rewrite_valid_arguments_into_an_executable_placeholder() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(RewriteHook {
        rewrite: true,
        ..Default::default()
    });
    let provider = FakeProvider::new(vec![
        Turn::named_tool_call("grep", "c", r#"{"pattern":"needle"}"#),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        OperationalExecutor {
            calls: calls.clone(),
        },
        MemorySession::new(),
        Options {
            hooks: Some(hooks.clone()),
            ..options()
        },
    );
    agent
        .run("inspect", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(hooks.before.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let messages = agent.session().messages();
    let result = messages
        .iter()
        .flat_map(|m| &m.blocks)
        .find(|b| b.block_type == BlockType::ToolResult)
        .unwrap();
    assert!(result.is_error);
    assert!(result.text.starts_with("redacted_input:"));
    assert_eq!(
        result.operation_metadata.as_ref().unwrap().effect_certainty,
        crate::model::EffectCertainty::NotStarted
    );
}

#[tokio::test]
async fn model_feedback_does_not_change_live_json_or_persist_new_annotations() {
    let candidate = ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat();
    let output = format!(r#"{{"api_key":"{candidate}","count":2}}"#);
    let provider = FakeProvider::new(vec![Turn::tool_call("c", "{}"), Turn::text("done")]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some(output.clone()),
            ..Default::default()
        },
        MemorySession::new(),
        options(),
    );
    let mut live = String::new();
    agent
        .run(
            "inspect",
            &mut |event| {
                if let Event::ToolCallFinished { result, .. } = event {
                    live = result.content;
                }
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(live, output);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&live).unwrap()["count"],
        2
    );
    let requests = agent.provider().requests();
    let model_text = requests[1]
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .find(|b| b.block_type == BlockType::ToolResult)
        .unwrap()
        .text
        .clone();
    assert!(!model_text.contains(&candidate));
    assert!(model_text.contains("status=\"partial\""));
    let history = agent.session().messages();
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("otto_redaction")
    );
}
