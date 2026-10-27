//! Summary-tree view of a message log (I/O-free).
//!
//! The log is append-only. Each log message has a permanent id. A [`SummaryTree`] holds
//! one text of at most [`NODE_MAXIMUM_BYTES`] bytes (when the compressor obeys) for each
//! aligned range of messages. A [`View`] is the ordered list of nodes that covers the log
//! once; recent ranges are small nodes and old ranges are large nodes. [`zoom`] opens a
//! node into its children.

mod build_schedule;
mod cache_cost;
mod log_messages;
mod node_address;
mod summary_tree;
mod view_prompts;
mod view_selection;
mod zoom_lookup;

pub use build_schedule::BuildSchedule;
pub use cache_cost::{
    CacheCostInput, CacheCostReport, CachePrices, CacheState, CompactionLimits, CompressorUsage,
    LayoutCost, ModelCallBytes, TurnBytes, ViewTurnBytes, cache_cost_report, current_layout_cost,
    view_layout_cost,
};
pub use log_messages::{LogKind, LogMessage, log_from_history};
pub use node_address::NodeAddress;
pub use summary_tree::{LeafTask, MergeTask, NodeInput, SummaryTree, render_line};
pub use view_prompts::{SYSTEM_PROMPT, leaf_task, merge_task, too_long_task};
pub use view_selection::{ShrinkEvent, View};
pub use zoom_lookup::{date, tool_definitions, zoom};

/// Largest size in bytes of one tree node text.
pub const NODE_MAXIMUM_BYTES: usize = 512;

#[cfg(test)]
mod tests;
