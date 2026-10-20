//! Context tests: the active path, checkpoints, runtime precedence and branch framing.

use super::*;

test!(build_context_uses_only_the_active_root_to_leaf_path {
    let file = read_pi_fixture("tree.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let (context, warnings) = build_context(&file.entries, &leaf).expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        [
            "root",
            "[Branch summary]\ninactive work was abandoned",
            "active branch",
        ]
    );
    for message in &context.messages {
        let text = message.text();
        assert!(
            !text.contains("inactive branch") && !text.contains("inactive custom"),
            "inactive branch leaked into context: {:?}",
            context.messages
        );
    }
});

test!(build_context_uses_the_retained_tail_checkpoint {
    let context = context_from_fixture("compacted.jsonl");
    assert_eq!(
        texts(&context.messages),
        ["[Compaction summary]\nsummary", "retained", "after"]
    );
    let checkpoint = &context.messages[0];
    assert_eq!(checkpoint.role, Role::Context);
    assert!(checkpoint.display);
    assert_eq!(checkpoint.context_type, "compaction");
    assert_eq!(context.usage.input_tokens, 12);
    assert_eq!(context.usage.output_tokens, 4);
});

test!(build_context_prefers_a_real_first_kept_entry_in_the_dual_form {
    let context = context_from_fixture("compaction-dual-form.jsonl");
    assert_eq!(
        texts(&context.messages),
        [
            "[Compaction summary]\ndual summary",
            "real retained path",
            "after dual checkpoint",
        ]
    );
    assert_eq!(context.messages[0].context_tokens_before, 140);
    for message in &context.messages {
        assert!(
            !message.id.contains("-tail-") && !message.text().contains("synthetic must lose"),
            "synthetic retained tail won over the real active path: {:?}",
            context.messages
        );
    }
});

test!(build_context_falls_back_to_the_retained_tail {
    let context = context_from_fixture("compaction-retained-tail-only.jsonl");
    assert_eq!(
        texts(&context.messages),
        [
            "[Compaction summary]\nretained-tail summary",
            "synthetic retained tail",
        ]
    );
    assert_eq!(context.messages[0].context_tokens_before, 120);
});

test!(build_context_carries_tokens_before_from_a_compaction_summary_message {
    let built = decode_entry(
        r#"{"type":"message","id":"cf000001","parentId":null,"timestamp":"2026-08-28T10:00:00Z","message":{"role":"compactionSummary","summary":"message summary","tokensBefore":321,"timestamp":1787911200000}}"#,
    );
    let leaf = built.id.clone();
    let (context, _) = build_context(&[built], &leaf).expect("build context");
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.messages[0].context_tokens_before, 321);
});

test!(build_context_uses_the_legacy_first_kept_entry_id {
    let file = read_pi_fixture("compacted.jsonl");
    let (context, warnings) = build_context(&file.entries, "c0000003").expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        ["[Compaction summary]\nlegacy summary", "legacy retained"]
    );

    let mut missing = file.entries[2].clone();
    missing
        .compaction
        .as_mut()
        .expect("compaction payload")
        .first_kept_entry_id = Some("ffffffff".into());
    let leaf = missing.id.clone();
    let entries = vec![file.entries[0].clone(), file.entries[1].clone(), missing];
    let error = build_context(&entries, &leaf).expect_err("missing anchor accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(build_context_keeps_an_image_on_the_active_branch_only {
    let root = user_entry("10000001", None, "root");
    let image = decode_entry(
        r#"{"type":"message","id":"10000002","parentId":"10000001","timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":[{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":2}}"#,
    );
    let text = user_entry("10000003", Some("10000001"), "text leaf");
    let entries = vec![root, image, text];

    let (context, _) = build_context(&entries, "10000002").expect("build image context");
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[1].blocks[0].block_type, BlockType::Image);
    assert_eq!(context.messages[1].blocks[0].mime_type, "image/png");
    let (context, _) = build_context(&entries, "10000003").expect("build context");
    assert_eq!(texts(&context.messages), ["root", "text leaf"]);
});

test!(build_context_rejects_an_unsupported_message_on_the_active_branch_only {
    let bash = decode_entry(
        r#"{"type":"message","id":"20000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}}"#,
    );
    let text = user_entry("20000002", None, "supported root");
    let entries = vec![bash, text];

    let error = build_context(&entries, "20000001").expect_err("bash accepted");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
    build_context(&entries, "20000002").expect("build context");
});

test!(build_context_rejects_provider_required_content_on_the_active_branch_only {
    let provider_specific = decode_entry(
        r#"{"type":"message","id":"21000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"},"thoughtSignature":"provider-state"}],"api":"provider-api","provider":"provider","model":"model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}"#,
    );
    let supported = user_entry("21000002", None, "supported root");
    let entries = vec![provider_specific, supported];

    let error = build_context(&entries, "21000001").expect_err("provider content accepted");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedContent);
    build_context(&entries, "21000002").expect("build context");
});

test!(build_context_applies_runtime_precedence {
    let file = read_pi_fixture("tree.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let (context, _) = build_context(&file.entries, &leaf).expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: "default".into(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
        }
    );
    assert_eq!(context.thinking_level, "high");
    assert_eq!(context.session_name, "Fixture tree");

    let root = user_entry("30000001", None, "root");
    let assistant = assistant_entry(
        "30000002",
        Some("30000001"),
        "assistant",
        "assistant-provider",
        "assistant-model",
        3,
        2,
        "stop",
    );
    let mut model_change = entry("model_change", "30000003", Some("30000002"));
    model_change.model_change = Some(PiModelChange {
        provider: "changed-provider".into(),
        model_id: "changed-model".into(),
    });
    let latest = assistant_entry(
        "30000004",
        Some("30000003"),
        "latest",
        "later-provider",
        "later-model",
        5,
        4,
        "stop",
    );

    let entries = vec![root.clone(), assistant.clone(), model_change, latest];
    let (context, _) = build_context(&entries, "30000004").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: String::new(),
            provider: "changed-provider".into(),
            model: "changed-model".into(),
        }
    );

    let (context, _) =
        build_context(&[root, assistant], "30000002").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: String::new(),
            provider: "assistant-provider".into(),
            model: "assistant-model".into(),
        }
    );
});

test!(build_context_uses_the_latest_active_otto_runtime {
    let first = runtime_entry(
        "31000001",
        None,
        &RuntimeMetadata {
            profile: "first".into(),
            provider: "openai-compatible".into(),
            model: "one".into(),
        },
    );
    let second = runtime_entry(
        "31000002",
        Some("31000001"),
        &RuntimeMetadata {
            profile: "second".into(),
            provider: "openai-compatible".into(),
            model: "two".into(),
        },
    );
    let mut change = entry("model_change", "31000003", Some("31000002"));
    change.model_change = Some(PiModelChange {
        provider: "ignored".into(),
        model_id: "ignored".into(),
    });

    let (context, _) =
        build_context(&[first, second, change], "31000003").expect("build context");
    assert_eq!(
        context.runtime,
        RuntimeMetadata {
            profile: "second".into(),
            provider: "openai-compatible".into(),
            model: "two".into(),
        }
    );
});

test!(build_context_replays_latest_remote_compaction_and_post_checkpoint_messages {
    let first = user_entry("32000001", None, "before");
    let first_checkpoint = RemoteCompaction {
        provider: "openai-compatible".into(),
        id: "first".into(),
        encrypted_content: "opaque-one".into(),
    };
    let checkpoint_one =
        remote_compaction_entry("32000002", Some("32000001"), &first_checkpoint);
    let between = user_entry("32000003", Some("32000002"), "between");
    let second_checkpoint = RemoteCompaction {
        provider: "openai-compatible".into(),
        id: "second".into(),
        encrypted_content: "opaque-two".into(),
    };
    let checkpoint_two =
        remote_compaction_entry("32000004", Some("32000003"), &second_checkpoint);
    let after = user_entry("32000005", Some("32000004"), "after");

    let (context, warnings) = build_context(
        &[first, checkpoint_one, between, checkpoint_two, after],
        "32000005",
    )
    .expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(context.remote_compaction, Some(second_checkpoint));
    assert_eq!(texts(&context.messages), ["before", "between", "after"]);
    assert_eq!(texts(&context.model_messages), ["before", "between", "after"]);
    assert_eq!(texts(&context.post_remote_compaction_messages), ["after"]);
});

test!(build_context_ignores_unknown_and_invalid_remote_custom_records {
    let first = user_entry("33000001", None, "before");
    let mut unknown = entry("custom", "33000002", Some("33000001"));
    unknown.custom = Some(PiCustom {
        custom_type: "future.fixture".into(),
        data: Some(RawValue::from_string("{\"future\":true}".into()).expect("JSON")),
    });
    let mut invalid = entry("custom", "33000003", Some("33000002"));
    invalid.custom = Some(PiCustom {
        custom_type: REMOTE_COMPACTION_CUSTOM_TYPE.into(),
        data: Some(RawValue::from_string("{\"schemaVersion\":2}".into()).expect("JSON")),
    });
    let after = user_entry("33000004", Some("33000003"), "after");

    let (context, warnings) =
        build_context(&[first, unknown, invalid, after], "33000004").expect("build context");
    assert_eq!(context.remote_compaction, None);
    assert_eq!(texts(&context.model_messages), ["before", "after"]);
    assert_eq!(context.post_remote_compaction_messages, context.model_messages);
    assert_warning_contains(&warnings, "ignored malformed otto.remote_compaction");
});

test!(build_context_frames_branch_and_custom_messages {
    let mut branch = entry("branch_summary", "40000001", None);
    branch.branch_summary = Some(Box::new(PiBranchSummary {
        from_id: "ffffffff".into(),
        summary: "abandoned work".into(),
        usage: Some(PiUsage {
            input: 2,
            output: 1,
            cache_read: 3,
            cache_write: 4,
            total_tokens: 10,
            ..PiUsage::default()
        }),
        ..PiBranchSummary::default()
    }));
    let mut hidden = entry("custom_message", "40000002", Some("40000001"));
    hidden.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "hidden.fixture".into(),
        content_text: Some("secret context".into()),
        display: false,
        ..PiCustomMessage::default()
    }));
    let mut visible = entry("custom_message", "40000003", Some("40000002"));
    visible.custom_message = Some(Box::new(PiCustomMessage {
        custom_type: "visible.fixture".into(),
        content_text: Some("shown context".into()),
        display: true,
        ..PiCustomMessage::default()
    }));

    let (context, warnings) =
        build_context(&[branch, hidden, visible], "40000003").expect("build context");
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(
        texts(&context.messages),
        [
            "[Branch summary]\nabandoned work",
            "[Custom context: hidden.fixture]\nsecret context",
            "[Custom context: visible.fixture]\nshown context",
        ]
    );
    assert_eq!(context.messages[0].context_type, "branch_summary");
    assert!(context.messages[0].display);
    assert!(!context.messages[1].display);
    assert!(context.messages[2].display);
    assert_eq!(
        context.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 1,
            cached_input_tokens: 3,
        }
    );
});

test!(build_context_supports_custom_agent_messages {
    let built = decode_entry(
        r#"{"type":"message","id":"41000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"custom","customType":"agent.fixture","content":"injected","display":false,"timestamp":1}}"#,
    );
    let (context, _) = build_context(&[built], "41000001").expect("build context");
    assert_eq!(
        texts(&context.messages),
        ["[Custom context: agent.fixture]\ninjected"]
    );
    assert!(!context.messages[0].display);
});

test!(build_context_warns_for_an_orphan_root_and_for_multiple_roots {
    let orphan = user_entry("50000001", Some("ffffffff"), "orphan");
    let (context, warnings) = build_context(&[orphan], "50000001").expect("build context");
    assert_eq!(texts(&context.messages), ["orphan"]);
    assert_warning_contains(&warnings, "50000001");

    let first = user_entry("50000002", None, "first root");
    let second = user_entry("50000003", None, "second root");
    let (context, warnings) =
        build_context(&[first, second], "50000003").expect("build context");
    assert_eq!(texts(&context.messages), ["second root"]);
    assert_warning_contains(&warnings, "multiple roots");
});

test!(build_context_warns_and_ignores_an_unknown_active_entry {
    let root = user_entry("60000001", None, "root");
    let unknown = entry(
        "future_entry_with_untrusted_\u{1b}[31m_type",
        "60000002",
        Some("60000001"),
    );
    let leaf = user_entry("60000003", Some("60000002"), "leaf");

    let (context, warnings) =
        build_context(&[root.clone(), unknown, leaf], "60000003").expect("build context");
    assert_eq!(texts(&context.messages), ["root", "leaf"]);
    assert_warning_contains(&warnings, "60000002");
    for warning in &warnings {
        assert!(
            !warning.message.contains('\u{1b}') && warning.message.len() <= 256,
            "unsafe or unbounded warning = {:?}",
            warning.message
        );
    }

    // The two explicit roots produce exactly the multiple-root warning; the
    // inactive unknown entry must not add another.
    let inactive = entry("future_entry", "60000004", None);
    let (_, warnings) =
        build_context(&[inactive, root], "60000001").expect("build context");
    assert_eq!(warnings.len(), 1, "warnings = {warnings:?}");
});
