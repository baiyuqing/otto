use super::*;
use crate::tool::{ToolCall, ToolContentScope, ToolExecution};

struct BashAndRead;

#[async_trait::async_trait]
impl ToolExecutor for BashAndRead {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["bash", "read"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                ..ToolDefinition::default()
            })
            .collect()
    }

    async fn execute(&self, call: ToolCall<'_>, _: &dyn OperationControl) -> ToolExecution {
        let (content, persisted, scope) = match call.name {
            "bash" => (
                "bash display only",
                "exit_code: 0",
                ToolContentScope::Frontend,
            ),
            "read" => (
                "one-time excerpt",
                "read record",
                ToolContentScope::NextStep,
            ),
            _ => panic!("unexpected test tool"),
        };
        ToolExecution::completed(ToolResult {
            content: content.into(),
            persisted_content: Some(persisted.into()),
            content_scope: scope,
            ..ToolResult::default()
        })
    }
}

const LEGACY_TEXT: &str = "stdout:\nlegacy stdout\nstderr:\nlegacy stderr\nexit_code: 7";

async fn session_with_legacy_bash_output() -> MemorySession {
    let session = MemorySession::new();
    session
        .append(Message {
            role: Role::User,
            blocks: vec![Block::text("legacy turn")],
            ..Message::default()
        })
        .await
        .unwrap();
    session
        .append(
            Turn::named_tool_call("bash", "old-call", r#"{"command":"legacy"}"#)
                .outcome
                .unwrap()
                .message,
        )
        .await
        .unwrap();
    session
        .append(Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_name: "bash".into(),
                tool_call_id: "old-call".into(),
                text: LEGACY_TEXT.into(),
                ..Block::default()
            }],
            ..Message::default()
        })
        .await
        .unwrap();
    session
}

#[tokio::test]
async fn bash_history_is_summarized_and_read_excerpts_reach_only_the_next_step() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::named_tool_call("bash", "b1", r#"{"command":"first"}"#),
            Turn::named_tool_call("read", "r1", r#"{"path":"log"}"#),
            Turn::named_tool_call("bash", "b2", r#"{"command":"second"}"#),
            Turn::text("done"),
            Turn::text("later turn"),
        ]),
        BashAndRead,
        session_with_legacy_bash_output().await,
        options(),
    );
    let mut events = Vec::new();
    let cancel = CancellationToken::new();
    agent
        .run("begin", &mut |event| events.push(event), &cancel)
        .await
        .unwrap();
    agent.run("again", &mut |_| {}, &cancel).await.unwrap();

    let requests = agent.provider().normal_requests();
    let tool_texts = |request: &Request| -> Vec<String> {
        request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| &message.blocks)
            .map(|block| block.text.clone())
            .collect()
    };
    for request in &requests {
        let texts = tool_texts(request);
        assert_eq!(texts[0], "exit_code: 7\nHistorical Bash output omitted.");
        assert!(texts.iter().all(|text| !text.contains("bash display only")));
    }
    assert_eq!(tool_texts(&requests[1])[1], "exit_code: 0");
    assert_eq!(tool_texts(&requests[2])[2], "one-time excerpt");
    assert_eq!(tool_texts(&requests[3])[2], "read record");
    assert_eq!(tool_texts(&requests[4])[2], "read record");

    let stored = agent.session().messages();
    let stored_tools: Vec<_> = stored
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| &message.blocks)
        .map(|block| block.text.as_str())
        .collect();
    assert_eq!(stored_tools[0], LEGACY_TEXT);
    assert_eq!(stored_tools[2], "read record");
    assert!(!stored_tools.contains(&"one-time excerpt"));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ToolCallFinished { result, .. } if result.content == "bash display only"
    )));
}

#[tokio::test]
async fn remote_compaction_only_receives_a_summary_of_legacy_bash_output() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::remote_compaction("checkpoint", "opaque")])
            .with_remote_compaction(),
        BashAndRead,
        session_with_legacy_bash_output().await,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();

    let requests = agent.provider().requests();
    assert!(requests[0].request_remote_compaction);
    let result = &requests[0]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .blocks[0];
    assert_eq!(result.tool_call_id, "old-call");
    assert_eq!(result.text, "exit_code: 7\nHistorical Bash output omitted.");
}
