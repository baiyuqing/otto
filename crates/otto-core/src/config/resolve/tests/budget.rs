//! Resolve tests: the `[agent.budget]` per-turn limits.

use super::*;
use crate::agent::budget::{BudgetPolicy, Limit};

fn resolve_with(file: &File) -> Result<Runtime, ConfigError> {
    resolve(
        file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            model: selector_of(file, "sub"),
            ..Default::default()
        },
    )
}

fn base_file() -> File {
    file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))])
}

#[test]
fn the_defaults_are_4_200_000_tokens_54_steps_and_40_minutes() {
    let runtime = resolve_with(&base_file()).unwrap();
    assert_eq!(runtime.budget, BudgetPolicy::default());
    assert_eq!(runtime.budget.max_tokens, Limit::from_count(4_200_000));
    assert_eq!(runtime.budget.max_steps, Limit::from_count(54));
    assert_eq!(runtime.budget.max_duration, Limit::from_count(2_400));
}

#[test]
fn explicit_values_replace_the_defaults_and_zero_is_unlimited() {
    let mut file = base_file();
    file.agent.budget.max_tokens = Some(1000);
    file.agent.budget.max_steps = Some(0);
    let runtime = resolve_with(&file).unwrap();
    assert_eq!(runtime.budget.max_tokens, Limit::from_count(1000));
    assert_eq!(runtime.budget.max_steps, Limit::Unlimited);

    file.agent.budget.max_tokens = Some(0);
    file.agent.budget.max_steps = Some(7);
    let runtime = resolve_with(&file).unwrap();
    assert_eq!(runtime.budget.max_tokens, Limit::Unlimited);
    assert_eq!(runtime.budget.max_steps, Limit::from_count(7));
}

#[test]
fn a_toml_table_sets_both_keys() {
    let file = crate::config::parse("[agent.budget]\nmax_tokens = 0\nmax_steps = 12\n").unwrap();
    assert_eq!(file.agent.budget.max_tokens, Some(0));
    assert_eq!(file.agent.budget.max_steps, Some(12));
}

#[test]
fn negative_and_oversized_values_are_rejected_with_the_key_name() {
    for (field, value, maximum) in [
        ("max_tokens", -1_i64, 1_000_000_000_000_i64),
        ("max_tokens", 1_000_000_000_001, 1_000_000_000_000),
        ("max_steps", -5, 1_000_000),
        ("max_steps", 1_000_001, 1_000_000),
    ] {
        let mut file = base_file();
        match field {
            "max_tokens" => file.agent.budget.max_tokens = Some(value),
            _ => file.agent.budget.max_steps = Some(value),
        }
        let error = resolve_with(&file).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("[agent.budget].{field} must be between 0 and {maximum}, got {value}")
        );
    }
}

#[test]
fn an_unknown_key_in_the_table_is_rejected_and_named() {
    let error = crate::config::parse("[agent.budget]\nmax_turns = 3\n").unwrap_err();
    let text = error.to_string();
    assert!(text.contains("unknown field"), "{text}");
    assert!(text.contains("max_turns"), "{text}");
}

fn duration_of(text: &str) -> Result<Limit, ConfigError> {
    let mut file = base_file();
    file.agent.budget.max_duration = Some(text.to_string());
    resolve_with(&file).map(|runtime| runtime.budget.max_duration)
}

#[test]
fn max_duration_is_a_go_duration_and_zero_is_unlimited() {
    assert_eq!(duration_of("40m"), Ok(Limit::from_count(2_400)));
    assert_eq!(duration_of("1h30m"), Ok(Limit::from_count(5_400)));
    assert_eq!(duration_of("90s"), Ok(Limit::from_count(90)));
    assert_eq!(duration_of("0"), Ok(Limit::Unlimited));
    assert_eq!(duration_of("0s"), Ok(Limit::Unlimited));
    // A positive duration below one second rounds up to one second.
    assert_eq!(duration_of("500ms"), Ok(Limit::from_count(1)));
    assert_eq!(duration_of("168h"), Ok(Limit::from_count(604_800)));
}

#[test]
fn a_toml_table_sets_max_duration() {
    let file = crate::config::parse("[agent.budget]\nmax_duration = \"15m\"\n").unwrap();
    assert_eq!(file.agent.budget.max_duration.as_deref(), Some("15m"));
}

#[test]
fn an_invalid_negative_or_oversized_max_duration_is_rejected_with_the_key_name() {
    for value in ["soon", "", "40", "-5m", "169h", "99999999h"] {
        let error = duration_of(value).unwrap_err().to_string();
        assert!(
            error.starts_with("invalid [agent.budget].max_duration: "),
            "{value:?}: {error}"
        );
    }
    assert_eq!(
        duration_of("-5m").unwrap_err().to_string(),
        "invalid [agent.budget].max_duration: must not be negative"
    );
    assert_eq!(
        duration_of("169h").unwrap_err().to_string(),
        "invalid [agent.budget].max_duration: must be at most 604800 seconds (7 days), got \"169h\""
    );
}
