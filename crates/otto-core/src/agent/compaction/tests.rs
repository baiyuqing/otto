use super::*;
use crate::model::Block;

#[test]
fn the_retained_budget_leaves_one_token_when_the_reserve_swallows_the_window() {
    let budget = |hard, reserve| {
        compaction_retained_budget(&CompactionSettings {
            hard_input_window: hard,
            reserve_tokens: reserve,
            ..CompactionSettings::default()
        })
    };
    assert_eq!(budget(0, 100), 0);
    assert_eq!(budget(-1, 100), 0);
    assert_eq!(budget(100, 100), 1);
    assert_eq!(budget(100, 200), 1);
    assert_eq!(budget(100, 40), 60);
    assert_eq!(budget(100, -40), 100);
}

#[test]
fn token_addition_saturates_and_floors() {
    assert_eq!(saturating_token_add(2, 3), 5);
    assert_eq!(saturating_token_add(-1, 3), 0);
    assert_eq!(saturating_token_add(2, -3), 0);
    assert_eq!(saturating_token_add(i64::MAX, 1), i64::MAX);
}

#[test]
fn usage_is_combined_only_when_both_sides_reported_it() {
    let left = Usage {
        input_tokens: 1,
        output_tokens: 2,
        cached_input_tokens: 3,
        ..Default::default()
    };
    let right = Usage {
        input_tokens: 10,
        output_tokens: 20,
        cached_input_tokens: 30,
        ..Default::default()
    };
    assert_eq!(
        combine_compaction_usage(left, false, right, true),
        (right, true)
    );
    assert_eq!(
        combine_compaction_usage(left, true, right, false),
        (left, true)
    );
    assert_eq!(
        combine_compaction_usage(left, true, right, true),
        (
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                cached_input_tokens: 33,
                ..Default::default()
            },
            true
        )
    );
    assert_eq!(
        combine_compaction_usage(left, false, right, false),
        (right, false)
    );
}

#[test]
fn the_raw_response_bound_rejects_non_text_and_oversize() {
    let text = |value: &str| Message {
        blocks: vec![Block::text(value)],
        ..Message::default()
    };
    assert!(validate_raw_summary_response_bound(&text("abc"), 3).is_ok());
    assert_eq!(
        validate_raw_summary_response_bound(&text("abcd"), 3).unwrap_err(),
        "response exceeds its byte bound"
    );
    let two = Message {
        blocks: vec![Block::text("ab"), Block::text("cd")],
        ..Message::default()
    };
    assert_eq!(
        validate_raw_summary_response_bound(&two, 3).unwrap_err(),
        "response exceeds its byte bound"
    );
    assert!(validate_raw_summary_response_bound(&two, 4).is_ok());
    let call = Message {
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_name: "echo".into(),
            tool_call_id: "c1".into(),
            ..Block::default()
        }],
        ..Message::default()
    };
    assert_eq!(
        validate_raw_summary_response_bound(&call, 100).unwrap_err(),
        "response contains a non-text block"
    );
}

#[test]
fn the_compacted_estimate_counts_the_summary_and_the_retained_tail() {
    let options = Options::default();
    let retained = [Message {
        role: Role::User,
        blocks: vec![Block::text("kept")],
        ..Message::default()
    }];
    let summary = "s".repeat(300);
    let empty = estimate_compacted_context(&options, &[], "", &[]);
    let with_summary = estimate_compacted_context(&options, &[], &summary, &[]);
    let with_tail = estimate_compacted_context(&options, &[], "", &retained);
    let with_both = estimate_compacted_context(&options, &[], &summary, &retained);
    assert_eq!(
        with_summary - empty,
        100,
        "300 summary bytes add 100 tokens"
    );
    assert_eq!(
        with_tail - empty,
        10,
        "the retained message adds 6 framing + 2 text-block + 2 text tokens"
    );
    assert_eq!(with_both - with_summary, 10, "the summary keeps the tail");
    assert_eq!(with_both - with_tail, 100, "the tail keeps the summary");
}
