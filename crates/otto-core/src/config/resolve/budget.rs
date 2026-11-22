//! Resolution of the `[agent.budget]` per-turn limits.

use crate::agent::budget::{
    BudgetPolicy, DEFAULT_MAX_DURATION_SECS, DEFAULT_MAX_STEPS, DEFAULT_MAX_TOKENS, Limit,
};

use crate::config::duration::parse_go_duration;
use crate::config::{BudgetConfig, ConfigError};

/// Upper bound of `max_tokens`: 10^12 tokens. It keeps a mistyped value from
/// reading as a limit while leaving room above any realistic turn (the default
/// is 4.2 x 10^6).
pub const MAX_TOKENS_LIMIT: i64 = 1_000_000_000_000;
/// Upper bound of `max_steps`: one million provider calls in one turn.
pub const MAX_STEPS_LIMIT: i64 = 1_000_000;
/// Upper bound of `max_duration`: seven days of active time (604,800 s). A
/// longer value is more likely a mistyped unit (`"2400h"` for `"2400s"`) than
/// an intended limit, and the default is 40 minutes.
pub const MAX_DURATION_LIMIT_SECS: u64 = 7 * 24 * 60 * 60;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

pub(super) fn resolve_budget(config: &BudgetConfig) -> Result<BudgetPolicy, ConfigError> {
    Ok(BudgetPolicy {
        max_tokens: limit(
            "max_tokens",
            config.max_tokens,
            DEFAULT_MAX_TOKENS,
            MAX_TOKENS_LIMIT,
        )?,
        max_steps: limit(
            "max_steps",
            config.max_steps,
            DEFAULT_MAX_STEPS,
            MAX_STEPS_LIMIT,
        )?,
        max_duration: duration_limit(&config.max_duration)?,
    })
}

/// An absent key keeps the default; an explicit zero is unlimited.
fn limit(
    field: &str,
    configured: Option<i64>,
    default: u64,
    maximum: i64,
) -> Result<Limit, ConfigError> {
    let Some(value) = configured else {
        return Ok(Limit::from_count(default));
    };
    if !(0..=maximum).contains(&value) {
        return Err(ConfigError::new(format!(
            "[agent.budget].{field} must be between 0 and {maximum}, got {value}"
        )));
    }
    Ok(Limit::from_count(value as u64))
}

/// A Go-duration string. An absent key keeps the default of 40 minutes; a zero
/// duration is unlimited; a positive duration is rounded up to whole seconds.
fn duration_limit(configured: &Option<String>) -> Result<Limit, ConfigError> {
    let Some(text) = configured else {
        return Ok(Limit::from_count(DEFAULT_MAX_DURATION_SECS));
    };
    let nanos = parse_go_duration(text).map_err(|error| {
        ConfigError::new(format!("invalid [agent.budget].max_duration: {error}"))
    })?;
    if nanos < 0 {
        return Err(ConfigError::new(
            "invalid [agent.budget].max_duration: must not be negative",
        ));
    }
    let seconds = (nanos as u64).div_ceil(NANOS_PER_SECOND);
    if seconds > MAX_DURATION_LIMIT_SECS {
        return Err(ConfigError::new(format!(
            "invalid [agent.budget].max_duration: must be at most {MAX_DURATION_LIMIT_SECS} seconds (7 days), got {text:?}"
        )));
    }
    Ok(Limit::from_count(seconds))
}
