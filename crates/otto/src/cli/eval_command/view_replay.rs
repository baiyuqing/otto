//! Replays the log through the chat view with every parent built.

use otto_core::view_tree::{
    LogKind, LogMessage, NodeAddress, ShrinkEvent, SummaryTree, View, ViewTurnBytes,
};
use serde::{Deserialize, Serialize};

/// One batch shrink, as written to `build.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShrinkRecord {
    /// Number of log messages when the shrink ran.
    pub messages: u64,
    pub first_changed_line: usize,
    pub lines_before: usize,
    pub lines_after: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub unchanged_prefix_bytes: usize,
}

pub struct ViewReplay {
    pub final_view: Vec<NodeAddress>,
    pub shrinks: Vec<ShrinkRecord>,
    /// Per turn: the view at the turn's first message and the shrinks since the previous turn.
    pub turn_views: Vec<ViewTurnBytes>,
}

/// Ids of the first message of each turn: every `user` message, and message 0 when the log does
/// not start with one.
pub fn turn_starts(log: &[LogMessage]) -> Vec<u64> {
    let mut starts: Vec<u64> = log
        .iter()
        .filter(|message| message.kind == LogKind::User)
        .map(|message| message.id)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts
}

/// Appends each message and shrinks with the message count, as the live view would.
pub fn replay_view(
    log: &[LogMessage],
    tree: &SummaryTree,
    low_bytes: usize,
    high_bytes: usize,
) -> ViewReplay {
    let starts = turn_starts(log);
    let mut view = View::new(low_bytes, high_bytes);
    let mut shrinks = Vec::new();
    let mut turn_views = Vec::new();
    let mut unchanged_since_turn_start: Vec<u64> = Vec::new();
    let mut next_start = 0;
    for message in log {
        if starts.get(next_start) == Some(&message.id) {
            turn_views.push(ViewTurnBytes {
                view_bytes: view.bytes(tree) as u64,
                shrink_unchanged_prefix_bytes: std::mem::take(&mut unchanged_since_turn_start),
            });
            next_start += 1;
        }
        view.append(message.id);
        if let Some(event) = view.shrink(message.id + 1, tree) {
            unchanged_since_turn_start.push(event.unchanged_prefix_bytes as u64);
            shrinks.push(shrink_record(message.id + 1, &event));
        }
    }
    ViewReplay {
        final_view: view.lines().to_vec(),
        shrinks,
        turn_views,
    }
}

fn shrink_record(messages: u64, event: &ShrinkEvent) -> ShrinkRecord {
    ShrinkRecord {
        messages,
        first_changed_line: event.first_changed_line,
        lines_before: event.lines_before,
        lines_after: event.lines_after,
        bytes_before: event.bytes_before,
        bytes_after: event.bytes_after,
        unchanged_prefix_bytes: event.unchanged_prefix_bytes,
    }
}

/// The final view as `[id, n]` pairs.
pub fn view_pairs(lines: &[NodeAddress]) -> Vec<[u64; 2]> {
    lines.iter().map(|line| [line.id, line.n]).collect()
}

/// Rebuilds view lines from `[id, n]` pairs.
pub fn view_from_pairs(pairs: &[[u64; 2]]) -> Result<Vec<NodeAddress>, String> {
    pairs
        .iter()
        .map(|[id, n]| NodeAddress::new(*id, *n))
        .collect()
}
