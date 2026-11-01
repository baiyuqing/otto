use otto_core::config::resolve::CompactionRuntime;
use otto_core::view_tree::{LogKind, LogMessage, ModelCallBytes, NodeAddress, SummaryTree};

use crate::eval_command::cost_input::{
    ASSUMED_SUMMARY_BYTES, compaction_limits, tools_system_bytes, turns_from_log,
};
use crate::eval_command::view_replay::{replay_view, turn_starts};

fn message(id: u64, kind: LogKind, text: &str) -> LogMessage {
    LogMessage {
        id,
        kind,
        text: text.into(),
        entry_id: String::new(),
        timestamp: String::new(),
    }
}

fn tool_log() -> Vec<LogMessage> {
    vec![
        message(0, LogKind::User, "ask"),
        message(1, LogKind::Assistant, "plan"),
        message(2, LogKind::ToolCall, "read {}"),
        message(3, LogKind::ToolResult, "file contents"),
        message(4, LogKind::Assistant, "done"),
        message(5, LogKind::User, "again"),
        message(6, LogKind::Assistant, "ok"),
    ]
}

#[test]
fn turns_split_at_user_messages_and_calls_at_assistant_messages() {
    let turns = turns_from_log(&tool_log());
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].user_bytes, 3);
    assert_eq!(
        turns[0].calls,
        [
            ModelCallBytes {
                output_bytes: 4 + 7,
                tool_result_bytes: 13
            },
            ModelCallBytes {
                output_bytes: 4,
                tool_result_bytes: 0
            }
        ]
    );
    assert_eq!((turns[1].user_bytes, turns[1].calls.len()), (5, 1));
    assert_eq!(turn_starts(&tool_log()), [0, 5]);
}

#[test]
fn the_view_replay_records_the_view_at_each_turn_start() {
    let log = tool_log();
    let mut tree = SummaryTree::new();
    for address in SummaryTree::addresses_for_log(log.len() as u64) {
        let input = tree.node_input(&log, address).unwrap();
        let otto_core::view_tree::NodeInput::Ready(text) = input else {
            panic!("short texts are ready")
        };
        tree.insert(address, text).unwrap();
    }
    let replay = replay_view(&log, &tree, 1 << 20, 1 << 20);
    assert_eq!(replay.turn_views.len(), 2);
    assert_eq!(replay.turn_views[0].view_bytes, 0);
    // Turn 1 starts after messages 0..=4: their lines as `id+1|kind: text\n`.
    let expected: usize = log[..5]
        .iter()
        .map(|m| format!("{}+1|{}: {}\n", m.id, m.kind.name(), m.text).len())
        .sum();
    assert_eq!(replay.turn_views[1].view_bytes as usize, expected);
    assert!(replay.shrinks.is_empty());
    assert_eq!(replay.final_view.len(), 7);
    assert_eq!(replay.final_view[0], NodeAddress::leaf(0));
}

#[test]
fn compaction_limits_state_where_the_window_came_from() {
    let mut runtime = CompactionRuntime {
        auto: true,
        context_window: 0,
        hard_input_window: 0,
        working_window: 0,
        max_output_tokens: 0,
        reserve_tokens: 1000,
        keep_recent_tokens: 2000,
    };
    let (limits, note) = compaction_limits(&runtime);
    assert_eq!(limits.working_window_tokens, 128_000);
    assert!(note.contains("fallback of 128000"));
    runtime.context_window = 50_000;
    assert_eq!(compaction_limits(&runtime).0.working_window_tokens, 50_000);
    runtime.working_window = 40_000;
    let (limits, note) = compaction_limits(&runtime);
    assert_eq!(
        (
            limits.working_window_tokens,
            limits.reserve_tokens,
            limits.keep_recent_tokens
        ),
        (40_000, 1000, 2000)
    );
    assert_eq!(limits.summary_bytes, ASSUMED_SUMMARY_BYTES);
    assert!(note.contains("working_window"));
    assert!(tools_system_bytes() > otto_core::view_tree::SYSTEM_PROMPT.len() as u64);
}
