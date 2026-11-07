use super::*;

fn message(role: Role, blocks: Vec<Block>) -> Message {
    Message {
        role,
        blocks,
        ..Default::default()
    }
}

fn tool_call(id: &str) -> Block {
    Block {
        block_type: BlockType::ToolCall,
        tool_call_id: id.into(),
        tool_name: "echo".into(),
        arguments: Some(RawValue::from_string("{}".into()).unwrap()),
        ..Default::default()
    }
}

fn tool_result(id: &str) -> Block {
    Block {
        block_type: BlockType::ToolResult,
        tool_call_id: id.into(),
        text: "result".into(),
        ..Default::default()
    }
}

fn encode(messages: Vec<Message>, cache_boundaries: Vec<usize>, system: &str) -> Value {
    let request = Request {
        system_prompt: system.into(),
        messages,
        cache_boundaries,
        tools: vec![ToolDefinition {
            name: "echo".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    serde_json::to_value(protocol::build_request(&request).unwrap()).unwrap()
}

/// (message index, block index) of every message block carrying `cache_control`.
fn marked(body: &Value) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (m, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (b, block) in message["content"].as_array().unwrap().iter().enumerate() {
            if block.get("cache_control").is_some() {
                found.push((m, b));
            }
        }
    }
    found
}

fn conversation() -> Vec<Message> {
    vec![
        message(Role::User, vec![Block::text("u0")]),
        message(Role::Assistant, vec![Block::text("a1"), tool_call("c1")]),
        message(Role::Tool, vec![tool_result("c1")]),
        message(Role::User, vec![Block::text("u3")]),
        message(Role::Assistant, vec![Block::text("a4")]),
        message(Role::User, vec![Block::text("u5")]),
    ]
}

#[test]
fn a_boundary_marks_the_last_block_of_its_prefix_and_the_last_block() {
    let body = encode(conversation(), vec![2], "sys");
    // Prefix of 2 source messages ends with the tool call block at wire message 1, block 1.
    assert_eq!(marked(&body), vec![(1, 1), (4, 0)]);
    assert_eq!(
        body["messages"][1]["content"][1]["cache_control"],
        json!({"type":"ephemeral"})
    );
}

#[test]
fn a_boundary_inside_a_coalesced_wire_message_marks_the_block_before_the_next_source_message() {
    // The tool result and the following user message share one wire message.
    let body = encode(conversation(), vec![3], "sys");
    assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(marked(&body), vec![(2, 0), (4, 0)]);
}

#[test]
fn a_boundary_message_ending_in_reasoning_marks_the_last_emitted_block() {
    let mut messages = conversation();
    messages[1] = message(
        Role::Assistant,
        vec![Block::text("a1"), Block::reasoning("hidden")],
    );
    messages.remove(2);
    // Source prefix of 2 ends with a reasoning block that is not emitted.
    let body = encode(messages, vec![2], "sys");
    assert_eq!(marked(&body)[0], (1, 0));
    assert_eq!(body["messages"][1]["content"][0]["text"], "a1");
}

#[test]
fn a_boundary_message_with_no_emitted_blocks_marks_the_previous_block() {
    let messages = vec![
        message(Role::User, vec![Block::text("u0")]),
        message(Role::Assistant, vec![Block::reasoning("hidden")]),
        message(Role::User, vec![Block::text("u2")]),
    ];
    let body = encode(messages, vec![2], "sys");
    // The user messages around the empty assistant message coalesce into wire message 0.
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(marked(&body), vec![(0, 0), (0, 1)]);
}

#[test]
fn no_boundaries_mark_only_the_system_text_and_the_last_block() {
    let body = encode(conversation(), Vec::new(), "sys");
    assert!(body.get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(body["system"][0].get("cache_control").is_some());
    assert_eq!(marked(&body), vec![(4, 0)]);
    assert_eq!(body.to_string().matches("cache_control").count(), 2);
}

#[test]
fn boundaries_beyond_the_messages_are_ignored_and_duplicates_collapse() {
    let body = encode(conversation(), vec![6, 6, 9, 99], "sys");
    assert_eq!(marked(&body), vec![(4, 0)]);
}

#[test]
fn many_boundaries_keep_at_most_four_breakpoints_and_drop_the_earliest() {
    let messages: Vec<Message> = (0..12)
        .map(|index| {
            let role = if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            message(role, vec![Block::text(format!("m{index}"))])
        })
        .collect();
    let body = encode(messages.clone(), (1..12).collect(), "sys");
    assert_eq!(body.to_string().matches("cache_control").count(), 4);
    // System plus the latest three: boundaries 10, 11 and the last block (12 messages).
    assert_eq!(marked(&body), vec![(9, 0), (10, 0), (11, 0)]);
    let no_system = encode(messages, (1..12).collect(), "");
    assert_eq!(no_system.to_string().matches("cache_control").count(), 4);
    assert_eq!(marked(&no_system), vec![(8, 0), (9, 0), (10, 0), (11, 0)]);
}
