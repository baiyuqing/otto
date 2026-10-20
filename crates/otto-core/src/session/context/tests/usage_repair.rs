//! Context tests: usage totals, fixture resolution and dangling tool-call repair.

use super::*;

test!(build_context_usage_comes_from_the_full_active_path {
    let assistant = assistant_entry(
        "90000001",
        None,
        "answer",
        "openai-compatible",
        "model",
        10,
        3,
        "stop",
    );
    let mut branch = entry("branch_summary", "90000002", Some("90000001"));
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "90000001".into(),
        summary: "summary".into(),
        usage: Some(pi_usage(4, 2)),
        ..PiBranchSummary::default()
    }));
    let mut compaction = entry("compaction", "90000003", Some("90000002"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "compact".into(),
        tokens_before: 10,
        retained_tail: Some(Vec::new()),
        usage: Some(pi_usage(6, 1)),
        ..PiCompaction::default()
    }));

    let (context, _) = build_context(&[assistant, branch, compaction], "90000003")
        .expect("build context");
    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 6,
            cached_input_tokens: 0,
        }
    );
});

test!(build_context_counts_a_task_notification_usage {
    let assistant = assistant_entry(
        "91000001",
        None,
        "answer",
        "openai-compatible",
        "model",
        10,
        3,
        "stop",
    );
    let task_usage = Usage {
        input_tokens: 40,
        output_tokens: 5,
        cached_input_tokens: 30,
    };
    let mut notification = entry("custom_message", "91000002", Some("91000001"));
    notification.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: TASK_NOTIFICATION_CONTEXT_TYPE.into(),
        content_text: Some("[task-notification] task t1 succeeded".into()),
        display: true,
        details: Some(
            encode_pi_otto_details(&PiOttoDetails {
                task_id: "t1".into(),
                usage: Some(task_usage),
                ..PiOttoDetails::default()
            })
            .expect("encode details"),
        ),
        ..PiCustomMessage::default()
    }));

    let (context, _) =
        build_context(&[assistant, notification], "91000002").expect("build context");

    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 50,
            output_tokens: 8,
            cached_input_tokens: 30,
        },
        "the sub-agent's tokens belong to the session totals"
    );
    assert!(context.usage_present);
    assert_eq!(
        context.messages[1].usage,
        Some(task_usage),
        "the replayed notification also carries its own usage"
    );
});

// Every fixture under `testdata/session/pi-v3` resolves to the message
// sequence below. `unknown-entry.jsonl` has no resolution expectation of
// its own (its codec test covers raw-JSON preservation only), so the roles
// and count are snapshotted here.
test!(every_fixture_resolves_to_the_expected_message_sequence {
    let cases: &[(&str, &[(Role, &str)])] = &[
        (
            "linear.jsonl",
            &[
                (Role::User, "run the check"),
                (Role::Assistant, "I will read it."),
                (Role::Tool, ""),
                (Role::Assistant, "The check passed."),
            ],
        ),
        (
            "tree.jsonl",
            &[
                (Role::User, "root"),
                (Role::Context, "[Branch summary]\ninactive work was abandoned"),
                (Role::User, "active branch"),
            ],
        ),
        (
            "compacted.jsonl",
            &[
                (Role::Context, "[Compaction summary]\nsummary"),
                (Role::User, "retained"),
                (Role::User, "after"),
            ],
        ),
        (
            "compaction-dual-form.jsonl",
            &[
                (Role::Context, "[Compaction summary]\ndual summary"),
                (Role::User, "real retained path"),
                (Role::User, "after dual checkpoint"),
            ],
        ),
        (
            "compaction-retained-tail-only.jsonl",
            &[
                (Role::Context, "[Compaction summary]\nretained-tail summary"),
                (Role::User, "synthetic retained tail"),
            ],
        ),
        ("unknown-entry.jsonl", &[]),
    ];
    assert_eq!(cases.len(), 6, "every fixture must be covered");
    for (name, want) in cases {
        let file = read_pi_fixture(name);
        let leaf = file.entries.last().expect("fixture entries").id.clone();
        let (context, _) = build_context(&file.entries, &leaf)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let got: Vec<(Role, String)> = context
            .messages
            .iter()
            .map(|message| (message.role.clone(), message.text()))
            .collect();
        let want: Vec<(Role, String)> = want
            .iter()
            .map(|(role, text)| (role.clone(), (*text).to_owned()))
            .collect();
        assert_eq!(got, want, "{name}");
    }
});
test!(build_context_repairs_a_tool_call_the_history_left_behind {
    let root = user_entry("63100001", None, "root");
    let assistant = tool_call_entry("63100002", Some("63100001"), &[("call-1", "read")]);
    let next = user_entry("63100003", Some("63100002"), "next");

    let (context, warnings) =
        build_context(&[root, assistant, next], "63100003").expect("build context");

    let roles: Vec<Role> = context.messages.iter().map(|message| message.role.clone()).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::User],
        "the stand-in result belongs next to its call"
    );
    let repaired = &context.messages[2];
    assert_eq!(repaired.blocks.len(), 1);
    assert_eq!(repaired.blocks[0].tool_call_id, "call-1");
    assert_eq!(repaired.blocks[0].tool_name, "read");
    assert!(repaired.blocks[0].is_error);
    assert_eq!(repaired.blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
    assert_eq!(repaired.created_at, context.messages[1].created_at);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("call-1")),
        "the repair must be reported: {warnings:?}"
    );
});

test!(build_context_marks_only_the_first_unanswered_call_as_possibly_run {
    let root = user_entry("63120001", None, "root");
    let assistant = tool_call_entry(
        "63120002",
        Some("63120001"),
        &[("call-1", "read"), ("call-2", "bash"), ("call-3", "write")],
    );
    let result = tool_result_entry("63120003", Some("63120002"), "call-1", "read", "ok");
    let next = user_entry("63120004", Some("63120003"), "next");

    let (context, _) = build_context(&[root, assistant, result, next], "63120004")
        .expect("build context");

    let stand_ins: Vec<(&str, &str)> = context.messages[3..5]
        .iter()
        .map(|message| {
            let block = &message.blocks[0];
            (block.tool_call_id.as_str(), block.text.as_str())
        })
        .collect();
    assert_eq!(
        stand_ins,
        vec![
            ("call-2", MAY_HAVE_RUN_TOOL_RESULT_TEXT),
            ("call-3", NOT_EXECUTED_TOOL_RESULT_TEXT),
        ]
    );
});

test!(build_context_projects_dangling_repairs_from_the_operation_ledger {
    let cases = [
        (
            "terminal",
            vec![
                user_entry("63200001", None, "root"),
                tool_call_entry("63200002", Some("63200001"), &[("call-1", "read")]),
                operation_entry(
                    "63200003",
                    Some("63200002"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_terminal","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                operation_entry(
                    "63200004",
                    Some("63200003"),
                    concat!(
                        r#"{"event":"terminal","schemaVersion":1,"operationId":"op_terminal","#,
                        r#""attempt":1,"kind":"tool_call","toolCallId":"call-1","#,
                        r#""toolName":"read","disposition":"succeeded","#,
                        r#""effectCertainty":"completed"}"#,
                    ),
                ),
                user_entry("63200005", Some("63200004"), "next"),
            ],
            "63200005",
            Some("op_terminal"),
            OperationDisposition::Succeeded,
            EffectCertainty::Completed,
            None,
            None,
        ),
        (
            "attempt",
            vec![
                user_entry("63200011", None, "root"),
                tool_call_entry("63200012", Some("63200011"), &[("call-1", "read")]),
                operation_entry(
                    "63200013",
                    Some("63200012"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_attempt","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                user_entry("63200014", Some("63200013"), "next"),
            ],
            "63200014",
            Some("op_attempt"),
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            None,
        ),
        (
            "legacy",
            vec![
                user_entry("63200021", None, "root"),
                tool_call_entry(
                    "63200022",
                    Some("63200021"),
                    &[("call-1", "read"), ("call-2", "write")],
                ),
                user_entry("63200023", Some("63200022"), "next"),
            ],
            "63200023",
            None,
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            None,
        ),
        (
            "corrupt",
            vec![
                user_entry("63200031", None, "root"),
                tool_call_entry("63200032", Some("63200031"), &[("call-1", "read")]),
                operation_entry(
                    "63200033",
                    Some("63200032"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_corrupt","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                operation_entry(
                    "63200034",
                    Some("63200033"),
                    r#"{"event":"attempt","schemaVersion":1,"operationId":"op_corrupt","attempt":3,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#,
                ),
                user_entry("63200035", Some("63200034"), "next"),
            ],
            "63200035",
            Some("op_corrupt"),
            OperationDisposition::Interrupted,
            EffectCertainty::Unknown,
            Some(OperationStopReason::ProcessLost),
            Some("corrupt"),
        ),
    ];

    for (
        name,
        entries,
        leaf,
        operation_id,
        disposition,
        effect_certainty,
        stop_reason,
        warning,
    ) in cases
    {
        let (context, warnings) =
            build_context(&entries, leaf).unwrap_or_else(|error| panic!("{name}: {error}"));
        let repairs: Vec<&Block> = context
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| &message.blocks)
            .collect();
        let metadata = repairs[0]
            .operation_metadata
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: missing operation metadata"));
        assert_eq!(
            metadata.operation_id.as_ref().map(|id| id.as_str()),
            operation_id,
            "{name}"
        );
        assert_eq!(metadata.disposition, disposition, "{name}");
        assert_eq!(metadata.effect_certainty, effect_certainty, "{name}");
        assert_eq!(metadata.stop_reason, stop_reason, "{name}");
        if name == "legacy" {
            assert_eq!(repairs.len(), 2);
            let second = repairs[1].operation_metadata.as_ref().expect("metadata");
            assert_eq!(second.operation_id, None);
            assert_eq!(second.effect_certainty, EffectCertainty::NotStarted);
        }
        if let Some(warning) = warning {
            assert_warning_contains(&warnings, warning);
        }
    }
});

test!(build_context_correlates_dangling_repairs_across_branch_and_compaction_views {
    let root = user_entry("63300001", None, "root");
    let assistant = tool_call_entry("63300002", Some("63300001"), &[("branch-call", "read")]);
    let attempt = operation_entry(
        "63300003",
        Some("63300002"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_branch","attempt":1,"kind":"tool_call","toolCallId":"branch-call","toolName":"read"}"#,
    );
    let mut branch = entry("branch_summary", "63300004", Some("63300003"));
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "63300001".into(),
        summary: "branch moved on".into(),
        ..PiBranchSummary::default()
    }));
    let inactive = operation_entry(
        "63300005",
        Some("63300001"),
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_inactive","attempt":1,"kind":"tool_call","toolCallId":"branch-call","toolName":"read"}"#,
    );
    let (branch_context, _) = build_context(
        &[root, assistant, attempt, branch, inactive],
        "63300004",
    )
    .expect("branch context");
    let branch_metadata = branch_context.messages[2].blocks[0]
        .operation_metadata
        .as_ref()
        .expect("branch repair metadata");
    assert_eq!(
        branch_metadata.operation_id.as_ref().map(|id| id.as_str()),
        Some("op_branch")
    );

    let attempt = operation_entry(
        "63300011",
        None,
        r#"{"event":"attempt","schemaVersion":1,"operationId":"op_compacted","attempt":1,"kind":"tool_call","toolCallId":"retained-call","toolName":"bash"}"#,
    );
    let retained = tool_call_entry("ignored01", None, &[("retained-call", "bash")])
        .message
        .expect("retained message");
    let mut compaction = entry("compaction", "63300012", Some("63300011"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "summary".into(),
        retained_tail: Some(vec![*retained]),
        ..PiCompaction::default()
    }));
    let next = user_entry("63300013", Some("63300012"), "next");
    let (compacted, _) = build_context(&[attempt, compaction, next], "63300013")
        .expect("compacted context");
    let repair = compacted
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("retained-tail repair");
    let metadata = repair.blocks[0]
        .operation_metadata
        .as_ref()
        .expect("compacted repair metadata");
    assert_eq!(
        metadata.operation_id.as_ref().map(|id| id.as_str()),
        Some("op_compacted")
    );
    assert_eq!(metadata.effect_certainty, EffectCertainty::Unknown);
});

test!(build_context_leaves_a_trailing_tool_call_for_the_durable_repair {
    let (root, assistant) = single_tool_call("63110001", "63110002");

    let (context, warnings) = build_context(&[root, assistant], "63110002").expect("build context");

    assert_eq!(context.messages.len(), 2, "{:?}", context.messages);
    assert!(warnings.is_empty(), "{warnings:?}");
    let pending = pending_tool_calls(&context.messages).expect("pending tool calls");
    assert_eq!(pending.len(), 1);
});

test!(build_context_still_rejects_a_tool_result_without_a_call {
    let root = user_entry("63120001", None, "root");
    let orphan = tool_result_entry("63120002", Some("63120001"), "call-1", "read", "done");

    let error = build_context(&[root, orphan], "63120002").expect_err("must reject");

    assert_eq!(error.kind(), PiErrorKind::Invalid);
});
