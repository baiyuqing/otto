
use super::*;

fn apply(events: &[&str]) -> Vec<Item> {
    let mut items = Vec::new();
    for event in events {
        items = reduce_json(&items, event).expect("event decodes");
    }
    items
}

fn assistant(text: &str) -> Item {
    Item::Assistant {
        text: text.into(),
        created_at: String::new(),
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        created_at: String::new(),
    }
}

fn tool(id: &str, name: &str, args: &str, result: Option<&str>, is_error: Option<bool>) -> Item {
    Item::Tool {
        id: id.into(),
        name: name.into(),
        args: args.into(),
        result: result.map(str::to_string),
        is_error,
        operation_id: None,
        disposition: None,
        effect_certainty: None,
        stop_reason: None,
        created_at: String::new(),
    }
}

#[test]
fn merges_text_deltas_into_one_assistant_item() {
    let items = apply(&[
        r#"{"type":"agent_started"}"#,
        r#"{"type":"text_delta","text":"hel"}"#,
        r#"{"type":"text_delta","text":"lo"}"#,
        r#"{"type":"agent_finished"}"#,
    ]);
    assert_eq!(items, vec![assistant("hello")]);
}

fn reasoning(text: &str) -> Item {
    Item::Reasoning {
        text: text.into(),
        created_at: String::new(),
    }
}

#[test]
fn merges_reasoning_deltas_into_one_item_before_the_reply() {
    let items = apply(&[
        r#"{"type":"reasoning_delta","text":"weigh "}"#,
        r#"{"type":"reasoning_delta","text":"options"}"#,
        r#"{"type":"text_delta","text":"ok"}"#,
    ]);
    assert_eq!(items, vec![reasoning("weigh options"), assistant("ok")]);
}

#[test]
fn history_reasoning_is_not_rendered_as_reply_text() {
    let items = from_history(
        r#"[{"role":"assistant","blocks":[
              {"type":"reasoning","text":"weigh options"},
              {"type":"text","text":"ok"}]}]"#,
    )
    .expect("decodes");
    assert_eq!(items, vec![reasoning("weigh options"), assistant("ok")]);
}

#[test]
fn starts_a_new_assistant_item_after_a_tool_call() {
    let items = apply(&[
        r#"{"type":"text_delta","text":"first"}"#,
        r#"{"type":"tool_call_started","tool_call_id":"c1","tool_name":"bash","tool_args":{"command":"ls"}}"#,
        r#"{"type":"tool_call_finished","tool_call_id":"c1","tool_name":"bash","result":{"content":"a\nb","is_error":false}}"#,
        r#"{"type":"text_delta","text":"second"}"#,
    ]);
    assert_eq!(
        items,
        vec![
            assistant("first"),
            tool(
                "c1",
                "bash",
                "{\n  \"command\": \"ls\"\n}",
                Some("a\nb"),
                Some(false)
            ),
            assistant("second"),
        ]
    );
}

#[test]
fn records_an_unmatched_tool_call_finished_as_its_own_item() {
    let items = apply(&[
        r#"{"type":"tool_call_finished","tool_call_id":"c9","tool_name":"read","result":{"content":"x","is_error":true}}"#,
    ]);
    assert_eq!(items, vec![tool("c9", "read", "", Some("x"), Some(true))]);
}

#[test]
fn preserves_typed_tool_outcome_from_live_events() {
    let items = apply(&[
        r#"{"type":"tool_call_started","tool_call_id":"c1","tool_name":"bash","operation_id":"op_1","attempt":1,"tool_args":{"command":"touch marker"}}"#,
        r#"{"type":"tool_call_finished","tool_call_id":"c1","tool_name":"bash","operation_id":"op_1","attempt":1,"result":{"content":"transport lost","is_error":true,"operation_id":"op_1","disposition":"interrupted","effect_certainty":"unknown","stop_reason":"transport_lost"}}"#,
    ]);
    let Item::Tool {
        operation_id,
        disposition,
        effect_certainty,
        stop_reason,
        ..
    } = &items[0]
    else {
        panic!("expected tool")
    };
    assert_eq!(operation_id.as_deref(), Some("op_1"));
    assert_eq!(*disposition, Some(OperationDisposition::Interrupted));
    assert_eq!(*effect_certainty, Some(EffectCertainty::Unknown));
    assert_eq!(*stop_reason, Some(OperationStopReason::TransportLost));
}

#[test]
fn reopened_history_preserves_typed_tool_outcome() {
    let items = from_history(
            r#"[{"role":"assistant","blocks":[{"type":"tool_call","tool_call_id":"c1","tool_name":"read","arguments":{"path":"README.md"}}]},{"role":"tool","blocks":[{"type":"tool_result","tool_call_id":"c1","tool_name":"read","text":"ok","operation_id":"op_2","disposition":"succeeded","effect_certainty":"completed"}]}]"#,
        )
        .expect("history decodes");
    let Item::Tool {
        operation_id,
        disposition,
        effect_certainty,
        stop_reason,
        ..
    } = &items[0]
    else {
        panic!("expected tool")
    };
    assert_eq!(operation_id.as_deref(), Some("op_2"));
    assert_eq!(*disposition, Some(OperationDisposition::Succeeded));
    assert_eq!(*effect_certainty, Some(EffectCertainty::Completed));
    assert_eq!(*stop_reason, None);
}

#[test]
fn legacy_tool_result_does_not_infer_typed_outcome() {
    let items = apply(&[
        r#"{"type":"tool_call_finished","tool_call_id":"c1","tool_name":"bash","result":{"content":"failed","is_error":true}}"#,
    ]);
    let Item::Tool {
        operation_id,
        disposition,
        effect_certainty,
        stop_reason,
        ..
    } = &items[0]
    else {
        panic!("expected tool")
    };
    assert_eq!(operation_id, &None);
    assert_eq!(disposition, &None);
    assert_eq!(effect_certainty, &None);
    assert_eq!(stop_reason, &None);
}

#[test]
fn does_not_mutate_the_previous_transcript() {
    let before = vec![assistant("a")];
    let after = reduce_json(&before, r#"{"type":"text_delta","text":"b"}"#).expect("decodes");
    assert_eq!(before, vec![assistant("a")]);
    assert_eq!(after, vec![assistant("ab")]);
}

#[test]
fn adds_a_notice_for_a_real_compaction_and_nothing_for_a_noop() {
    let done = r#"{"type":"compaction_completed","compaction":{"reason":"threshold","tokens_before":900,"estimated_tokens_after":300,"automatic":true,"noop":false}}"#;
    assert_eq!(
        apply(&[done]),
        vec![Item::Notice {
            text: "Context compacted: 900 → ~300 tokens (threshold)".into(),
        }]
    );
    let noop = r#"{"type":"compaction_completed","compaction":{"reason":"threshold","tokens_before":900,"estimated_tokens_after":300,"automatic":true,"noop":true}}"#;
    assert_eq!(apply(&[noop]), vec![]);
}

#[test]
fn user_message_adds_a_user_item_unless_empty() {
    assert_eq!(
        apply(&[
            r#"{"type":"user_message","text":"hi","image":true}"#,
            r#"{"type":"user_message","text":""}"#,
        ]),
        vec![user("hi")]
    );
}

#[test]
fn turn_end_adds_an_error_item_only_for_status_error() {
    assert_eq!(
        apply(&[
            r#"{"type":"turn_end","status":"ok"}"#,
            r#"{"type":"turn_end","status":"canceled"}"#,
        ]),
        vec![]
    );
    assert_eq!(
        apply(&[r#"{"type":"turn_end","error":"boom","status":"error"}"#]),
        vec![Item::Error {
            text: "boom".into()
        }]
    );
    assert_eq!(
        apply(&[
            r#"{"type":"agent_error","error":"provider: 500"}"#,
            r#"{"type":"turn_end","error":"provider: 500","status":"error"}"#,
        ]),
        vec![Item::Error {
            text: "provider: 500".into()
        }]
    );
}

#[test]
fn approval_frames_become_notices() {
    let items = apply(&[
        r#"{"type":"approval_requested","tool_call_id":"c1","approval_id":"a1","command":"ls","justification":"why"}"#,
        r#"{"type":"approval_decided","approval_id":"a1","decision":"timeout"}"#,
    ]);
    assert_eq!(items.len(), 2);
    let Item::Notice { text } = &items[0] else {
        panic!("expected notice, got {:?}", items[0]);
    };
    assert!(text.contains("ls") && text.contains("why"), "{text}");
    assert!(
        text.contains("/approve a1") && text.contains("/deny a1"),
        "{text}"
    );
    assert_eq!(
        items[1],
        Item::Notice {
            text: "Approval a1: timeout".into()
        }
    );
}

#[test]
fn turns_agent_error_into_an_error_item() {
    assert_eq!(
        apply(&[r#"{"type":"agent_error","error":"provider: 500"}"#]),
        vec![Item::Error {
            text: "provider: 500".into(),
        }]
    );
}

#[test]
fn notification_without_text_names_the_task() {
    assert_eq!(
        apply(&[r#"{"type":"notification","task_id":"t1"}"#]),
        vec![Item::Notice {
            text: "Task t1 finished".into(),
        }]
    );
}

#[test]
fn warnings_fall_back_to_the_error_field_then_the_type() {
    assert_eq!(
        apply(&[r#"{"type":"memory_warning","error":"recall failed"}"#]),
        vec![Item::Notice {
            text: "recall failed".into(),
        }]
    );
    assert_eq!(
        apply(&[r#"{"type":"compaction_warning"}"#]),
        vec![Item::Notice {
            text: "compaction_warning".into(),
        }]
    );
}

#[test]
fn pairs_tool_results_and_shows_only_display_context() {
    let history = r#"[
          {"id":"1","role":"user","created_at":"","blocks":[{"type":"text","text":"list files"}]},
          {"id":"2","role":"assistant","created_at":"","blocks":[
            {"type":"text","text":"Running ls."},
            {"type":"tool_call","tool_call_id":"c1","tool_name":"bash","arguments":{"command":"ls"}}]},
          {"id":"3","role":"tool","created_at":"","blocks":[
            {"type":"tool_result","tool_call_id":"c1","tool_name":"bash","text":"a.go","is_error":false}]},
          {"id":"4","role":"context","created_at":"","context_type":"memory","blocks":[{"type":"text","text":"hidden recall"}]},
          {"id":"5","role":"context","created_at":"","display":true,"blocks":[{"type":"text","text":"Task t1 finished"}]},
          {"id":"6","role":"assistant","created_at":"","blocks":[{"type":"text","text":"One file."}]}
        ]"#;
    assert_eq!(
        from_history(history).expect("history decodes"),
        vec![
            user("list files"),
            assistant("Running ls."),
            tool(
                "c1",
                "bash",
                "{\n  \"command\": \"ls\"\n}",
                Some("a.go"),
                Some(false)
            ),
            Item::Notice {
                text: "Task t1 finished".into()
            },
            assistant("One file."),
        ]
    );
}

#[test]
fn preserves_user_images_for_display() {
    let history = r#"[{"role":"user","blocks":[{"type":"image","data":"iVBORw0KGgo=","mime_type":"image/png"}]}]"#;
    let items = from_history(history).expect("history decodes");
    assert_eq!(
        serde_json::to_value(items).expect("items encode"),
        serde_json::json!([{
            "kind": "image",
            "data": "iVBORw0KGgo=",
            "mime_type": "image/png"
        }])
    );
}

#[test]
fn format_args_matches_json_stringify_with_two_space_indent() {
    let cases = [
        ("null", ""),
        (r#""plain text""#, "plain text"),
        ("{}", "{}"),
        ("[]", "[]"),
        (r#"{"b":1,"a":2}"#, "{\n  \"b\": 1,\n  \"a\": 2\n}"),
        (r#"[1,2]"#, "[\n  1,\n  2\n]"),
        (
            r#"{"a":{"b":[1,{"c":null}]}}"#,
            "{\n  \"a\": {\n    \"b\": [\n      1,\n      {\n        \"c\": null\n      }\n    ]\n  }\n}",
        ),
        (r#"{"s":"a\"b,c:d"}"#, "{\n  \"s\": \"a\\\"b,c:d\"\n}"),
    ];
    for (input, want) in cases {
        let raw = RawValue::from_string(input.to_string()).expect("valid json");
        assert_eq!(format_args(Some(&raw)), want, "input {input}");
    }
    assert_eq!(format_args(None), "");
}

#[test]
fn argument_display_decodes_strings_without_changing_json_values() {
    let input = r#"{"\u8def\u5f84":"\u4e2d\u6587/\ud83d\ude80\u0026\u003c\u003e","literal":"\\u0026","controls":"\u001b\u000a\u0009","quote":"\u0022\\","n":123456789012345678901234567890,"exp":1e+09}"#;
    let raw = RawValue::from_string(input.into()).unwrap();
    let expected = r#"{"路径":"中文/🚀&<>","literal":"\\u0026","controls":"\u001b\n\t","quote":"\"\\","n":123456789012345678901234567890,"exp":1e+09}"#;
    assert_eq!(format_args_compact(Some(&raw)), expected);
    let pretty = format_args(Some(&raw));
    assert!(pretty.contains(r#""路径": "中文/🚀&<>""#), "{pretty}");
    assert!(pretty.contains(r#""literal": "\\u0026""#), "{pretty}");
    assert!(pretty.contains("123456789012345678901234567890"));
    assert!(pretty.contains("1e+09"));
    // The display is still valid JSON and can safely be parsed by Web summaries.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pretty).unwrap(),
        serde_json::from_str::<serde_json::Value>(input).unwrap(),
    );
}

#[test]
fn compact_string_arguments_keep_quotes_backslashes_and_controls() {
    let raw = RawValue::from_string(r#""\u4e2d\u6587\u0026\\u0026\u001b\u000a""#.into()).unwrap();
    assert_eq!(format_args_compact(Some(&raw)), r#""中文&\\u0026\u001b\n""#);
}

#[test]
fn argument_display_preserves_unpaired_surrogates_without_panicking() {
    // RawValue accepts JSON syntax without requiring strings to be UTF-8 decodable.
    let raw = RawValue::from_string(r#"{"s":"\ud800"}"#.into()).unwrap();
    assert_eq!(format_args_compact(Some(&raw)), raw.get());
    assert_eq!(format_args(Some(&raw)), "{\n  \"s\": \"\\ud800\"\n}");
}

#[test]
fn live_history_and_phase_decode_argument_escapes() {
    let args =
        r#"{"command":"echo \u4e2d\u6587\ud83d\ude80 \u0026\u003c\u003e","literal":"\\u0026"}"#;
    let history = format!(
        r#"[{{"role":"assistant","blocks":[{{"type":"tool_call","tool_call_id":"c1","tool_name":"bash","arguments":{args}}}]}}]"#
    );
    let event = format!(
        r#"{{"type":"tool_call_started","tool_call_id":"c1","tool_name":"bash","tool_args":{args}}}"#
    );
    let stored = from_history(&history).unwrap();
    let event: WireEvent = serde_json::from_str(&event).unwrap();
    let live = reduce(&[], &event);
    assert_eq!(stored, live);
    let Item::Tool { args, .. } = &live[0] else {
        panic!("expected tool")
    };
    assert!(args.contains("echo 中文🚀 &<>"), "{args}");
    assert!(args.contains(r#"\\u0026"#), "{args}");
    assert!(phase(&event).unwrap().contains("echo 中文🚀 &<>"));
}

#[test]
fn items_serialize_with_the_typescript_field_names() {
    let json = serde_json::to_string(&tool("c1", "bash", "a", Some("out"), Some(true)))
        .expect("item serializes");
    assert_eq!(
        json,
        r#"{"kind":"tool","id":"c1","name":"bash","args":"a","result":"out","isError":true}"#
    );
    assert_eq!(
        serde_json::to_string(&tool("c1", "bash", "a", None, None)).expect("item serializes"),
        r#"{"kind":"tool","id":"c1","name":"bash","args":"a"}"#
    );
}

fn phase_of(event_json: &str) -> Option<String> {
    phase(&serde_json::from_str(event_json).expect("event decodes"))
}

#[test]
fn interrupted_output_is_separated_from_the_replacement_and_legacy_retries_decode() {
    let mut items = reduce_json(&[], r#"{"type":"text_delta","text":"partial"}"#).unwrap();
    let legacy: WireEvent =
        serde_json::from_str(r#"{"type":"provider_retry","retry":{"attempt":2}}"#).unwrap();
    assert!(!legacy.retry.as_ref().unwrap().interrupted_output);
    assert_eq!(reduce(&items, &legacy), items);
    items = reduce_json(
        &items,
        r#"{"type":"provider_retry","retry":{"attempt":2,"interrupted_output":true}}"#,
    )
    .unwrap();
    items = reduce_json(&items, r#"{"type":"text_delta","text":"replacement"}"#).unwrap();
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[0], Item::Assistant { text, .. } if text == "partial"));
    assert!(matches!(&items[1], Item::Notice { text } if text == INTERRUPTED_OUTPUT_NOTICE));
    assert!(matches!(&items[2], Item::Assistant { text, .. } if text == "replacement"));
}

#[test]
fn retry_phase_counts_down_then_shows_the_request_in_flight() {
    let event: WireEvent = serde_json::from_str(r#"{"type":"provider_retry","retry":{"attempt":2,"max_attempts":4,"delay_ms":4000,"reason":"connection interrupted"}}"#).unwrap();
    for (elapsed, expected) in [
        (0, "waiting 4s"),
        (1500, "waiting 3s"),
        (3999, "waiting 1s"),
        (4000, "requesting"),
        (8000, "requesting"),
    ] {
        assert_eq!(
            phase_at(&event, elapsed).unwrap(),
            format!("retry 1/3 after connection interrupted, {expected}")
        );
    }
}

#[test]
fn each_turn_event_maps_to_its_phase() {
    let long = "x".repeat(80);
    let cases = [
            (r#"{"type":"agent_started"}"#.to_string(), Some("waiting for model")),
            (r#"{"type":"tool_call_finished","tool_call_id":"c1"}"#.into(), Some("waiting for model")),
            (r#"{"type":"compaction_completed"}"#.into(), Some("waiting for model")),
            (r#"{"type":"reasoning_delta","text":"a"}"#.into(), Some("reasoning")),
            (r#"{"type":"text_delta","text":"a"}"#.into(), Some("responding")),
            (r#"{"type":"compaction_started"}"#.into(), Some("compacting")),
            (
                r#"{"type":"tool_call_started","tool_name":"bash","tool_args":{"command":"ls\n-la"}}"#.into(),
                Some(r#"running bash {"command":"ls\n-la"}"#),
            ),
            (
                format!(r#"{{"type":"tool_call_started","tool_name":"read","tool_args":"{long}"}}"#),
                Some(&*format!("running read \"{}…", "x".repeat(58))),
            ),
            (
                r#"{"type":"provider_retry","retry":{"attempt":2,"max_attempts":3,"delay_ms":250,"reason":"HTTP 503"}}"#.into(),
                Some("retry 1/2 after HTTP 503, waiting 1s"),
            ),
            (
                r#"{"type":"provider_retry","retry":{"attempt":3,"max_attempts":3,"delay_ms":2500,"reason":"HTTP 429"}}"#.into(),
                Some("retry 2/2 after HTTP 429, waiting 3s"),
            ),
            (r#"{"type":"provider_usage"}"#.into(), None),
            (r#"{"type":"agent_finished"}"#.into(), None),
        ];
    for (event, expected) in cases {
        assert_eq!(phase_of(&event).as_deref(), expected, "{event}");
    }
}

#[test]
fn the_status_line_names_the_phase_and_both_durations() {
    assert_eq!(status_line("reasoning", 4, 71), "reasoning · 4s · turn 71s");
}
