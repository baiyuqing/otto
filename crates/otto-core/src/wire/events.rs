//! The JSON shape of one turn event.
//!
//! Port of `internal/server/events.go`. Field order in each struct is the
//! order Go declares, so `serde_json` emits the same byte sequence the Go
//! server does.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::agent::events::{CompactionPlan, CompactionResult, Event};
use crate::model::Usage;

fn is_empty(value: &str) -> bool {
    value.is_empty()
}

/// `tool_call_finished`'s payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolResult {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

/// `compaction_started` and `compaction_completed`'s payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCompaction {
    #[serde(default, skip_serializing_if = "is_empty")]
    pub checkpoint_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub tokens_before: i64,
    #[serde(default)]
    pub estimated_tokens_after: i64,
    #[serde(default)]
    pub automatic: bool,
    /// Present only when the summary call reported usage, matching Go's
    /// `if c.UsagePresent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub noop: bool,
}

/// `compaction_planned`'s payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePlan {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub tokens_before: i64,
    #[serde(default)]
    pub estimated_tokens_after: i64,
    #[serde(default)]
    pub summarized_messages: usize,
    #[serde(default)]
    pub retained_messages: usize,
    #[serde(default)]
    pub mode: String,
}

/// One event as it crosses the HTTP boundary.
///
/// `tool_args` stays a [`RawValue`] so provider argument JSON reaches the
/// browser with its key order and number literals unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub task_id: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<WireToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<WireCompaction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<WirePlan>,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub error: String,
}

impl PartialEq for WireEvent {
    /// `tool_args` compares by raw JSON text, since [`RawValue`] has no
    /// `PartialEq`.
    fn eq(&self, other: &Self) -> bool {
        self.event_type == other.event_type
            && self.turn_id == other.turn_id
            && self.task_id == other.task_id
            && self.text == other.text
            && self.tool_name == other.tool_name
            && self.tool_call_id == other.tool_call_id
            && self.tool_args.as_ref().map(|raw| raw.get())
                == other.tool_args.as_ref().map(|raw| raw.get())
            && self.result == other.result
            && self.usage == other.usage
            && self.usage_present == other.usage_present
            && self.compaction == other.compaction
            && self.plan == other.plan
            && self.error == other.error
    }
}

impl Eq for WireEvent {}

/// Converts an agent event to its wire form. Every nested value is rebuilt,
/// so the result borrows nothing from `event`.
pub fn to_wire(event: &Event) -> WireEvent {
    let mut wire = WireEvent {
        event_type: event.name().to_string(),
        ..WireEvent::default()
    };
    match event {
        Event::TextDelta { text } => wire.text = text.clone(),
        Event::ToolCallStarted {
            tool_name,
            tool_call_id,
            arguments,
        } => {
            wire.tool_name = tool_name.clone();
            wire.tool_call_id = tool_call_id.clone();
            wire.tool_args = tool_args_raw(arguments);
        }
        Event::ToolCallFinished {
            tool_name,
            tool_call_id,
            result,
        } => {
            wire.tool_name = tool_name.clone();
            wire.tool_call_id = tool_call_id.clone();
            wire.result = Some(WireToolResult {
                content: result.content.clone(),
                is_error: result.is_error,
            });
        }
        Event::ProviderUsage { usage, present } => {
            wire.usage = Some(*usage);
            wire.usage_present = Some(*present);
        }
        Event::Notification {
            task_id,
            text,
            usage,
            present,
        } => {
            wire.task_id = task_id.clone();
            wire.text = text.clone();
            wire.usage = Some(*usage);
            wire.usage_present = Some(*present);
        }
        Event::CompactionStarted { compaction } | Event::CompactionCompleted { compaction } => {
            wire.compaction = Some(to_wire_compaction(compaction));
        }
        Event::CompactionPlanned { plan } => wire.plan = Some(to_wire_plan(plan)),
        Event::CompactionWarning { message }
        | Event::MemoryWarning { message }
        | Event::AgentError { message } => wire.error = message.clone(),
        Event::AgentStarted | Event::AgentFinished | Event::ProviderApiCall { .. } => {}
    }
    wire
}

/// Passes valid JSON through unchanged and JSON-quotes anything else, so the
/// browser always receives parseable arguments. Empty text carries nothing.
pub fn tool_args_raw(args: &str) -> Option<Box<RawValue>> {
    if args.is_empty() {
        return None;
    }
    if let Ok(raw) = RawValue::from_string(args.to_string()) {
        return Some(raw);
    }
    let quoted = serde_json::to_string(args).ok()?;
    RawValue::from_string(quoted).ok()
}

pub fn to_wire_compaction(result: &CompactionResult) -> WireCompaction {
    WireCompaction {
        checkpoint_id: result.checkpoint_id.clone(),
        reason: result.reason.name().to_string(),
        tokens_before: result.tokens_before,
        estimated_tokens_after: result.estimated_tokens_after,
        automatic: result.automatic,
        usage: result.usage_present.then_some(result.usage),
        noop: result.noop,
    }
}

pub fn to_wire_plan(plan: &CompactionPlan) -> WirePlan {
    WirePlan {
        reason: plan.reason.name().to_string(),
        automatic: plan.automatic,
        tokens_before: plan.tokens_before,
        estimated_tokens_after: plan.estimated_tokens_after,
        summarized_messages: plan.summarized_messages,
        retained_messages: plan.retained_messages,
        mode: plan.mode.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::{ApiStatus, CompactionMode, CompactionReason};
    use crate::tool::ToolResult;

    fn json(event: &Event) -> String {
        serde_json::to_string(&to_wire(event)).expect("wire event serializes")
    }

    #[test]
    fn text_delta_carries_only_the_text() {
        assert_eq!(
            json(&Event::TextDelta { text: "hi".into() }),
            r#"{"type":"text_delta","text":"hi"}"#
        );
    }

    #[test]
    fn tool_call_started_passes_valid_argument_json_through() {
        assert_eq!(
            json(&Event::ToolCallStarted {
                tool_name: "bash".into(),
                tool_call_id: "c1".into(),
                arguments: r#"{"b":1,"a":2}"#.into(),
            }),
            r#"{"type":"tool_call_started","tool_name":"bash","tool_call_id":"c1","tool_args":{"b":1,"a":2}}"#
        );
    }

    #[test]
    fn tool_call_started_quotes_arguments_that_are_not_json() {
        assert_eq!(
            json(&Event::ToolCallStarted {
                tool_name: "bash".into(),
                tool_call_id: "c1".into(),
                arguments: "not json".into(),
            }),
            r#"{"type":"tool_call_started","tool_name":"bash","tool_call_id":"c1","tool_args":"not json"}"#
        );
    }

    #[test]
    fn empty_arguments_carry_no_tool_args_field() {
        assert!(tool_args_raw("").is_none());
    }

    #[test]
    fn tool_call_finished_carries_the_result() {
        assert_eq!(
            json(&Event::ToolCallFinished {
                tool_name: "read".into(),
                tool_call_id: "c2".into(),
                result: ToolResult {
                    content: "x".into(),
                    persisted_content: None,
                    is_error: true,
                },
            }),
            r#"{"type":"tool_call_finished","tool_name":"read","tool_call_id":"c2","result":{"content":"x","is_error":true}}"#
        );
    }

    #[test]
    fn provider_usage_always_carries_usage_and_the_present_flag() {
        assert_eq!(
            json(&Event::ProviderUsage {
                usage: Usage::default(),
                present: false,
            }),
            r#"{"type":"provider_usage","usage":{"input_tokens":0,"output_tokens":0},"usage_present":false}"#
        );
    }

    #[test]
    fn notification_carries_task_id_text_and_usage() {
        assert_eq!(
            json(&Event::Notification {
                task_id: "t1".into(),
                text: "done".into(),
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                },
                present: true,
            }),
            r#"{"type":"notification","task_id":"t1","text":"done","usage":{"input_tokens":5,"output_tokens":2},"usage_present":true}"#
        );
    }

    #[test]
    fn compaction_omits_usage_when_not_present() {
        let compaction = CompactionResult {
            checkpoint_id: "m9".into(),
            reason: CompactionReason::Threshold,
            tokens_before: 900,
            estimated_tokens_after: 300,
            automatic: true,
            usage: Usage::default(),
            usage_present: false,
            noop: false,
        };
        assert_eq!(
            json(&Event::CompactionCompleted { compaction }),
            r#"{"type":"compaction_completed","compaction":{"checkpoint_id":"m9","reason":"threshold","tokens_before":900,"estimated_tokens_after":300,"automatic":true,"noop":false}}"#
        );
    }

    #[test]
    fn compaction_planned_carries_the_plan() {
        assert_eq!(
            json(&Event::CompactionPlanned {
                plan: CompactionPlan {
                    reason: CompactionReason::Overflow,
                    automatic: false,
                    tokens_before: 10,
                    estimated_tokens_after: 4,
                    summarized_messages: 3,
                    retained_messages: 2,
                    mode: CompactionMode::SplitTurn,
                },
            }),
            r#"{"type":"compaction_planned","plan":{"reason":"overflow","automatic":false,"tokens_before":10,"estimated_tokens_after":4,"summarized_messages":3,"retained_messages":2,"mode":"split-turn"}}"#
        );
    }

    #[test]
    fn warnings_and_errors_land_in_the_error_field() {
        for event in [
            Event::CompactionWarning {
                message: "w".into(),
            },
            Event::MemoryWarning {
                message: "w".into(),
            },
            Event::AgentError {
                message: "w".into(),
            },
        ] {
            let name = event.name();
            assert_eq!(json(&event), format!(r#"{{"type":"{name}","error":"w"}}"#));
        }
    }

    #[test]
    fn provider_api_call_carries_only_its_type() {
        assert_eq!(
            json(&Event::ProviderApiCall {
                provider: "openai-compatible".into(),
                model: "gpt".into(),
                duration: std::time::Duration::from_millis(5),
                status: ApiStatus::Ok,
            }),
            r#"{"type":"provider_api_call"}"#
        );
    }
}
