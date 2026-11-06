//! Tests of the constructed transcripts: references, checkpoints and the copies of the claim.

use otto_core::model::Message;
use otto_core::session::Session;

use crate::memory_eval_command::answer_checks::contains_term;
use crate::memory_eval_command::argument_parsing::Variant;
use crate::memory_eval_command::items::{Item, embedded_items};
use crate::memory_eval_command::transcripts::{build_transcript, transcript_file};

/// The text of every block, tool results included.
fn all_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| message.blocks.iter().map(|block| block.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn holds_claim(item: &Item, text: &str) -> bool {
    item.obsolete_terms
        .iter()
        .any(|term| contains_term(text, term))
}

#[tokio::test]
async fn references_and_checkpoints_follow_the_variant() {
    for item in embedded_items().expect("items") {
        for variant in Variant::ALL {
            let transcript = build_transcript(&item, variant).await.expect("transcript");
            let session = &transcript.session;
            let references = session.memory_references();
            let referenced = matches!(
                variant,
                Variant::NativeRecall | Variant::AssistantParaphrase
            );
            let any_record = references.iter().any(|turn| !turn.records.is_empty());
            assert_eq!(any_record, referenced, "{} {variant:?}", item.name);
            if referenced {
                assert_eq!(references.len(), 1);
                assert_eq!(references[0].user_message_id, "prior-u1");
                assert_eq!(references[0].records.len(), 1);
                assert_eq!(references[0].records[0].id, item.record.id);
                assert_eq!(
                    references[0].records[0].revision.as_deref(),
                    Some(item.record.revision.as_str())
                );
            }
            let latest = session.latest_compaction();
            assert_eq!(
                latest.is_some(),
                variant.has_summary(),
                "{} {variant:?}",
                item.name
            );
            if let Some(latest) = latest {
                assert_eq!(latest.summary, item.prior.summary);
                assert_eq!(
                    !latest.details.memory_records.is_empty(),
                    variant == Variant::SummaryWithReferences
                );
                assert!(latest.tokens_before > 0);
            }
        }
    }
}

#[tokio::test]
async fn claim_reaches_the_model_messages_in_the_four_copy_variants_only() {
    for item in embedded_items().expect("items") {
        for variant in Variant::ALL {
            let transcript = build_transcript(&item, variant).await.expect("transcript");
            let text = all_text(&transcript.session.model_messages());
            let expected = !matches!(variant, Variant::NativeRecall);
            assert_eq!(
                holds_claim(&item, &text),
                expected,
                "{} {variant:?}: {text}",
                item.name
            );
            assert!(
                !text.contains(&item.record.marker),
                "{} {variant:?}",
                item.name
            );
            let entries: String = transcript
                .entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect();
            assert!(
                !entries.contains(&item.record.marker),
                "{} {variant:?}",
                item.name
            );
        }
    }
}

async fn entry_kinds(item: &Item, variant: Variant) -> Vec<String> {
    let transcript = build_transcript(item, variant).await.expect("transcript");
    transcript
        .entries
        .into_iter()
        .map(|entry| entry.kind)
        .collect()
}

#[tokio::test]
async fn entry_kinds_follow_the_variant() {
    let item = embedded_items().expect("items").remove(0);
    assert_eq!(
        entry_kinds(&item, Variant::NativeRecall).await,
        ["user", "memory_recall", "assistant"]
    );
    assert_eq!(
        entry_kinds(&item, Variant::AssistantParaphrase).await,
        ["user", "memory_recall", "assistant"]
    );
    assert_eq!(
        entry_kinds(&item, Variant::ToolResult).await,
        ["user", "tool_call", "tool_result", "assistant"]
    );
    let summary = [
        "user",
        "assistant",
        "user",
        "memory_recall",
        "assistant",
        "user",
        "memory_recall",
        "assistant",
        "compaction",
    ];
    assert_eq!(
        entry_kinds(&item, Variant::SummaryWithReferences).await,
        summary
    );
    assert_eq!(entry_kinds(&item, Variant::LegacySummary).await, summary);
}

#[test]
fn transcript_file_name_holds_item_and_variant() {
    assert_eq!(
        transcript_file("install_command_pnpm", Variant::ToolResult),
        "transcripts/install_command_pnpm-tool_result.jsonl"
    );
}
