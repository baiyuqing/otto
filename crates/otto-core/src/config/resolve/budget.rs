//! Resolution of the `[agent.budget]` per-turn limits.

use crate::agent::budget::{BudgetPolicy, DEFAULT_MAX_STEPS, DEFAULT_MAX_TOKENS, Limit};

use crate::config::{BudgetConfig, ConfigError};

/// Upper bound of `max_tokens`: 10^12 tokens. It keeps a mistyped value from
/// reading as a limit while leaving room above any realistic turn (the default
/// is 4.2 x 10^6).
pub const MAX_TOKENS_LIMIT: i64 = 1_000_000_000_000;
/// Upper bound of `max_steps`: one million provider calls in one turn.
pub const MAX_STEPS_LIMIT: i64 = 1_000_000;

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
