
use crate::model::Block;

use super::*;

fn user(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn assistant(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn assistant_call(id: &str, call_id: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_name: "echo".into(),
            tool_call_id: call_id.into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

fn tool(id: &str, call_id: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            text: "out".into(),
            tool_name: "echo".into(),
            tool_call_id: call_id.into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

fn compaction_message(id: &str, summary: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Context,
        context_type: "compaction".into(),
        display: true,
        blocks: vec![Block::text(format!(
            "{COMPACTION_SUMMARY_DISPLAY_PREFIX}{summary}"
        ))],
        ..Message::default()
    }
}

#[test]
fn the_most_recent_user_turn_is_retained_and_the_rest_summarized() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
    ];
    let selection = select_compaction(&messages, None, 0, 0).expect("a boundary exists");
    assert_eq!(selection.first_kept_id, "m3");
    assert_eq!(selection.retained.len(), 2);
    assert_eq!(selection.historical_source.len(), 2);
    assert!(selection.turn_prefix_source.is_empty());
    assert!(!selection.split_turn);
}

#[test]
fn a_single_turn_has_nothing_to_compact() {
    let messages = vec![user("m1", "only"), assistant("m2", "reply")];
    assert!(matches!(
        select_compaction(&messages, None, 0, 0),
        Err(AgentError::NothingToCompact)
    ));
    assert!(matches!(
        select_compaction(&[], None, 0, 0),
        Err(AgentError::NothingToCompact)
    ));
}

#[test]
fn keep_recent_tokens_pulls_earlier_turns_back_into_the_retained_part() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
        user("m5", "third"),
        assistant("m6", "reply"),
    ];
    let tight = select_compaction(&messages, None, 0, 0).expect("valid");
    assert_eq!(tight.first_kept_id, "m5");
    let generous = select_compaction(&messages, None, 40, 0).expect("valid");
    assert_eq!(generous.first_kept_id, "m3");
}

#[test]
fn the_hard_budget_stops_the_walk_backwards() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
        user("m5", "third"),
        assistant("m6", "reply"),
    ];
    // Without the budget, a keep of 1000 tokens would walk to the start
    // of the transcript and leave nothing to compact.
    let selection = select_compaction(&messages, None, 1_000, 45).expect("valid");
    assert_eq!(selection.first_kept_id, "m3");
}

#[test]
fn a_retained_part_over_the_hard_budget_reports_the_turn_is_too_large() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", &"x".repeat(1_000)),
    ];
    assert!(matches!(
        select_compaction(&messages, None, 0, 10),
        Err(AgentError::CurrentTurnTooLarge)
    ));
}

#[test]
fn tool_results_stay_with_the_assistant_call_that_made_them() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant_call("m4", "c1"),
        tool("m5", "c1"),
        assistant("m6", "done"),
    ];
    let selection = select_compaction(&messages, None, 0, 0).expect("valid");
    assert_eq!(selection.first_kept_id, "m3");
    assert_eq!(
        selection
            .retained
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m3", "m4", "m5", "m6"]
    );
}

#[test]
fn a_turn_retained_from_its_assistant_half_splits_the_source() {
    // The recorded anchor lands on the assistant half of a turn, so the
    // user message that opened it is summarized in turn-prefix mode.
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
    ];
    let latest = CompactionMetadata {
        retained_tail_only: true,
        first_post_checkpoint_message_id: "m4".into(),
        ..CompactionMetadata::default()
    };
    let selection = select_compaction(&messages, Some(&latest), 0, 0).expect("valid");
    assert_eq!(selection.first_kept_id, "m4");
    assert!(selection.split_turn);
    assert_eq!(
        selection
            .historical_source
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2"]
    );
    assert_eq!(
        selection
            .turn_prefix_source
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m3"]
    );
}

#[test]
fn context_messages_join_the_turn_that_follows_them() {
    let context = Message {
        id: "m0".into(),
        role: Role::Context,
        context_type: "task_notification".into(),
        blocks: vec![Block::text("note")],
        ..Message::default()
    };
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        context,
        user("m3", "second"),
        assistant("m4", "reply"),
    ];
    let selection = select_compaction(&messages, None, 0, 0).expect("valid");
    assert_eq!(selection.first_kept_id, "m0");
}

#[test]
fn the_previous_summary_comes_from_the_checkpoint_message() {
    let messages = vec![
        compaction_message("c1", "earlier state\n\n<read-files>\na.txt\n</read-files>"),
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
    ];
    let latest = CompactionMetadata {
        id: "c1".into(),
        summary: "ignored".into(),
        ..CompactionMetadata::default()
    };
    let selection = select_compaction(&messages, Some(&latest), 0, 0).expect("valid");
    assert_eq!(selection.previous_summary, "earlier state");
    assert!(
        selection
            .historical_source
            .iter()
            .all(|message| message.context_type != "compaction"),
        "the checkpoint message is not summarized again"
    );
}

#[test]
fn a_checkpoint_message_missing_from_the_transcript_falls_back_to_its_metadata() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
    ];
    let latest = CompactionMetadata {
        id: "c1".into(),
        summary: "stored state\n\n<read-files>\na.txt\n</read-files>".into(),
        ..CompactionMetadata::default()
    };
    let selection = select_compaction(&messages, Some(&latest), 0, 0).expect("valid");
    assert_eq!(selection.previous_summary, "stored state");
}

#[test]
fn a_retained_tail_checkpoint_advances_from_its_recorded_anchor() {
    let messages = vec![
        user("m1", "first"),
        assistant("m2", "reply"),
        user("m3", "second"),
        assistant("m4", "reply"),
        user("m5", "third"),
        assistant("m6", "reply"),
    ];
    let latest = CompactionMetadata {
        id: "c1".into(),
        retained_tail_only: true,
        first_post_checkpoint_message_id: "m3".into(),
        ..CompactionMetadata::default()
    };
    let selection = select_compaction(&messages, Some(&latest), 0, 0).expect("valid");
    assert_eq!(selection.first_kept_id, "m3");
    assert_eq!(
        selection
            .historical_source
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2"]
    );
}

#[test]
fn a_retained_tail_checkpoint_without_a_usable_anchor_has_nothing_to_compact() {
    let messages = vec![user("m1", "first"), assistant("m2", "reply")];
    for anchor in ["", "missing"] {
        let latest = CompactionMetadata {
            retained_tail_only: true,
            first_post_checkpoint_message_id: anchor.into(),
            ..CompactionMetadata::default()
        };
        assert!(
            matches!(
                select_compaction(&messages, Some(&latest), 0, 0),
                Err(AgentError::NothingToCompact)
            ),
            "anchor {anchor:?}"
        );
    }
}

#[test]
fn a_malformed_transcript_is_reported_before_any_selection() {
    let messages = vec![user("m1", "first"), tool("m2", "c1")];
    let error = select_compaction(&messages, None, 0, 0).unwrap_err();
    assert_eq!(
        error.to_string(),
        "select compaction: tool result \"c1\" has no pending call"
    );
}

#[test]
fn tool_pair_validation_reports_each_violation() {
    let cases: Vec<(Vec<Message>, &str)> = vec![
        (
            vec![assistant_call("m1", "c1"), assistant("m2", "next")],
            "unresolved tool calls must be followed immediately by results",
        ),
        (
            vec![assistant_call("m1", "c1")],
            "unresolved tool calls at end of context",
        ),
        (
            vec![assistant_call("m1", ""), tool("m2", "")],
            "assistant tool call requires an id and name",
        ),
        (
            vec![
                assistant_call("m1", "c1"),
                tool("m2", "c1"),
                assistant_call("m3", "c1"),
                tool("m4", "c1"),
            ],
            "duplicate tool-call id \"c1\"",
        ),
        (
            vec![Message {
                id: "m1".into(),
                role: Role::Tool,
                blocks: Vec::new(),
                ..Message::default()
            }],
            "tool message must contain a result",
        ),
        (
            vec![Message {
                id: "m1".into(),
                role: Role::User,
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_name: "echo".into(),
                    tool_call_id: "c1".into(),
                    ..Block::default()
                }],
                ..Message::default()
            }],
            "tool blocks are incompatible with user messages",
        ),
    ];
    for (messages, expected) in cases {
        assert_eq!(
            validate_retained_tool_pairs(&messages).unwrap_err(),
            expected
        );
    }
    assert!(validate_retained_tool_pairs(&[assistant_call("m1", "c1"), tool("m2", "c1")]).is_ok());
}

#[test]
fn a_mismatched_tool_result_name_is_rejected() {
    let mut result = tool("m2", "c1");
    result.blocks[0].tool_name = "other".into();
    assert_eq!(
        validate_retained_tool_pairs(&[assistant_call("m1", "c1"), result]).unwrap_err(),
        "tool result \"c1\" name does not match its call"
    );
}
