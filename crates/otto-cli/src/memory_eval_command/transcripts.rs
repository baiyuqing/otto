//! The synthetic transcripts: one in-memory session per item and variant, built through the
//! `Session` trait so the references and checkpoints are the ones the agent loop reads.

use otto_core::session::MemorySession;
use serde::{Deserialize, Serialize};

use super::argument_parsing::Variant;
use super::items::Item;

/// One entry of a constructed transcript, written to `transcripts/<item>-<variant>.jsonl`
/// for inspection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// `user`, `assistant`, `tool_call`, `tool_result`, `memory_recall` or `compaction`.
    pub kind: String,
    /// The message text, the tool input or output, the referenced record ids or the summary.
    pub text: String,
}

/// The session a case starts from and its entries for the artifact.
pub struct Transcript {
    pub session: MemorySession,
    pub entries: Vec<TranscriptEntry>,
}

/// Builds the prior of `variant` for `item`.
pub async fn build_transcript(_item: &Item, _variant: Variant) -> Result<Transcript, String> {
    Err("not implemented".into())
}

/// The artifact name of a transcript.
pub fn transcript_file(item_name: &str, variant: Variant) -> String {
    format!("transcripts/{item_name}-{}.jsonl", variant.as_str())
}
