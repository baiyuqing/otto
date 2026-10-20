//! Context tests: context edits.

use super::*;

test!(context_edit_omits_replaces_and_keeps_raw_history {
    let root = user_entry("ce000001", None, "original");
    let replace = context_edit_entry("ce000002", Some("ce000001"), "ce000001", Some("new"));
    let omit = context_edit_entry("ce000003", Some("ce000002"), "ce000001", None);

    let (replaced, _) =
        build_context(&[root.clone(), replace.clone()], "ce000002").expect("replace");
    assert_eq!(replaced.messages.len(), 1);
    assert_eq!(replaced.messages[0].text(), "original");
    assert_eq!(replaced.model_messages[0].text(), "new");
    assert_eq!(root.message.as_ref().unwrap().content_text.as_deref(), Some("original"));

    let (omitted, _) =
        build_context(&[root, replace, omit.clone()], "ce000003").expect("omit");
    assert!(omitted.model_messages.is_empty());
    assert_eq!(omitted.messages[0].text(), "original");
    assert!(omit.context_edit.as_ref().unwrap().replacement.is_none());
});

test!(context_edit_is_latest_wins_and_branch_relative {
    let root = user_entry("ce100001", None, "original");
    let first = context_edit_entry("ce100002", Some("ce100001"), "ce100001", Some("first"));
    let second = context_edit_entry("ce100003", Some("ce100002"), "ce100001", Some("second"));
    let branch = user_entry("ce100004", Some("ce100001"), "branch");

    let (latest, _) = build_context(
        &[root.clone(), first, second],
        "ce100003",
    )
    .expect("latest edit");
    assert_eq!(latest.model_messages[0].text(), "second");

    let (other_branch, _) = build_context(&[root, branch], "ce100004").expect("branch");
    assert_eq!(other_branch.model_messages[0].text(), "original");
    assert_eq!(other_branch.model_messages[1].text(), "branch");
});

test!(context_edit_rejects_a_noneditable_or_inactive_target {
    let root = user_entry("ce200001", None, "root");
    let mut label = entry("label", "ce200002", Some("ce200001"));
    label.label = Some(crate::session::PiLabel {
        target_id: "ce200001".into(),
        label: Some("x".into()),
    });
    let invalid = context_edit_entry("ce200003", Some("ce200002"), "ce200002", None);
    let (_, warnings) = build_context(&[root.clone(), label, invalid], "ce200003")
        .expect("label target is skipped");
    assert_warning_contains(&warnings, "unsupported target");

    let inactive = user_entry("ce200004", Some("ce200001"), "inactive");
    let active = user_entry("ce200005", Some("ce200001"), "active");
    let invalid = context_edit_entry("ce200006", Some("ce200005"), "ce200004", None);
    let (_, warnings) = build_context(&[root, inactive, active, invalid], "ce200006")
        .expect("inactive target is skipped");
    assert_warning_contains(&warnings, "unsupported target");
});

test!(context_edit_skips_tool_call_and_result_targets_without_affecting_recovery {
    let root = user_entry("ce400001", None, "root");
    let assistant = tool_call_entry("ce400002", Some("ce400001"), &[("call-1", "read")]);
    let result = tool_result_entry(
        "ce400003",
        Some("ce400002"),
        "call-1",
        "read",
        "done",
    );
    let edit = context_edit_entry("ce400004", Some("ce400003"), "ce400003", None);
    let (context, warnings) =
        build_context(&[root, assistant, result, edit], "ce400004").expect("build context");
    assert_eq!(context.messages.len(), 3);
    assert_eq!(context.model_messages.len(), 3);
    assert_eq!(context.model_messages[2].blocks[0].text, "done");
    assert!(warnings.iter().any(|warning| warning.message.contains("unsupported target")));
});

test!(context_edit_skips_a_target_removed_by_compaction {
    let root = user_entry("ce500001", None, "old");
    let edit = context_edit_entry("ce500002", Some("ce500001"), "ce500001", None);
    let mut compaction = entry("compaction", "ce500003", Some("ce500002"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "summary".into(),
        first_kept_entry_id: None,
        tokens_before: 1,
        retained_tail: Some(Vec::new()),
        ..PiCompaction::default()
    }));
    let (context, warnings) =
        build_context(&[root, edit, compaction], "ce500003").expect("build context");
    assert_eq!(context.model_messages.len(), 1);
    assert_eq!(context.model_messages[0].context_type, COMPACTION_CONTEXT_TYPE);
    assert!(warnings
        .iter()
        .any(|warning| warning.message.contains("not retained after compaction")));
});

test!(context_edit_uses_pi_replacement_shape_and_round_trips {
    let raw = br#"{"type":"context_edit","id":"ce300001","parentId":null,"timestamp":"2026-08-27T12:00:00Z","targetId":"ce300000","replacement":{"content":"replacement"}}"#;
    let entry = decode_pi_entry(raw).expect("decode");
    let edit = entry.context_edit.as_ref().expect("payload");
    assert_eq!(edit.target_id, "ce300000");
    assert_eq!(edit.replacement_text.as_deref(), Some("replacement"));
    let encoded = crate::session::encode_pi_record(crate::session::PiRecord::Entry(&entry))
        .expect("encode raw");
    assert_eq!(encoded, raw);

    let mut typed = entry;
    typed.raw.clear();
    let encoded = crate::session::encode_pi_record(crate::session::PiRecord::Entry(&typed))
        .expect("encode typed");
    let round_trip = decode_pi_entry(&encoded).expect("decode typed record");
    assert_eq!(
        round_trip
            .context_edit
            .as_ref()
            .expect("typed payload")
            .replacement_text
            .as_deref(),
        Some("replacement")
    );
});
