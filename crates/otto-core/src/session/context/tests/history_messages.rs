//! Context tests: the full active-branch history.

use super::*;

test!(active_branch_history_keeps_messages_before_a_compaction_and_skips_other_branches {
    let root = user_entry("a1000001", None, "first");
    let reply = assistant_entry("a1000002", Some("a1000001"), "second", "p", "m", 1, 1, "stop");
    let mut compaction = entry("compaction", "a1000003", Some("a1000002"));
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "summary".into(),
        tokens_before: 1,
        retained_tail: Some(Vec::new()),
        ..PiCompaction::default()
    }));
    let after = user_entry("a1000004", Some("a1000003"), "third");
    let other_branch = user_entry("a1000005", Some("a1000002"), "abandoned");
    let entries = [root, reply, compaction, after, other_branch];

    let history = active_branch_history(&entries, "a1000004").expect("history");
    let texts: Vec<String> = history.iter().map(|item| item.message.text()).collect();
    assert_eq!(texts, ["first", "second", "third"]);
    let ids: Vec<&str> = history.iter().map(|item| item.entry_id.as_str()).collect();
    assert_eq!(ids, ["a1000001", "a1000002", "a1000004"]);
    assert!(history.iter().all(|item| item.timestamp == "2026-08-27T12:00:00Z"));
    // build_context keeps only the post-checkpoint path.
    let (context, _) = build_context(&entries, "a1000004").expect("context");
    assert_eq!(texts_of(&context.messages), ["[Compaction summary]\nsummary", "third"]);
});

fn texts_of(messages: &[Message]) -> Vec<String> {
    texts(messages)
}
