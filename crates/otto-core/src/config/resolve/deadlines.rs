//! Resolution of the cooperative deadlines and no-progress limits.

use std::time::Duration;

use super::{
    ConfigError, DEFAULT_CANCELLATION_GRACE, DEFAULT_PROGRESS_CONSECUTIVE_FAILURES,
    DEFAULT_PROGRESS_REPEATED_CALLS, DEFAULT_UNATTENDED_TURN_TIMEOUT, DeadlineRuntime,
    MAX_PROGRESS_LIMIT, ProgressRuntime, ResilienceRuntime, parse_go_duration,
};

pub(super) fn resolve_resilience(
    agent: &crate::config::Agent,
) -> Result<ResilienceRuntime, ConfigError> {
    let deadlines = DeadlineRuntime {
        turn_timeout: optional_positive_duration("turn_timeout", &agent.turn_timeout)?,
        unattended_turn_timeout: unattended_turn_timeout(&agent.unattended_turn_timeout)?,
        provider_timeout: optional_positive_duration("provider_timeout", &agent.provider_timeout)?,
        cancellation_grace: optional_positive_duration(
            "cancellation_grace",
            &agent.cancellation_grace,
        )?
        .unwrap_or(DEFAULT_CANCELLATION_GRACE),
        subagent_timeout: optional_positive_duration("subagent_timeout", &agent.subagent_timeout)?,
        workflow_step_timeout: optional_positive_duration(
            "workflow_step_timeout",
            &agent.workflow_step_timeout,
        )?,
    };

    Ok(ResilienceRuntime { deadlines })
}

pub(super) fn resolve_progress(
    agent: &crate::config::Agent,
) -> Result<ProgressRuntime, ConfigError> {
    Ok(ProgressRuntime {
        repeated_calls: optional_progress_limit(
            "no_progress_repeats",
            agent.no_progress_repeats,
            DEFAULT_PROGRESS_REPEATED_CALLS,
        )?,
        consecutive_failures: optional_progress_limit(
            "no_progress_failures",
            agent.no_progress_failures,
            DEFAULT_PROGRESS_CONSECUTIVE_FAILURES,
        )?,
    })
}

/// An absent limit keeps the built-in default; an explicit zero disables the
/// condition.
fn optional_progress_limit(
    field: &str,
    configured: Option<i64>,
    default: u32,
) -> Result<u32, ConfigError> {
    let Some(value) = configured else {
        return Ok(default);
    };
    if !(0..=i64::from(MAX_PROGRESS_LIMIT)).contains(&value) {
        return Err(ConfigError::new(format!(
            "[agent].{field} must be between 0 and {MAX_PROGRESS_LIMIT}, got {value}"
        )));
    }
    Ok(value as u32)
}

fn optional_positive_duration(
    field: &str,
    configured: &Option<String>,
) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = configured else {
        return Ok(None);
    };
    let nanos = parse_go_duration(value)
        .map_err(|err| ConfigError::new(format!("invalid {field}: {err}")))?;
    if nanos <= 0 {
        return Err(ConfigError::new(format!(
            "invalid {field}: must be greater than zero"
        )));
    }
    Ok(Some(Duration::from_nanos(nanos as u64)))
}

/// Omitted gives the default; a zero duration disables the deadline.
fn unattended_turn_timeout(configured: &Option<String>) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = configured else {
        return Ok(Some(DEFAULT_UNATTENDED_TURN_TIMEOUT));
    };
    let nanos = parse_go_duration(value)
        .map_err(|err| ConfigError::new(format!("invalid unattended_turn_timeout: {err}")))?;
    if nanos < 0 {
        return Err(ConfigError::new(
            "invalid unattended_turn_timeout: must not be negative",
        ));
    }
    Ok((nanos > 0).then(|| Duration::from_nanos(nanos as u64)))
}
