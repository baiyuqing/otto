//! Run-loop tests: the Claude cache breakpoints of consecutive turns mark an identical prefix even
//! though each turn adds a request-local memory message and a live tool-result overlay.

use super::*;
use crate::claude::protocol::build_request;
use serde_json::Value;

/// Serialized Claude body of every ordinary provider request, in call order.
fn claude_bodies(agent: &Agent<FakeProvider, EchoExecutor, MemorySession>) -> Vec<Value> {
    agent
        .provider()
        .normal_requests()
        .iter()
        .map(|request| serde_json::to_value(build_request(request).expect("encodes")).unwrap())
        .collect()
}

/// Wire positions (message index, block index) carrying `cache_control`, in order.
fn breakpoint_positions(body: &Value) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    for (message_index, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (block_index, block) in message["content"].as_array().unwrap().iter().enumerate() {
            if block.get("cache_control").is_some() {
                positions.push((message_index, block_index));
            }
        }
    }
    positions
}

/// The serialized system plus every message block up to and including the block at `position`.
fn prefix_through(body: &Value, position: (usize, usize)) -> Value {
    let messages = body["messages"].as_array().unwrap();
    let mut prefix: Vec<Value> = messages[..position.0].to_vec();
    let mut last = messages[position.0].clone();
    last["content"]
        .as_array_mut()
        .unwrap()
        .truncate(position.1 + 1);
    prefix.push(last);
    serde_json::json!({"system": body["system"], "messages": prefix})
}

async fn run_three_turns(
    executor: EchoExecutor,
    turns: Vec<Turn>,
) -> Agent<FakeProvider, EchoExecutor, MemorySession> {
    let agent = Agent::new(
        FakeProvider::new(turns),
        executor,
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    let cancel = CancellationToken::new();
    for input in ["first", "second", "third"] {
        agent.run(input, &mut |_| {}, &cancel).await.expect("run");
    }
    agent
}

/// Asserts that the first request of the third turn finds, at its earliest message breakpoint,
/// the prefix that the first request of the second turn wrote at its own earliest breakpoint.
fn assert_turn_three_reuses_turn_two_prefix(
    bodies: &[Value],
    turn_two_first: usize,
    turn_three_first: usize,
) {
    let written = &bodies[turn_two_first];
    let read = &bodies[turn_three_first];
    assert!(
        written["messages"].to_string().contains("prefers vim"),
        "the second turn carries the request-local memory message"
    );
    assert!(read["messages"].to_string().contains("prefers vim"));
    let written_positions = breakpoint_positions(written);
    let read_positions = breakpoint_positions(read);
    assert_eq!(written_positions.len(), 2, "boundary and last block");
    assert_eq!(read_positions.len(), 3, "two boundaries and last block");
    let written_prefix = prefix_through(written, written_positions[0]);
    let read_prefix = prefix_through(read, read_positions[0]);
    assert_eq!(written_prefix, read_prefix);
    assert!(!written_prefix.to_string().contains("prefers vim"));
    assert!(written_prefix.to_string().contains("first reply"));
    // The later boundary of the third turn precedes its memory message and "third".
    let later = prefix_through(read, read_positions[1]);
    assert!(later.to_string().contains("second reply"));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn third_turn_reuses_second_turn_cache_prefix_with_memory_message() {
    let agent = run_three_turns(
        EchoExecutor::default(),
        vec![
            Turn::text("first reply"),
            Turn::text("second reply"),
            Turn::text("third reply"),
        ],
    )
    .await;
    assert_turn_three_reuses_turn_two_prefix(&claude_bodies(&agent), 1, 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn later_turn_reuses_cache_prefix_after_a_tool_result_overlay() {
    let agent = run_three_turns(
        EchoExecutor {
            persisted: Some("[stored placeholder]".into()),
            content: Some("live tool text".into()),
        },
        vec![
            Turn::text("first reply"),
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("second reply"),
            Turn::text("third reply"),
        ],
    )
    .await;
    let bodies = claude_bodies(&agent);
    // Requests: 0 = turn one; 1, 2 = turn two (2 carries the live tool text);
    // 3 = turn three, which sends the stored placeholder instead.
    assert!(bodies[2]["messages"].to_string().contains("live tool text"));
    assert!(!bodies[3]["messages"].to_string().contains("live tool text"));
    assert!(
        bodies[3]["messages"]
            .to_string()
            .contains("stored placeholder")
    );
    // The turn-two request that carried the live text wrote the prefix that
    // ends before "second"; turn three reads exactly that prefix.
    assert_turn_three_reuses_turn_two_prefix(&bodies, 2, 3);
}
