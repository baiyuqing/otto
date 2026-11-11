//! Fills the cache-cost model from the log, the view replay and the runtime settings.

use otto_core::config::resolve::CompactionRuntime;
use otto_core::view_tree::{
    CacheCostInput, CachePrices, CompactionLimits, CompressorUsage, LogKind, LogMessage,
    ModelCallBytes, SYSTEM_PROMPT, TurnBytes, ViewTurnBytes, tool_definitions,
};

use super::model_calls::TokenUsage;
use super::view_replay::turn_starts;

/// Working window in tokens when neither the compaction settings nor the context window name one.
pub const FALLBACK_WORKING_WINDOW_TOKENS: u64 = 128_000;
/// Assumed size of a compaction summary in bytes.
pub const ASSUMED_SUMMARY_BYTES: u64 = 8 * 1024;

/// Bytes of the system prompt and tool definitions that both layouts send. The view layout's
/// prompt is used for the current layout too, because only the difference between layouts is
/// compared and the product system prompt is not part of this evaluation.
pub fn tools_system_bytes() -> u64 {
    let tools: usize = tool_definitions()
        .iter()
        .map(|tool| {
            tool.name.len()
                + tool.description.len()
                + tool
                    .parameters
                    .as_ref()
                    .map_or(0, |schema| schema.get().len())
        })
        .sum();
    (SYSTEM_PROMPT.len() + tools) as u64
}

/// Compaction limits and a note saying where each came from.
pub fn compaction_limits(runtime: &CompactionRuntime) -> (CompactionLimits, String) {
    let (window, source) = if runtime.working_window > 0 {
        (runtime.working_window as u64, "working_window")
    } else if runtime.context_window > 0 {
        (
            runtime.context_window as u64,
            "context_window (working_window is unset)",
        )
    } else {
        (
            FALLBACK_WORKING_WINDOW_TOKENS,
            "fallback of 128000 tokens (the window is unknown)",
        )
    };
    let limits = CompactionLimits {
        working_window_tokens: window,
        reserve_tokens: runtime.reserve_tokens.max(0) as u64,
        keep_recent_tokens: runtime.keep_recent_tokens.max(0) as u64,
        summary_bytes: ASSUMED_SUMMARY_BYTES,
    };
    let note = format!(
        "working window {source}; reserve {} and keep-recent {} tokens from the profile; summary assumed {ASSUMED_SUMMARY_BYTES} bytes",
        limits.reserve_tokens, limits.keep_recent_tokens
    );
    (limits, note)
}

/// Per turn: the user message size and, for each model call, its output and tool-result sizes. A
/// call is an `assistant` message and the `tool_call` messages after it; the `tool_result`
/// messages after those are its tool results.
pub fn turns_from_log(log: &[LogMessage]) -> Vec<TurnBytes> {
    let starts = turn_starts(log);
    let mut turns = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(log.len() as u64);
        let mut turn = TurnBytes::default();
        let mut previous: Option<LogKind> = None;
        for message in &log[*start as usize..end as usize] {
            let bytes = message.text.len() as u64;
            match message.kind {
                LogKind::User | LogKind::Context => turn.user_bytes += bytes,
                LogKind::Assistant | LogKind::ToolCall => {
                    if matches!(previous, None | Some(LogKind::User | LogKind::ToolResult)) {
                        turn.calls.push(ModelCallBytes::default());
                    }
                    if let Some(call) = turn.calls.last_mut() {
                        call.output_bytes += bytes;
                    }
                }
                LogKind::ToolResult => {
                    if turn.calls.is_empty() {
                        turn.calls.push(ModelCallBytes::default());
                    }
                    if let Some(call) = turn.calls.last_mut() {
                        call.tool_result_bytes += bytes;
                    }
                }
            }
            previous = Some(message.kind);
        }
        turns.push(turn);
    }
    turns
}

pub fn cost_input(
    log: &[LogMessage],
    turn_views: Vec<ViewTurnBytes>,
    compaction: CompactionLimits,
    prices: CachePrices,
    compressor: &TokenUsage,
) -> CacheCostInput {
    CacheCostInput {
        prices,
        tools_system_bytes: tools_system_bytes(),
        compaction,
        turns: turns_from_log(log),
        view_turns: turn_views,
        compressor: CompressorUsage {
            input_tokens: compressor.input_tokens,
            cached_tokens: compressor.cached_tokens,
            output_tokens: compressor.output_tokens,
        },
    }
}
