//! Computed per-turn input cost of two request layouts under prompt caching.
//!
//! The model works on byte sizes. A request's cached prefix is the part identical to the
//! previous request of the same layout; it is billed at the read price, the rest of the
//! input at the write price, and output at the output price. Costs are in units of one
//! uncached input token. Nothing here is measured from a provider.

use serde::{Deserialize, Serialize};

/// Price multipliers relative to one uncached input token.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CachePrices {
    /// Price of a cached input token.
    pub cache_read: f64,
    /// Price of an input token written to the cache.
    pub cache_write: f64,
    /// Price of an output token.
    pub output: f64,
    /// Price of a compressor-model token as a fraction of the main-model price.
    pub compressor_ratio: f64,
    /// Bytes per token used to turn byte sizes into tokens.
    pub bytes_per_token: f64,
}

impl Default for CachePrices {
    fn default() -> Self {
        Self {
            cache_read: 0.1,
            cache_write: 1.25,
            output: 5.0,
            compressor_ratio: 0.2,
            bytes_per_token: 4.0,
        }
    }
}

/// Compaction settings of the current layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionLimits {
    /// Working window in tokens.
    pub working_window_tokens: u64,
    /// Tokens kept free of the window; compaction runs above `working_window - reserve`.
    pub reserve_tokens: u64,
    /// Tokens of recent messages kept verbatim by a compaction.
    pub keep_recent_tokens: u64,
    /// Size in bytes of the summary a compaction writes.
    pub summary_bytes: u64,
}

/// One model call of a turn and the tool results that follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelCallBytes {
    /// Bytes of the assistant message the call produced (text and tool calls).
    pub output_bytes: u64,
    /// Bytes of the tool results appended after it (0 for a final answer).
    pub tool_result_bytes: u64,
}

/// A user message and the model calls until the next user message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TurnBytes {
    /// Bytes of the user message.
    pub user_bytes: u64,
    /// The model calls in order.
    pub calls: Vec<ModelCallBytes>,
}

/// The chat view at the start of one turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ViewTurnBytes {
    /// Rendered bytes of the view the turn's requests carry.
    pub view_bytes: u64,
    /// For each batch shrink since the previous turn's view, the bytes of the lines before
    /// its first changed line (`ShrinkEvent::unchanged_prefix_bytes`).
    pub shrink_unchanged_prefix_bytes: Vec<u64>,
}

/// Billed compressor tokens, measured from the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompressorUsage {
    /// Input tokens including cached ones.
    pub input_tokens: u64,
    /// Input tokens read from the cache.
    pub cached_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

/// Everything the cost model needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheCostInput {
    /// Prices.
    pub prices: CachePrices,
    /// Bytes of tool definitions plus system prompt, which lead every request.
    pub tools_system_bytes: u64,
    /// Compaction settings of the current layout.
    pub compaction: CompactionLimits,
    /// The session's turns.
    pub turns: Vec<TurnBytes>,
    /// The view at each turn; missing entries count as an empty view.
    pub view_turns: Vec<ViewTurnBytes>,
    /// Measured compressor usage.
    pub compressor: CompressorUsage,
}

/// Whether the cache is still valid at the start of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// The previous request's prefix is cached at the start of every turn.
    Warm,
    /// Nothing is cached at the start of every turn; the current layout also applies
    /// otto's idle compaction above half of `working_window - reserve`.
    Expired,
}

/// Cost of one layout.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LayoutCost {
    /// Number of turns.
    pub turns: usize,
    /// Main-model cost plus compressor cost.
    pub total: f64,
    /// `total` divided by the number of turns.
    pub per_turn: f64,
    /// Main-model cost: input, compaction summaries and output.
    pub main: f64,
    /// Compressor cost (view layout only).
    pub compressor: f64,
    /// Input tokens read from the cache.
    pub cached_input_tokens: f64,
    /// Input tokens billed as written.
    pub written_input_tokens: f64,
    /// Main-model output tokens, summaries included.
    pub output_tokens: f64,
    /// Compactions run (current layout only).
    pub compactions: usize,
}

/// The four cases of the report.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CacheCostReport {
    /// Current layout, cache warm between turns.
    pub current_warm: LayoutCost,
    /// Current layout, cache expired before every turn.
    pub current_expired: LayoutCost,
    /// View layout, cache warm between turns.
    pub view_warm: LayoutCost,
    /// View layout, cache expired before every turn.
    pub view_expired: LayoutCost,
}

/// Computes both layouts in both cache states.
pub fn cache_cost_report(input: &CacheCostInput) -> CacheCostReport {
    CacheCostReport {
        current_warm: current_layout_cost(input, CacheState::Warm),
        current_expired: current_layout_cost(input, CacheState::Expired),
        view_warm: view_layout_cost(input, CacheState::Warm),
        view_expired: view_layout_cost(input, CacheState::Expired),
    }
}

#[derive(Default)]
struct Bill {
    main: f64,
    cached_tokens: f64,
    written_tokens: f64,
    output_tokens: f64,
}

impl Bill {
    /// Bills one request: `cacheable_bytes` of its `input_bytes` are read from the cache.
    fn request(
        &mut self,
        prices: &CachePrices,
        input_bytes: u64,
        cacheable_bytes: u64,
        output_bytes: u64,
    ) {
        let cached = tokens(prices, cacheable_bytes.min(input_bytes));
        let written = tokens(prices, input_bytes) - cached;
        let output = tokens(prices, output_bytes);
        self.main +=
            prices.cache_read * cached + prices.cache_write * written + prices.output * output;
        self.cached_tokens += cached;
        self.written_tokens += written;
        self.output_tokens += output;
    }

    fn finish(self, input: &CacheCostInput, compressor: f64, compactions: usize) -> LayoutCost {
        let turns = input.turns.len();
        let total = self.main + compressor;
        LayoutCost {
            turns,
            total,
            per_turn: if turns == 0 {
                0.0
            } else {
                total / turns as f64
            },
            main: self.main,
            compressor,
            cached_input_tokens: self.cached_tokens,
            written_input_tokens: self.written_tokens,
            output_tokens: self.output_tokens,
            compactions,
        }
    }
}

fn tokens(prices: &CachePrices, bytes: u64) -> f64 {
    bytes as f64 / prices.bytes_per_token
}

/// Cost of the transcript otto carries today: every request holds tools, system prompt and
/// the whole transcript. A compaction replaces the transcript by a summary and the recent
/// messages whose sizes add up to `keep_recent_tokens`; it runs before a call when the
/// request would exceed `working_window - reserve`, and (expired case) before a turn when
/// the transcript exceeds half of that. The summary call reads the cached prefix, and the
/// next request can read only tools and system prompt.
pub fn current_layout_cost(input: &CacheCostInput, state: CacheState) -> LayoutCost {
    let prices = &input.prices;
    let limits = &input.compaction;
    let threshold_tokens = limits
        .working_window_tokens
        .saturating_sub(limits.reserve_tokens) as f64;
    let mut bill = Bill::default();
    let mut compactions = 0;
    let mut messages: Vec<u64> = Vec::new();
    let mut cacheable = 0u64;
    let mut compact = |bill: &mut Bill, messages: &mut Vec<u64>, cacheable: &mut u64| {
        let request_bytes = input.tools_system_bytes + messages.iter().sum::<u64>();
        let keep_bytes = limits.keep_recent_tokens as f64 * prices.bytes_per_token;
        let mut kept_bytes = 0.0;
        let mut kept = 0;
        for size in messages.iter().rev() {
            if kept_bytes >= keep_bytes {
                break;
            }
            kept_bytes += *size as f64;
            kept += 1;
        }
        if kept == messages.len() {
            return;
        }
        bill.request(prices, request_bytes, *cacheable, limits.summary_bytes);
        let mut next = vec![limits.summary_bytes];
        next.extend_from_slice(&messages[messages.len() - kept..]);
        *messages = next;
        *cacheable = input.tools_system_bytes;
        compactions += 1;
    };
    for turn in &input.turns {
        if state == CacheState::Expired {
            cacheable = 0;
            let carried = input.tools_system_bytes + messages.iter().sum::<u64>();
            if tokens(prices, carried) > threshold_tokens / 2.0 {
                compact(&mut bill, &mut messages, &mut cacheable);
            }
        }
        messages.push(turn.user_bytes);
        for call in &turn.calls {
            let request_bytes = input.tools_system_bytes + messages.iter().sum::<u64>();
            if tokens(prices, request_bytes) > threshold_tokens {
                compact(&mut bill, &mut messages, &mut cacheable);
            }
            let request_bytes = input.tools_system_bytes + messages.iter().sum::<u64>();
            bill.request(prices, request_bytes, cacheable, call.output_bytes);
            cacheable = request_bytes;
            messages.push(call.output_bytes);
            if call.tool_result_bytes > 0 {
                messages.push(call.tool_result_bytes);
            }
        }
    }
    bill.finish(input, 0.0, compactions)
}

/// Cost of the view layout: each turn's requests are tools, system prompt, the view and the
/// turn's own messages. Between turns the cached prefix is the tools and system prompt plus
/// the part of the previous view that no shrink rewrote. The compressor's measured usage is
/// added at `compressor_ratio` of the main-model price.
pub fn view_layout_cost(input: &CacheCostInput, state: CacheState) -> LayoutCost {
    let prices = &input.prices;
    let mut bill = Bill::default();
    let mut previous_view_bytes = 0u64;
    for (index, turn) in input.turns.iter().enumerate() {
        let view = input.view_turns.get(index).cloned().unwrap_or_default();
        let mut cacheable = 0;
        if state == CacheState::Warm && index > 0 {
            let kept_view = view
                .shrink_unchanged_prefix_bytes
                .iter()
                .fold(previous_view_bytes, |kept, unchanged| kept.min(*unchanged));
            cacheable = input.tools_system_bytes + kept_view;
        }
        let mut request_bytes = input.tools_system_bytes + view.view_bytes + turn.user_bytes;
        for call in &turn.calls {
            bill.request(prices, request_bytes, cacheable, call.output_bytes);
            cacheable = request_bytes;
            request_bytes += call.output_bytes + call.tool_result_bytes;
        }
        previous_view_bytes = view.view_bytes;
    }
    let usage = &input.compressor;
    let cached = usage.cached_tokens.min(usage.input_tokens) as f64;
    let written = usage.input_tokens as f64 - cached;
    let compressor = prices.compressor_ratio
        * (prices.cache_read * cached
            + prices.cache_write * written
            + prices.output * usage.output_tokens as f64);
    bill.finish(input, compressor, 0)
}
