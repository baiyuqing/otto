//! Resolution of compaction targets and the model window limits.

use super::{
    CompactionConfig, CompactionRuntime, ConfigError, DEFAULT_COMPACTION_KEEP,
    DEFAULT_COMPACTION_RESERVE, MINIMUM_COMPACTION_TARGET, MINIMUM_COMPACTION_WINDOW,
    resolve_model_limits,
};
use crate::config::providers::ModelEntry;

pub(super) fn validate_thinking(thinking: &str) -> Result<(), ConfigError> {
    if matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        return Ok(());
    }
    Err(ConfigError::new(
        "invalid thinking: must be one of low, medium, high, xhigh, max",
    ))
}

pub(super) fn resolve_compaction(
    config: &CompactionConfig,
    model_entry: &ModelEntry,
    model: &str,
) -> Result<CompactionRuntime, ConfigError> {
    let auto = config.auto.unwrap_or(true);

    let reserve = match config.reserve_tokens {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid reserve_tokens: must be greater than zero",
            ));
        }
        Some(value) => value,
        None => DEFAULT_COMPACTION_RESERVE,
    };

    let keep = match config.keep_recent_tokens {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid keep_recent_tokens: must be greater than zero",
            ));
        }
        Some(value) => value,
        None => DEFAULT_COMPACTION_KEEP,
    };

    if let Some(context_window) = model_entry.context_window
        && context_window < MINIMUM_COMPACTION_WINDOW
    {
        return Err(ConfigError::new(format!(
            "invalid context_window: must be at least {MINIMUM_COMPACTION_WINDOW}"
        )));
    }
    if let Some(compaction_window) = model_entry.compaction_window {
        let Some(context_window) = model_entry.context_window else {
            return Err(ConfigError::new(
                "invalid context_window: required with compaction_window",
            ));
        };
        if compaction_window < MINIMUM_COMPACTION_WINDOW {
            return Err(ConfigError::new(format!(
                "invalid compaction_window: must be at least {MINIMUM_COMPACTION_WINDOW}"
            )));
        }
        if compaction_window > context_window {
            return Err(ConfigError::new(
                "invalid compaction_window: must not exceed context_window",
            ));
        }
    }

    let limits = resolve_model_limits(model);
    let mut runtime = CompactionRuntime {
        auto,
        context_window: limits.context_window,
        hard_input_window: limits.hard_input_window,
        working_window: limits.working_window,
        max_output_tokens: limits.max_output_tokens,
        reserve_tokens: reserve,
        keep_recent_tokens: keep,
    };
    if let Some(context_window) = model_entry.context_window {
        runtime.context_window = context_window;
        runtime.hard_input_window = context_window;
        runtime.working_window = context_window;
    }
    if let Some(compaction_window) = model_entry.compaction_window {
        runtime.working_window = compaction_window;
    }

    if runtime.working_window > 0 {
        let reserve_cap = MINIMUM_COMPACTION_TARGET.max(runtime.working_window / 4);
        runtime.reserve_tokens = runtime.reserve_tokens.min(reserve_cap);
        let keep_cap =
            MINIMUM_COMPACTION_TARGET.max((runtime.working_window - runtime.reserve_tokens) / 2);
        runtime.keep_recent_tokens = runtime.keep_recent_tokens.min(keep_cap);
    }
    Ok(runtime)
}
