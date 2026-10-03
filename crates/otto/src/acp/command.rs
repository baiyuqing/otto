//! The slash commands `otto acp` runs itself: `/compact [focus]` and
//! `/context`.
//!
//! They are advertised with `available_commands_update` and arrive as the
//! text of an ordinary `session/prompt`. [`parse`] decides whether a prompt is
//! one of them; anything else, including `/compactx` or text that only
//! contains a command, is a normal prompt to the model. The reply texts are
//! plain text and never include system prompt, memory or message bodies.

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, SessionUpdate,
    UnstructuredCommandInput,
};
use otto_core::agent::context_report::{ContextReport, SectionKind};
use otto_core::wire::events::WireCompaction;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command<'a> {
    Context,
    Compact { focus: &'a str },
}

/// `Some` when the whole prompt is `/context`, `/compact`, or `/compact`
/// followed by whitespace and a focus text.
pub(super) fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim();
    if text == "/context" {
        return Some(Command::Context);
    }
    let rest = text.strip_prefix("/compact")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return Some(Command::Compact { focus: rest.trim() });
    }
    None
}

/// The `available_commands_update` sent after `session/new` and `session/load`.
pub(super) fn available_commands() -> SessionUpdate {
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
        AvailableCommand::new(
            "compact",
            "Summarize earlier messages to free context; optional focus text",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("optional focus"),
        )),
        AvailableCommand::new(
            "context",
            "Show what the next request sends, with token counts per part",
        ),
    ]))
}

pub(super) const CONTEXT_UNAVAILABLE: &str = "The context report is not available in this session.";

fn section_name(kind: SectionKind, items: usize) -> String {
    match kind {
        SectionKind::SystemPrompt => "System prompt".into(),
        SectionKind::Tools => format!("Tools (built-in, {items})"),
        SectionKind::McpTools => format!("Tools (MCP, {items})"),
        SectionKind::CompactionSummary => "Compaction summary".into(),
        SectionKind::Memory => "Memory (last turn)".into(),
        SectionKind::Messages => format!("Messages ({items})"),
    }
}

/// Counts only: item labels and texts are not included.
pub(super) fn context_text(report: &ContextReport) -> String {
    let window = match report.context_window {
        0 => "not configured".to_string(),
        tokens => tokens.to_string(),
    };
    let threshold = match report.compaction_threshold {
        0 => "off".to_string(),
        tokens => tokens.to_string(),
    };
    let mut lines = vec![
        format!("Model: {}", report.model),
        format!("Context window: {window}"),
        format!("Automatic compaction threshold: {threshold}"),
        format!("Estimated next request: {} tokens", report.estimated_total),
    ];
    if let Some(tokens) = report.reported_input_tokens {
        lines.push(format!("Last reported input: {tokens} tokens"));
    }
    for section in report.sections.iter().filter(|s| !s.items.is_empty()) {
        lines.push(format!(
            "{}: {} tokens",
            section_name(section.kind, section.items.len()),
            section.tokens
        ));
    }
    lines.join("\n")
}

pub(super) fn compaction_text(result: &WireCompaction) -> String {
    if result.noop {
        return "Nothing was compacted: the session has no earlier messages that can be \
                summarized safely."
            .to_string();
    }
    if result.estimated_tokens_after > 0 {
        format!(
            "Compacted the session context: {} tokens before, about {} tokens after (estimated).",
            result.tokens_before, result.estimated_tokens_after
        )
    } else {
        format!(
            "Compacted the session context: {} tokens before.",
            result.tokens_before
        )
    }
}

#[cfg(test)]
mod tests {
    use otto_core::agent::context_report::{ContextItem, ContextSection};

    use super::*;

    #[test]
    fn parse_accepts_only_whole_commands() {
        assert_eq!(parse("/context"), Some(Command::Context));
        assert_eq!(parse("  /context \n"), Some(Command::Context));
        assert_eq!(parse("/compact"), Some(Command::Compact { focus: "" }));
        assert_eq!(
            parse("/compact  keep the API \n design "),
            Some(Command::Compact {
                focus: "keep the API \n design"
            })
        );
        assert_eq!(parse("/compact\tx"), Some(Command::Compact { focus: "x" }));
        for text in [
            "/compactx",
            "/contextual question",
            "/context now",
            "please /compact",
            "/Compact",
            "compact",
            "",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn available_commands_name_both_commands_with_a_focus_hint() {
        let value = serde_json::to_value(available_commands()).expect("serializes");
        assert_eq!(value["sessionUpdate"], "available_commands_update");
        let commands = value["availableCommands"].as_array().expect("array");
        assert_eq!(commands[0]["name"], "compact");
        assert_eq!(commands[0]["input"]["hint"], "optional focus");
        assert_eq!(commands[1]["name"], "context");
        assert!(commands[1].get("input").is_none());
    }

    #[test]
    fn context_text_has_counts_and_no_item_text() {
        let report = ContextReport {
            model: "m".into(),
            context_window: 0,
            compaction_threshold: 0,
            estimated_total: 120,
            reported_input_tokens: None,
            sections: vec![ContextSection {
                kind: SectionKind::SystemPrompt,
                tokens: 100,
                items: vec![ContextItem {
                    label: "Label".into(),
                    tokens: 100,
                    text: "SECRET-BODY".into(),
                }],
            }],
        };
        let text = context_text(&report);
        assert_eq!(
            text,
            "Model: m\nContext window: not configured\nAutomatic compaction threshold: off\n\
             Estimated next request: 120 tokens\nSystem prompt: 100 tokens"
        );
    }

    #[test]
    fn compaction_text_covers_noop_estimated_and_unknown_after() {
        let base = WireCompaction {
            tokens_before: 900,
            estimated_tokens_after: 200,
            ..WireCompaction::default()
        };
        assert!(compaction_text(&base).contains("900 tokens before, about 200 tokens after"));
        let unknown = WireCompaction {
            estimated_tokens_after: 0,
            ..base.clone()
        };
        assert_eq!(
            compaction_text(&unknown),
            "Compacted the session context: 900 tokens before."
        );
        let noop = WireCompaction { noop: true, ..base };
        assert!(compaction_text(&noop).starts_with("Nothing was compacted"));
    }
}
