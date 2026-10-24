//! The slash commands `otto acp` runs itself: `/compact [focus]`,
//! `/context`, `/model [profile]`, `/steer <guidance>`,
//! and `/queue <prompt>`.
//!
//! They are advertised with `available_commands_update` and arrive as the
//! text of an ordinary `session/prompt`. [`parse`] decides whether a prompt is
//! one of them; anything else, including `/compactx` or text that only
//! contains a command, is a normal task prompt. Replies are plain text and never
//! include system prompt, memory or message bodies.

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, SessionUpdate,
    UnstructuredCommandInput,
};
use otto_core::agent::context_report::{ContextReport, SectionKind};
use otto_core::wire::events::WireCompaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Command<'a> {
    Context,
    Permissions { args: &'a str },
    Compact { focus: &'a str },
    Model { profile: &'a str },
    Steer { guidance: &'a str },
    Queue { prompt: &'a str },
}

/// `Some` when the whole prompt is one of the supported slash commands.
pub(super) fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim_end();
    // `/auto` is a hidden alias of `/permissions`.
    if let Some(rest) = text
        .strip_prefix("/permissions")
        .or_else(|| text.strip_prefix("/auto"))
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(Command::Permissions { args: rest.trim() });
    }
    if text == "/context" {
        return Some(Command::Context);
    }
    if let Some(rest) = text.strip_prefix("/model")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(Command::Model {
            profile: rest.trim(),
        });
    }
    if let Some(rest) = text.strip_prefix("/steer")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(Command::Steer {
            guidance: rest.trim(),
        });
    }
    if let Some(rest) = text.strip_prefix("/queue")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(Command::Queue {
            prompt: rest.trim(),
        });
    }
    if let Some(rest) = text.strip_prefix("/compact")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
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
        AvailableCommand::new(
            "model",
            "Show the session's profile and model, or switch the session to another configured profile",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("optional profile"),
        )),
        AvailableCommand::new("steer", "Guide the active task at its next safe boundary").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("guidance")),
        ),
        AvailableCommand::new(
            "queue",
            "Queue a separate follow-up prompt for this session",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("prompt"),
        )),
        AvailableCommand::new("permissions", "Show the approval mode, or set manual|auto|yolo, policy on_request|never or reviewer user|auto_review"),
    ]))
}

pub(super) const CONTEXT_UNAVAILABLE: &str = "The context report is not available in this session.";

/// The `/model` report: the session's effective selection plus every profile a
/// switch may name. The profile list is what makes the command usable from a
/// chat client, which cannot read the configuration file.
pub(super) fn model_text(
    profile: &str,
    provider: &str,
    model: &str,
    thinking: &str,
    available: &[String],
) -> String {
    let mut lines = vec![
        format!("Profile: {profile}"),
        format!("Provider: {provider}"),
        format!("Model: {model}"),
    ];
    if !thinking.is_empty() {
        lines.push(format!("Thinking: {thinking}"));
    }
    if available.is_empty() {
        lines.push("No profiles are configured; add one on the host to switch.".to_string());
    } else {
        lines.push(format!("Configured profiles: {}", available.join(", ")));
        lines.push("Switch with /model <profile>.".to_string());
    }
    lines.join("\n")
}

/// The `/model <profile>` confirmation. It never claims the profile was saved:
/// a session switch leaves the configuration file and every other session
/// unchanged.
pub(super) fn model_switched_text(
    profile: &str,
    provider: &str,
    model: &str,
    thinking: &str,
) -> String {
    let thinking = if thinking.is_empty() {
        String::new()
    } else {
        format!(", thinking {thinking}")
    };
    format!("Switched to profile {profile} (provider {provider}, model {model}{thinking}).")
}

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
        if report.estimate_complete {
            format!("Estimated next request: {} tokens", report.estimated_total)
        } else {
            "Estimated next request: unknown (opaque provider context)".into()
        },
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
    match (result.tokens_before, result.estimated_tokens_after) {
        (Some(before), Some(after)) => format!(
            "Compacted the session context: {before} tokens before, about {after} tokens after (estimated)."
        ),
        (Some(before), None) => {
            format!("Compacted the session context: {before} tokens before.")
        }
        (None, _) => "Compacted the session context.".into(),
    }
}

#[cfg(test)]
mod tests {
    use otto_core::agent::context_report::{ContextItem, ContextSection};

    use super::*;

    #[test]
    fn parse_accepts_only_whole_commands() {
        assert_eq!(parse("/context"), Some(Command::Context));
        assert_eq!(parse("/context \n"), Some(Command::Context));
        assert_eq!(parse("/compact"), Some(Command::Compact { focus: "" }));
        assert_eq!(
            parse("/compact  keep the API \n design "),
            Some(Command::Compact {
                focus: "keep the API \n design"
            })
        );
        assert_eq!(parse("/compact\tx"), Some(Command::Compact { focus: "x" }));
        assert_eq!(
            parse("/steer  finish the current tool call first "),
            Some(Command::Steer {
                guidance: "finish the current tool call first"
            })
        );
        assert_eq!(parse("/steer"), Some(Command::Steer { guidance: "" }));
        assert_eq!(
            parse("/queue  check the implementation "),
            Some(Command::Queue {
                prompt: "check the implementation"
            })
        );
        assert_eq!(parse("/queue"), Some(Command::Queue { prompt: "" }));
        assert_eq!(parse("/model"), Some(Command::Model { profile: "" }));
        assert_eq!(
            parse("/model  terra "),
            Some(Command::Model { profile: "terra" })
        );
        assert_eq!(
            parse("/model\tterra"),
            Some(Command::Model { profile: "terra" })
        );
        for text in [
            "/compactx",
            "/reflect",
            "/reflect list",
            "/skill generated",
            "/skill revert example",
            "/contextual question",
            "/context now",
            "please /compact",
            " /context",
            "\t/compact",
            "/Compact",
            "/steerx",
            "/queuex",
            "/skillx",
            "/models",
            "/models work",
            "/modelx",
            "compact",
            "",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn available_commands_advertise_supported_commands() {
        let value = serde_json::to_value(available_commands()).expect("serializes");
        assert_eq!(value["sessionUpdate"], "available_commands_update");
        let commands = value["availableCommands"].as_array().expect("array");
        assert_eq!(commands[0]["name"], "compact");
        assert_eq!(commands[0]["input"]["hint"], "optional focus");
        assert_eq!(commands[1]["name"], "context");
        assert!(commands[1].get("input").is_none());
        assert_eq!(commands[2]["name"], "model");
        assert_eq!(commands[2]["input"]["hint"], "optional profile");
        assert_eq!(commands[3]["name"], "steer");
        assert_eq!(commands[4]["name"], "queue");
        assert_eq!(commands[5]["name"], "permissions");
        assert_eq!(commands.len(), 6);
        assert!(commands.iter().all(|command| command["name"] != "auto"));
        for alias in ["/auto off", "/auto", "/permissions off"] {
            assert!(
                matches!(parse(alias), Some(Command::Permissions { .. })),
                "{alias}"
            );
        }
        assert_eq!(
            parse("/auto off"),
            Some(Command::Permissions { args: "off" })
        );
        assert_eq!(parse("/autox"), None);
        assert_eq!(
            parse("/permissions policy never"),
            Some(Command::Permissions {
                args: "policy never"
            })
        );
        assert_eq!(parse("/permissionsx"), None);
    }

    #[test]
    fn model_text_reports_the_selection_and_every_profile() {
        let text = model_text(
            "terra",
            "chatgpt",
            "gpt-5.6-terra",
            "medium",
            &["chatgpt".to_string(), "terra".to_string()],
        );
        assert_eq!(
            text,
            "Profile: terra\nProvider: chatgpt\nModel: gpt-5.6-terra\nThinking: medium\n\
             Configured profiles: chatgpt, terra\nSwitch with /model <profile>."
        );
        let bare = model_text("terra", "chatgpt", "gpt-5.6-terra", "", &[]);
        assert!(bare.starts_with("Profile: terra\nProvider: chatgpt\nModel: gpt-5.6-terra\n"));
        assert!(bare.ends_with("add one on the host to switch."));
        assert!(!bare.contains("Thinking:"));

        assert_eq!(
            model_switched_text("terra", "chatgpt", "gpt-5.6-terra", "medium"),
            "Switched to profile terra (provider chatgpt, model gpt-5.6-terra, thinking medium)."
        );
        assert_eq!(
            model_switched_text("terra", "chatgpt", "gpt-5.6-terra", ""),
            "Switched to profile terra (provider chatgpt, model gpt-5.6-terra)."
        );
    }

    #[test]
    fn context_text_has_counts_and_no_item_text() {
        let report = ContextReport {
            model: "m".into(),
            context_window: 0,
            compaction_threshold: 0,
            estimated_total: 120,
            estimate_complete: true,
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
            tokens_before: Some(900),
            estimated_tokens_after: Some(200),
            ..WireCompaction::default()
        };
        assert!(compaction_text(&base).contains("900 tokens before, about 200 tokens after"));
        let unknown = WireCompaction {
            estimated_tokens_after: None,
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
