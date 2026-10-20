//! Context tests: operation facts and tool call/result pairing.

use super::*;

test!(build_context_folds_only_active_operation_facts {
    let root = user_entry("60000101", None, "root");
    let active = operation_entry(
        "60000102",
        Some("60000101"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_active","attempt":1,"kind":"tool_call","toolCallId":"call-active","toolName":"read"}"#,
    );
    let leaf = user_entry("60000103", Some("60000102"), "leaf");
    let inactive = operation_entry(
        "60000104",
        Some("60000101"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_inactive","attempt":1,"kind":"tool_call","toolCallId":"call-inactive","toolName":"write"}"#,
    );
    let (context, _) = build_context(&[root, active, leaf, inactive], "60000103")
        .expect("build context");
    assert_eq!(texts(&context.messages), ["root", "leaf"]);
    assert!(context
        .operation_ledger
        .operation_for_tool_call("call-active")
        .is_some());
    assert!(context
        .operation_ledger
        .operation_for_tool_call("call-inactive")
        .is_none());
});

test!(build_context_ignores_unknown_operation_schema_but_rejects_malformed_v1 {
    let root = user_entry("60000111", None, "root");
    let future = operation_entry(
        "60000112",
        Some("60000111"),
        r#"{"schemaVersion":2,"future":true}"#,
    );
    build_context(&[root.clone(), future], "60000112").expect("future schema ignored");

    let malformed = operation_entry(
        "60000113",
        Some("60000111"),
        r#"{"schemaVersion":1,"event":"attempt"}"#,
    );
    let error = build_context(&[root, malformed], "60000113")
        .expect_err("malformed known schema accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_warns_and_marks_operation_conflicts_corrupt {
    let root = operation_entry(
        "60000121",
        None,
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_1","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
    );
    let conflict = operation_entry(
        "60000122",
        Some("60000121"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_2","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
    );
    let (context, warnings) =
        build_context(&[root, conflict], "60000122").expect("tolerant fold");
    assert_warning_contains(&warnings, "different operation");
    let record = context
        .operation_ledger
        .operation_for_tool_call("call-1")
        .expect("first operation retained");
    assert!(record.corrupt);
});

test!(build_context_preserves_tool_call_result_pairing {
    let context = context_from_fixture("linear.jsonl");
    assert_eq!(context.messages.len(), 4);
    let call = &context.messages[1].blocks[1];
    let result = &context.messages[2].blocks[0];
    assert_eq!(call.block_type, BlockType::ToolCall);
    assert_eq!(result.block_type, BlockType::ToolResult);
    assert_eq!(call.tool_call_id, result.tool_call_id);
    assert_eq!(call.tool_name, result.tool_name);
});

test!(build_context_allows_sibling_tool_results_before_later_context {
    let root = user_entry("61000001", None, "root");
    let assistant = tool_call_entry(
        "61000002",
        Some("61000001"),
        &[("call-1", "read"), ("call-2", "write")],
    );
    let second_result =
        tool_result_entry("61000003", Some("61000002"), "call-2", "write", "second");
    let first_result =
        tool_result_entry("61000004", Some("61000003"), "call-1", "read", "first");
    let mut visible = entry("custom_message", "61000005", Some("61000004"));
    visible.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "visible.fixture".into(),
        content_text: Some("after tools".into()),
        display: true,
        ..PiCustomMessage::default()
    }));
    let user = user_entry("61000006", Some("61000005"), "continue");

    let entries = vec![
        root,
        assistant,
        second_result,
        first_result,
        visible,
        user,
    ];
    let (context, _) = build_context(&entries, "61000006").expect("build context");
    assert_eq!(
        texts(&context.messages),
        [
            "root",
            "",
            "",
            "",
            "[Custom context: visible.fixture]\nafter tools",
            "continue",
        ]
    );
    assert_eq!(context.messages[2].blocks[0].text, "second");
    assert_eq!(context.messages[3].blocks[0].text, "first");
});

test!(build_context_rejects_invalid_tool_result_ordering {
    let unmatched = {
        let root = user_entry("62000001", None, "root");
        let result =
            tool_result_entry("62000002", Some("62000001"), "missing", "read", "result");
        (vec![root, result], "62000002")
    };
    let duplicate_result = {
        let (root, assistant) = single_tool_call("62000003", "62000004");
        let result =
            tool_result_entry("62000005", Some("62000004"), "call-1", "read", "result");
        let duplicate = tool_result_entry(
            "62000006",
            Some("62000005"),
            "call-1",
            "read",
            "duplicate",
        );
        (vec![root, assistant, result, duplicate], "62000006")
    };
    let mismatched_name = {
        let (root, assistant) = single_tool_call("62000014", "62000015");
        let result =
            tool_result_entry("62000016", Some("62000015"), "call-1", "write", "result");
        (vec![root, assistant, result], "62000016")
    };
    let duplicate_call = {
        let root = user_entry("62000017", None, "root");
        let assistant = tool_call_entry(
            "62000018",
            Some("62000017"),
            &[("call-1", "read"), ("call-1", "read")],
        );
        (vec![root, assistant], "62000018")
    };
    let interposed_context = {
        let root = user_entry("62000007", None, "root");
        let assistant = tool_call_entry(
            "62000008",
            Some("62000007"),
            &[("call-1", "read"), ("call-2", "write")],
        );
        let result =
            tool_result_entry("62000009", Some("62000008"), "call-1", "read", "first");
        let mut interposed = entry("custom_message", "6200000a", Some("62000009"));
        interposed.custom_message = Some(Box::new(PiCustomMessage {
            custom_type: "provider-visible".into(),
            content_text: Some("not yet".into()),
            display: false,
            ..PiCustomMessage::default()
        }));
        let second =
            tool_result_entry("6200000b", Some("6200000a"), "call-2", "write", "second");
        (
            vec![root, assistant, result, interposed, second],
            "6200000b",
        )
    };
    let interposed_user = {
        let (root, assistant) = single_tool_call("6200000c", "6200000d");
        let user = user_entry("6200000e", Some("6200000d"), "too soon");
        let result =
            tool_result_entry("6200000f", Some("6200000e"), "call-1", "read", "result");
        (vec![root, assistant, user, result], "6200000f")
    };
    let interposed_assistant = {
        let (root, assistant) = single_tool_call("62000010", "62000011");
        let interposed = assistant_entry(
            "62000012",
            Some("62000011"),
            "too soon",
            "openai-compatible",
            "model",
            1,
            1,
            "stop",
        );
        let result =
            tool_result_entry("62000013", Some("62000012"), "call-1", "read", "result");
        (vec![root, assistant, interposed, result], "62000013")
    };

    let cases = [
        ("unmatched result", unmatched),
        ("duplicate result", duplicate_result),
        ("mismatched result name", mismatched_name),
        ("duplicate call", duplicate_call),
        (
            "interposed context with sibling outstanding",
            interposed_context,
        ),
        ("interposed user", interposed_user),
        ("interposed assistant", interposed_assistant),
    ];
    assert_eq!(cases.len(), 7);
    for (name, (entries, leaf)) in cases {
        let error = build_context(&entries, leaf).expect_err("case {name} accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
    }
});

test!(build_context_allows_dangling_tool_calls_only_at_the_final_boundary {
    let root = user_entry("63000001", None, "root");
    let assistant = tool_call_entry(
        "63000002",
        Some("63000001"),
        &[("call-1", "read"), ("call-2", "write")],
    );
    let (context, _) =
        build_context(&[root, assistant], "63000002").expect("build context");
    let pending = pending_tool_calls(&context.messages).expect("pending tool calls");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].tool_call_id, "call-1");
    assert_eq!(pending[1].tool_call_id, "call-2");
});

test!(pending_tool_calls_permits_only_tool_result_blocks {
    let assistant = Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            ..Block::default()
        }],
        ..Message::default()
    };
    let cases = [
        (
            "empty tool message",
            Message {
                role: Role::Tool,
                ..Message::default()
            },
        ),
        (
            "non-result block",
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::Text,
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    text: "not a result".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
        ),
    ];
    for (name, tool_message) in cases {
        let error = pending_tool_calls(&[assistant.clone(), tool_message])
            .expect_err("case {name} accepted");
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}");
    }
});

test!(build_context_preserves_the_session_info_name_exactly {
    let mut built = entry("session_info", "64000001", None);
    built.session_info = Some(PiSessionInfo {
        name: Some("  exact session name\t ".into()),
    });
    let (context, _) = build_context(&[built], "64000001").expect("build context");
    assert_eq!(context.session_name, "  exact session name\t ");
});

test!(build_context_rejects_a_pending_pi_assistant {
    let pending = assistant_entry(
        "70000001",
        None,
        "partial",
        "openai-compatible",
        "model",
        0,
        0,
        "pending",
    );
    let error = build_context(&[pending], "70000001").expect_err("pending accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_rejects_duplicate_forward_parent_and_cycle {
    let root = user_entry("80000001", None, "root");
    let duplicate = user_entry("80000001", None, "duplicate");
    let error = build_context(&[root, duplicate], "80000001").expect_err("duplicate accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);

    let forward = user_entry("80000002", Some("80000003"), "forward");
    let future = user_entry("80000003", None, "future");
    let error = build_context(&[forward, future], "80000003").expect_err("forward accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);

    let cycle = user_entry("80000004", Some("80000004"), "cycle");
    let error = build_context(&[cycle], "80000004").expect_err("cycle accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});
