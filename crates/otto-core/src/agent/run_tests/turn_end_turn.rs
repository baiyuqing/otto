//! Run-loop tests: the `turn` content of the `turn_end` hook.

use super::*;

#[derive(Default)]
struct TurnEndHooks {
    turn_end_data: Mutex<Vec<serde_json::Value>>,
}

#[async_trait::async_trait]
impl crate::agent::hooks::Hooks for TurnEndHooks {
    async fn run(
        &self,
        event: &str,
        data: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<crate::agent::hooks::Response, String> {
        if event == "turn_end" {
            self.turn_end_data.lock().unwrap().push(data);
        }
        Ok(Default::default())
    }
}

/// Flattens the recorded turn to `role:type:text-or-name` strings.
fn turn_summary(data: &serde_json::Value) -> Vec<String> {
    let mut summary = Vec::new();
    for message in data["turn"]["messages"].as_array().expect("turn messages") {
        for block in message["blocks"].as_array().expect("blocks") {
            let detail = block["text"]
                .as_str()
                .or(block["name"].as_str())
                .unwrap_or("");
            summary.push(format!(
                "{}:{}:{detail}",
                message["role"].as_str().unwrap(),
                block["type"].as_str().unwrap()
            ));
        }
    }
    summary
}

#[tokio::test]
async fn turn_end_carries_the_messages_appended_during_the_run() {
    let hooks = Arc::new(TurnEndHooks::default());
    let mut opts = options();
    opts.hooks = Some(hooks.clone());
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("ok"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        opts,
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let recorded = hooks.turn_end_data.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let summary = turn_summary(&recorded[0]);
    assert_eq!(summary[0], "user:text:hello");
    assert!(
        summary
            .iter()
            .any(|entry| entry.starts_with("assistant:tool_call:")),
        "{summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|entry| entry.starts_with("tool:tool_result:")),
        "{summary:?}"
    );
    assert_eq!(summary.last().unwrap(), "assistant:text:ok");
    let call = recorded[0]["turn"]["messages"][1]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_call")
        .unwrap();
    assert_eq!(call["id"], "c1");
    assert_eq!(call["arguments"], serde_json::json!({"value": 1}));
}

#[tokio::test]
async fn turn_end_keeps_messages_appended_before_a_compaction_in_the_run() {
    let hooks = Arc::new(TurnEndHooks::default());
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary()), Turn::text("ok")]),
        EchoExecutor::default(),
        seeded_session_with_echo_capabilities().await,
        Options {
            compaction: tight_compaction(),
            hooks: Some(hooks.clone()),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    let recorded = hooks.turn_end_data.lock().unwrap();
    assert_eq!(
        turn_summary(&recorded[0]),
        vec!["user:text:hello", "assistant:text:ok"]
    );
}
