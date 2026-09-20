//! The event set, the compaction value types, and the turn settings.
//!
//! An event is an enum rather than one struct with a type tag and a union of
//! unused fields, so a consumer cannot read a field the event does not carry.
//!
//! Ownership: every value here is plain owned data with no interior mutability.
//! Events are handed to the sink by value.
//!
//! Errors: [`AgentError`] is what [`super::Agent::run`] returns and what
//! [`Event::AgentError`] reports. The two always agree.

use crate::model::Usage;
use crate::provider::ProviderError;
use crate::session::SessionError;
use crate::tool::ToolResult;

/// The outcome the agent records for one provider HTTP call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStatus {
    Ok,
    Canceled,
    Error,
}

impl ApiStatus {
    /// The `APIStatus` string carried on the event.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Canceled => "canceled",
            Self::Error => "error",
        }
    }
}

/// Why a checkpoint was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionReason {
    #[default]
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// The summarization shape the selection resolved to. It is deterministic and
/// known before the provider summary call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionMode {
    Structured,
    TurnPrefix,
    SplitTurn,
}

impl CompactionMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::TurnPrefix => "turn-prefix",
            Self::SplitTurn => "split-turn",
        }
    }
}

/// What one completed compaction did. Also the payload of
/// [`Event::CompactionStarted`] and [`Event::CompactionCompleted`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionResult {
    pub checkpoint_id: String,
    pub reason: CompactionReason,
    pub tokens_before: i64,
    pub estimated_tokens_after: i64,
    pub automatic: bool,
    pub usage: Usage,
    /// False when no summary call reported usage; `usage` is then zero.
    pub usage_present: bool,
    /// True when there was no safe historic prefix, so nothing was written.
    pub noop: bool,
}

/// The pre-execution view of a compaction: what it will summarize versus
/// retain, and a token estimate. It carries no summary text, which does not
/// exist until the provider call runs. `estimated_tokens_after` is a floor
/// that excludes the not-yet-generated summary; [`CompactionResult`] on
/// completion carries the exact post-checkpoint estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub reason: CompactionReason,
    pub automatic: bool,
    pub tokens_before: i64,
    pub estimated_tokens_after: i64,
    pub summarized_messages: usize,
    pub retained_messages: usize,
    pub mode: CompactionMode,
}

/// The window sizes that drive automatic compaction. All zero disables it:
/// [`super::automatic_compaction_triggers`] reports unknown limits and the
/// dispatch path never compacts on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Whether the run loop may compact without being asked.
    pub auto: bool,
    /// The provider's hard input ceiling, in tokens.
    pub hard_input_window: i64,
    /// The lower ceiling the agent aims to stay under, in tokens.
    pub working_window: i64,
    /// Headroom subtracted from both windows to leave room for the reply.
    pub reserve_tokens: i64,
    /// How much of the recent transcript compaction tries to keep verbatim.
    pub keep_recent_tokens: i64,
}

/// Everything the agent reports while a turn runs.
///
/// Events are delivered synchronously and in order from inside `run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    AgentStarted,
    AgentFinished,
    TextDelta {
        text: String,
    },
    ToolCallStarted {
        tool_name: String,
        tool_call_id: String,
        /// The raw JSON arguments as the provider sent them.
        arguments: String,
    },
    ToolCallFinished {
        tool_name: String,
        tool_call_id: String,
        result: ToolResult,
    },
    ProviderUsage {
        usage: Usage,
        /// False when the provider reported no usage; `usage` is then zero.
        present: bool,
    },
    ProviderApiCall {
        provider: String,
        model: String,
        duration: std::time::Duration,
        status: ApiStatus,
    },
    CompactionStarted {
        compaction: CompactionResult,
    },
    CompactionPlanned {
        plan: CompactionPlan,
    },
    CompactionCompleted {
        compaction: CompactionResult,
    },
    /// An automatic compaction failed below the hard input limit. The turn
    /// continues with the uncompacted request.
    CompactionWarning {
        message: String,
    },
    /// Memory recall failed. The turn continues without recalled records.
    MemoryWarning {
        message: String,
    },
    /// One inbox notification was delivered into the transcript.
    Notification {
        task_id: String,
        text: String,
        usage: Usage,
        present: bool,
    },
    AgentError {
        /// The `Display` text of the [`AgentError`] `run` is about to return.
        message: String,
    },
}

impl Event {
    /// The wire `EventType` string for this event; frontends key off these
    /// names.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AgentStarted => "agent_started",
            Self::AgentFinished => "agent_finished",
            Self::TextDelta { .. } => "text_delta",
            Self::ToolCallStarted { .. } => "tool_call_started",
            Self::ToolCallFinished { .. } => "tool_call_finished",
            Self::ProviderUsage { .. } => "provider_usage",
            Self::ProviderApiCall { .. } => "provider_api_call",
            Self::CompactionStarted { .. } => "compaction_started",
            Self::CompactionPlanned { .. } => "compaction_planned",
            Self::CompactionCompleted { .. } => "compaction_completed",
            Self::CompactionWarning { .. } => "compaction_warning",
            Self::MemoryWarning { .. } => "memory_warning",
            Self::Notification { .. } => "notification",
            Self::AgentError { .. } => "agent_error",
        }
    }
}

/// The event callback a caller passes to [`super::Agent::run`]. See
/// [`crate::provider::StreamSink`] for why the `Send` bound is target
/// dependent.
#[cfg(not(target_arch = "wasm32"))]
pub type EventSink<'a> = &'a mut (dyn FnMut(Event) + Send);
/// See the native definition above.
#[cfg(target_arch = "wasm32")]
pub type EventSink<'a> = &'a mut dyn FnMut(Event);

/// Why a turn stopped early.
///
/// Frontends surface the `Display` text of each variant verbatim.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("user text is required")]
    EmptyUserText,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("persist {kind}: {source}")]
    Persist {
        /// What the agent was trying to store, for example `user message`.
        kind: String,
        source: SessionError,
    },
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    /// There was no safe historic prefix to summarize. A successful no-op for a
    /// manual compaction.
    #[error("nothing to compact")]
    NothingToCompact,
    /// The messages that must stay verbatim already exceed the retained budget.
    #[error("current turn exceeds the retained input budget")]
    CurrentTurnTooLarge,
    /// The summary request or its response failed validation. The cause is
    /// appended to the message.
    #[error("invalid compaction summary{}", detail(.0))]
    InvalidCompactionSummary(String),
    /// A compaction step failed at a boundary the agent does not own: the
    /// selection, the provider call, or the durable append.
    #[error("{message}")]
    CompactionBoundary { message: String, cause: String },
    /// An automatic compaction path gave up. `message` is one of the
    /// [`super::overflow`] constants and is shown to the user as is.
    #[error("{message}")]
    AutomaticDispatch {
        message: String,
        causes: Vec<String>,
    },
    /// An error whose message went through [`super::redactor::Redactor`].
    ///
    /// The original variant is gone, so the classification callers need is
    /// carried as flags.
    #[error("{message}")]
    Redacted {
        message: String,
        cancelled: bool,
        empty_user_text: bool,
        invalid_compaction_summary: bool,
    },
    /// Everything else, carrying the error text.
    #[error("{0}")]
    Other(String),
}

fn detail(cause: &str) -> String {
    if cause.is_empty() {
        String::new()
    } else {
        format!(": {cause}")
    }
}

impl AgentError {
    /// Whether this error is, or wraps, cancellation.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Provider(ProviderError::Cancelled)
                | Self::Redacted {
                    cancelled: true,
                    ..
                }
        )
    }

    /// Whether this is [`AgentError::NothingToCompact`].
    pub fn is_nothing_to_compact(&self) -> bool {
        matches!(self, Self::NothingToCompact)
    }

    /// Whether this is, or wraps, [`AgentError::EmptyUserText`].
    pub fn is_empty_user_text(&self) -> bool {
        matches!(
            self,
            Self::EmptyUserText
                | Self::Redacted {
                    empty_user_text: true,
                    ..
                }
        )
    }

    /// Whether this is, or wraps, [`AgentError::InvalidCompactionSummary`].
    pub fn is_invalid_compaction_summary(&self) -> bool {
        matches!(
            self,
            Self::InvalidCompactionSummary(_)
                | Self::Redacted {
                    invalid_compaction_summary: true,
                    ..
                }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_match_the_wire_type_constants() {
        let names = [
            Event::AgentStarted.name(),
            Event::AgentFinished.name(),
            Event::TextDelta {
                text: String::new(),
            }
            .name(),
            Event::ToolCallStarted {
                tool_name: String::new(),
                tool_call_id: String::new(),
                arguments: String::new(),
            }
            .name(),
            Event::ToolCallFinished {
                tool_name: String::new(),
                tool_call_id: String::new(),
                result: ToolResult::default(),
            }
            .name(),
            Event::ProviderUsage {
                usage: Usage::default(),
                present: false,
            }
            .name(),
            Event::ProviderApiCall {
                provider: String::new(),
                model: String::new(),
                duration: std::time::Duration::ZERO,
                status: ApiStatus::Ok,
            }
            .name(),
            Event::CompactionStarted {
                compaction: CompactionResult::default(),
            }
            .name(),
            Event::CompactionPlanned {
                plan: CompactionPlan {
                    reason: CompactionReason::Manual,
                    automatic: false,
                    tokens_before: 0,
                    estimated_tokens_after: 0,
                    summarized_messages: 0,
                    retained_messages: 0,
                    mode: CompactionMode::Structured,
                },
            }
            .name(),
            Event::CompactionCompleted {
                compaction: CompactionResult::default(),
            }
            .name(),
            Event::CompactionWarning {
                message: String::new(),
            }
            .name(),
            Event::MemoryWarning {
                message: String::new(),
            }
            .name(),
            Event::Notification {
                task_id: String::new(),
                text: String::new(),
                usage: Usage::default(),
                present: false,
            }
            .name(),
            Event::AgentError {
                message: String::new(),
            }
            .name(),
        ];
        assert_eq!(
            names,
            [
                "agent_started",
                "agent_finished",
                "text_delta",
                "tool_call_started",
                "tool_call_finished",
                "provider_usage",
                "provider_api_call",
                "compaction_started",
                "compaction_planned",
                "compaction_completed",
                "compaction_warning",
                "memory_warning",
                "notification",
                "agent_error",
            ]
        );
    }

    #[test]
    fn error_text_matches_the_sentinel_strings() {
        assert_eq!(
            AgentError::EmptyUserText.to_string(),
            "user text is required"
        );
        assert_eq!(
            AgentError::NothingToCompact.to_string(),
            "nothing to compact"
        );
        assert_eq!(
            AgentError::CurrentTurnTooLarge.to_string(),
            "current turn exceeds the retained input budget"
        );
        assert_eq!(
            AgentError::InvalidCompactionSummary(String::new()).to_string(),
            "invalid compaction summary"
        );
        assert_eq!(
            AgentError::InvalidCompactionSummary("response is empty".into()).to_string(),
            "invalid compaction summary: response is empty"
        );
    }

    #[test]
    fn compaction_names_match_the_wire_constants() {
        assert_eq!(CompactionReason::Manual.name(), "manual");
        assert_eq!(CompactionReason::Threshold.name(), "threshold");
        assert_eq!(CompactionReason::Overflow.name(), "overflow");
        assert_eq!(CompactionMode::Structured.name(), "structured");
        assert_eq!(CompactionMode::TurnPrefix.name(), "turn-prefix");
        assert_eq!(CompactionMode::SplitTurn.name(), "split-turn");
        assert_eq!(ApiStatus::Canceled.name(), "canceled");
    }
}
