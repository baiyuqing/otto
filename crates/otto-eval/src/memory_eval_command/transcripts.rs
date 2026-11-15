//! The synthetic transcripts: one in-memory session per item and variant, built through the
//! `Session` trait so the references and checkpoints are the ones the agent loop reads.

use chrono::{DateTime, Utc};
use otto_core::model::{Block, BlockType, FinishReason, Message, Role};
use otto_core::session::{
    CompactionCheckpoint, CompactionDetails, MEMORY_RECALL_CUSTOM_TYPE, MemoryRecordRef,
    MemoryReferenceCoverage, MemorySession, Session, encode_memory_recall,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

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
pub async fn build_transcript(item: &Item, variant: Variant) -> Result<Transcript, String> {
    let mut builder = TranscriptBuilder {
        session: MemorySession::new(),
        entries: Vec::new(),
    };
    let prior = &item.prior;
    let reference = MemoryRecordRef {
        id: item.record.id.clone(),
        revision: Some(item.record.revision.clone()),
    };
    match variant {
        Variant::NativeRecall | Variant::AssistantParaphrase => {
            let reply = if variant == Variant::NativeRecall {
                &prior.assistant_neutral
            } else {
                &prior.assistant
            };
            builder.message(Role::User, "prior-u1", &prior.user).await?;
            builder.recall(std::slice::from_ref(&reference)).await?;
            builder.message(Role::Assistant, "prior-a1", reply).await?;
        }
        Variant::ToolResult => {
            builder.message(Role::User, "prior-u1", &prior.user).await?;
            builder.tool_exchange(item).await?;
            builder
                .message(Role::Assistant, "prior-a2", &prior.assistant_neutral)
                .await?;
        }
        Variant::SummaryWithReferences | Variant::LegacySummary => {
            builder.message(Role::User, "prior-u1", &prior.user).await?;
            builder
                .message(Role::Assistant, "prior-a1", &prior.assistant_neutral)
                .await?;
            // The turns after the first carry an empty `otto.memory_recall` entry, as every
            // turn the agent writes does, so a rebuilt checkpoint can report complete coverage.
            builder.message(Role::User, "prior-u2", "Thanks.").await?;
            builder.recall(&[]).await?;
            builder
                .message(Role::Assistant, "prior-a2", "You're welcome.")
                .await?;
            // A compaction always keeps the newest turn verbatim, so a rebuild (condition D)
            // finds a summarizable turn only when the checkpoint keeps two turns.
            builder.message(Role::User, "prior-u3", "Noted.").await?;
            builder.recall(&[]).await?;
            builder
                .message(Role::Assistant, "prior-a3", "Understood.")
                .await?;
            let (records, coverage) = if variant == Variant::SummaryWithReferences {
                (vec![reference], MemoryReferenceCoverage::Complete)
            } else {
                (Vec::new(), MemoryReferenceCoverage::Unknown)
            };
            builder
                .checkpoint(&prior.summary, "prior-u2", records, coverage)
                .await?;
        }
    }
    Ok(Transcript {
        session: builder.session,
        entries: builder.entries,
    })
}

/// Appends to a session and mirrors each append in the entry list.
struct TranscriptBuilder {
    session: MemorySession,
    entries: Vec<TranscriptEntry>,
}

impl TranscriptBuilder {
    fn entry(&mut self, kind: &str, text: &str) {
        self.entries.push(TranscriptEntry {
            kind: kind.to_owned(),
            text: text.to_owned(),
        });
    }

    async fn append(&self, message: Message) -> Result<(), String> {
        let id = message.id.clone();
        self.session
            .append(message)
            .await
            .map_err(|error| format!("build transcript message {id}: {error}"))
    }

    async fn message(&mut self, role: Role, id: &str, text: &str) -> Result<(), String> {
        let kind = if role == Role::User {
            "user"
        } else {
            "assistant"
        };
        self.append(Message {
            id: id.to_owned(),
            role,
            blocks: vec![Block::text(text)],
            created_at: fixed_time(),
            ..Message::default()
        })
        .await?;
        self.entry(kind, text);
        Ok(())
    }

    /// Records the `otto.memory_recall` entry of the turn that started at the newest user
    /// message.
    async fn recall(&mut self, references: &[MemoryRecordRef]) -> Result<(), String> {
        self.session
            .append_custom(MEMORY_RECALL_CUSTOM_TYPE, &encode_memory_recall(references))
            .await
            .map_err(|error| format!("build transcript memory recall: {error}"))?;
        let ids: Vec<&str> = references
            .iter()
            .map(|reference| reference.id.as_str())
            .collect();
        self.entry("memory_recall", &ids.join(","));
        Ok(())
    }

    /// One tool call of `prior.tool_name` and its result holding `prior.tool_output`.
    async fn tool_exchange(&mut self, item: &Item) -> Result<(), String> {
        let prior = &item.prior;
        let arguments = serde_json::json!({ "command": prior.tool_input }).to_string();
        let raw = RawValue::from_string(arguments.clone())
            .map_err(|error| format!("build transcript tool input: {error}"))?;
        self.append(Message {
            id: "prior-a1".into(),
            role: Role::Assistant,
            finish_reason: Some(FinishReason::ToolCalls),
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: TOOL_CALL_ID.into(),
                tool_name: prior.tool_name.clone(),
                arguments: Some(raw),
                ..Block::default()
            }],
            created_at: fixed_time(),
            ..Message::default()
        })
        .await?;
        self.entry("tool_call", &format!("{} {arguments}", prior.tool_name));
        self.append(Message {
            id: "prior-t1".into(),
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: prior.tool_output.clone(),
                tool_call_id: TOOL_CALL_ID.into(),
                tool_name: prior.tool_name.clone(),
                ..Block::default()
            }],
            created_at: fixed_time(),
            ..Message::default()
        })
        .await?;
        self.entry("tool_result", &prior.tool_output);
        Ok(())
    }

    /// A checkpoint that summarizes everything before the message `first_kept_id`.
    async fn checkpoint(
        &mut self,
        summary: &str,
        first_kept_id: &str,
        memory_records: Vec<MemoryRecordRef>,
        memory_coverage: MemoryReferenceCoverage,
    ) -> Result<(), String> {
        self.session
            .append_compaction(CompactionCheckpoint {
                summary: summary.to_owned(),
                first_kept_entry_id: first_kept_id.to_owned(),
                tokens_before: CHECKPOINT_TOKENS_BEFORE,
                details: CompactionDetails {
                    memory_records,
                    memory_coverage,
                    ..CompactionDetails::default()
                },
                created_at: fixed_time(),
                ..CompactionCheckpoint::default()
            })
            .await
            .map_err(|error| format!("build transcript checkpoint: {error}"))?;
        self.entry("compaction", summary);
        Ok(())
    }
}

const TOOL_CALL_ID: &str = "prior-call-1";
/// The context size the constructed checkpoint claims to have replaced.
const CHECKPOINT_TOKENS_BEFORE: i64 = 200;

/// The creation time of every constructed message: fixed so transcripts are reproducible.
fn fixed_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("fixed timestamp is in range")
}

/// The artifact name of a transcript.
pub fn transcript_file(item_name: &str, variant: Variant) -> String {
    format!("transcripts/{item_name}-{}.jsonl", variant.as_str())
}
