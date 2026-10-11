//! Automatic compaction triggers and the turn-scoped dispatch state.
//!
//! The agent compacts on its own in three situations: before a request whose
//! estimate crosses the soft trigger, before a request that follows a long idle
//! gap with a context above half the soft trigger, and after a request the
//! provider rejected for context overflow. Each is attempted at most once per
//! turn; the first two share one attempt.
//!
//! Ownership: [`RunDispatchState`] lives for one `Agent::run` call and is
//! dropped with it, so the tool-result overlay never outlives the turn, and the
//! per-turn [`ProgressGuard`] starts every turn with no history.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};

use crate::model::zero_time;
use crate::provider::ProviderError;

use super::progress::ProgressGuard;
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

/// Idle time after which the provider prompt cache is treated as expired.
/// ChatGPT backend sessions (2026-09-01..2026-10-08): requests after 10-20
/// minutes hit the cache as often as after 5-10 minutes (51%); after more
/// than 90 minutes, 0 of 31 hit.
pub const IDLE_CACHE_EXPIRY: TimeDelta = TimeDelta::minutes(60);

/// Whether the last model response is old enough that the prompt cache has
/// likely expired. Unset timestamps (`zero_time`, from old sessions) and a
/// clock earlier than the response report false.
pub fn idle_cache_expired(last_response: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_response.is_some_and(|last| last != zero_time() && now - last >= IDLE_CACHE_EXPIRY)
}

/// What one `Agent::run` call carries across its provider steps.
#[derive(Debug, Default)]
pub struct RunDispatchState {
    /// Whether the proactive compaction attempt has been spent this turn.
    pub proactive_attempted: bool,
    /// The rendered memory block, prepended to every request of this turn.
    pub memory_context: String,
    /// The ids of the records `memory_context` renders.
    pub memory_record_ids: Vec<String>,
    /// The full redacted tool-result text, keyed by tool-call id, for results
    /// whose stored text is a placeholder. It is substituted into provider
    /// requests only, never into the session.
    pub tool_result_overlay: HashMap<String, String>,
    /// The limits that stop a turn making no progress. The turn loop replaces
    /// the default with the one this run's [`super::Options`] configured.
    pub progress: ProgressGuard,
    pub yield_after_batch: bool,
    pub batch_has_errors: bool,
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

    #[test]
    fn idle_cache_expires_at_sixty_minutes() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).expect("in range");
        let ago = |minutes| Some(now - TimeDelta::minutes(minutes));
        assert!(!idle_cache_expired(ago(59), now));
        assert!(idle_cache_expired(ago(60), now));
        assert!(idle_cache_expired(ago(61), now));
        assert!(!idle_cache_expired(None, now));
        assert!(!idle_cache_expired(Some(zero_time()), now));
        assert!(!idle_cache_expired(ago(-5), now));
    }
}
