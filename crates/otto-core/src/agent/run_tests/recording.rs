//! Recording copies must not replace live tool arguments.

use super::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn recording_masks_credentials_without_changing_tool_execution() {
    let candidate = "opaque-value-123";
    let arguments = format!(r#"{{"client_secret":"{candidate}","enabled":true}}"#);
    let provider = FakeProvider::new(vec![
        Turn::tool_call("record-call", &arguments),
        Turn::text(&format!("client_secret={candidate}")),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let mut observed_arguments = None;
    agent
        .run(
            &format!("credentials={candidate}"),
            &mut |event| {
                if let Event::ToolCallFinished { result, .. } = event {
                    observed_arguments = Some(result.content);
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    // EchoExecutor returns the exact arguments it received. A recording-only
    // mutation must never reach the executor.
    assert_eq!(observed_arguments.as_deref(), Some(arguments.as_str()));
    let messages = agent.session().messages();
    assert!(
        !serde_json::to_string(&messages)
            .unwrap()
            .contains(candidate)
    );
    let call = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .find(|block| block.block_type == BlockType::ToolCall)
        .expect("stored call");
    assert_eq!(call.tool_call_id, "record-call");
    assert_eq!(call.tool_name, "echo");
    let recorded: serde_json::Value =
        serde_json::from_str(call.arguments.as_ref().unwrap().get()).unwrap();
    assert_eq!(recorded["enabled"], true);
}
