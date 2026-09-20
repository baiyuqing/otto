//! Automatic compaction triggers and the turn-scoped dispatch state.
//!
//! The agent compacts on its own in two situations: before a request whose
//! estimate crosses the soft trigger, and after a request the provider rejected
//! for context overflow. Both are attempted at most once per turn.
//!
//! Ownership: [`RunDispatchState`] lives for one `Agent::run` call and is
//! dropped with it, so the tool-result overlay never outlives the turn.

use std::collections::HashMap;

use crate::provider::ProviderError;

use super::{AgentError, CompactionSettings};

/// Proactive compaction failed, but the request still fits under the hard
/// input limit, so the turn continues uncompacted.
pub const AUTOMATIC_COMPACTION_WARNING_MESSAGE: &str = "automatic context compaction failed below the hard input limit; continuing with the original request";
/// Proactive compaction failed at or above the hard input limit, so the turn
/// cannot continue.
pub const AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE: &str =
    "automatic context compaction failed at the hard input limit";
/// Compaction succeeded but the result is still over the hard input limit.
pub const AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE: &str =
    "compacted context still exceeds the hard input limit";
/// The turn already spent its one proactive compaction attempt and is still
/// over the hard input limit.
pub const AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE: &str =
    "context exceeds the hard input limit after the automatic compaction attempt";
/// Overflow recovery could not find anything safe to compact.
pub const OVERFLOW_COMPACTION_FAILURE_MESSAGE: &str =
    "context overflow recovery could not compact the current request";
/// The provider reported overflow again after the one retry.
pub const OVERFLOW_RETRY_FAILURE_MESSAGE: &str =
    "context overflow persisted after one automatic compaction retry";

/// What one `Agent::run` call carries across its provider steps.
#[derive(Debug, Default)]
pub struct RunDispatchState {
    /// Whether the proactive compaction attempt has been spent this turn.
    pub proactive_attempted: bool,
    /// The rendered memory block, prepended to every request of this turn.
    pub memory_context: String,
    /// The full redacted tool-result text, keyed by tool-call id, for results
    /// whose stored text is a placeholder. It is substituted into provider
    /// requests only, never into the session.
    pub tool_result_overlay: HashMap<String, String>,
}

/// Builds the error an automatic dispatch path gives up with, dropping causes
/// that carry no text.
pub fn automatic_dispatch_error(message: &str, causes: Vec<String>) -> AgentError {
    AgentError::AutomaticDispatch {
        message: message.to_owned(),
        causes: causes
            .into_iter()
            .filter(|cause| !cause.is_empty())
            .collect(),
    }
}

/// Returns the cancellation to report instead of `error`, if the turn was
/// cancelled.
pub fn automatic_cancellation(cancelled: bool, error: &AgentError) -> Option<AgentError> {
    if cancelled {
        return Some(AgentError::Provider(ProviderError::Cancelled));
    }
    if error.is_cancelled() {
        return Some(AgentError::Provider(ProviderError::Cancelled));
    }
    None
}

/// The soft and hard request estimates that start automatic compaction.
///
/// Returns `None` when either window is unset, which means the limits are
/// unknown and automatic compaction stays off.
pub fn automatic_compaction_triggers(settings: &CompactionSettings) -> Option<(i64, i64)> {
    if settings.working_window <= 0 || settings.hard_input_window <= 0 {
        return None;
    }
    let reserve = settings.reserve_tokens.max(0);
    Some((
        settings.working_window - reserve,
        settings.hard_input_window - reserve,
    ))
}

/// Whether the provider rejected the request because the context did not fit.
pub fn is_typed_context_overflow(error: &AgentError) -> bool {
    matches!(error, AgentError::Provider(ProviderError::Overflow(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_subtract_the_reserve_from_both_windows() {
        let settings = CompactionSettings {
            working_window: 100_000,
            hard_input_window: 200_000,
            reserve_tokens: 16_384,
            ..CompactionSettings::default()
        };
        assert_eq!(
            automatic_compaction_triggers(&settings),
            Some((83_616, 183_616))
        );
    }

    #[test]
    fn a_negative_reserve_counts_as_zero() {
        let settings = CompactionSettings {
            working_window: 100,
            hard_input_window: 200,
            reserve_tokens: -5,
            ..CompactionSettings::default()
        };
        assert_eq!(automatic_compaction_triggers(&settings), Some((100, 200)));
    }

    #[test]
    fn an_unset_window_makes_the_limits_unknown() {
        for (working, hard) in [(0, 200), (100, 0), (0, 0), (-1, 200)] {
            let settings = CompactionSettings {
                working_window: working,
                hard_input_window: hard,
                ..CompactionSettings::default()
            };
            assert_eq!(automatic_compaction_triggers(&settings), None);
        }
    }

    #[test]
    fn only_the_typed_overflow_error_counts_as_overflow() {
        let overflow = AgentError::Provider(ProviderError::Overflow(
            crate::provider::ContextOverflowError::default(),
        ));
        assert!(is_typed_context_overflow(&overflow));
        assert!(!is_typed_context_overflow(&AgentError::Other(
            "context length exceeded".into()
        )));
    }

    #[test]
    fn cancellation_wins_over_the_reported_error() {
        let error = AgentError::Other("boom".into());
        assert!(automatic_cancellation(true, &error).is_some());
        assert!(automatic_cancellation(false, &error).is_none());
        assert!(
            automatic_cancellation(false, &AgentError::Provider(ProviderError::Cancelled))
                .is_some()
        );
    }

    #[test]
    fn dispatch_errors_drop_empty_causes() {
        let error = automatic_dispatch_error(
            OVERFLOW_RETRY_FAILURE_MESSAGE,
            vec![String::new(), "overflow".into()],
        );
        assert_eq!(error.to_string(), OVERFLOW_RETRY_FAILURE_MESSAGE);
        match error {
            AgentError::AutomaticDispatch { causes, .. } => assert_eq!(causes, ["overflow"]),
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
