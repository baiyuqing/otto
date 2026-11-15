//! Selection of the model a run uses: CLI, environment, resumed session,
//! and `default_model`, in that order.

use std::collections::HashMap;

use super::{Overrides, SessionDefaults, env_value};
use crate::config::model_selector::ModelSelector;
use crate::config::providers::is_builtin_provider;
use crate::config::{ConfigError, File};

/// `flag` is the option text that precedes a value, such as `--model ` or `OTTO_MODEL=`.
fn selector_from(provider: &str, model: &str, flag: &str) -> Result<ModelSelector, ConfigError> {
    if !provider.is_empty() {
        return ModelSelector::from_parts(provider, model);
    }
    if !model.contains('/') {
        return Err(ConfigError::new(format!(
            "model \"{model}\" has no provider: use {flag}<provider>/{model}"
        )));
    }
    ModelSelector::parse(model)
}

/// The provider a run of a session recorded before selectors existed resumes
/// on: the provider of `default_model` when it has the session's type,
/// otherwise the built-in provider of that type.
fn provider_for_old_session(file: &File, provider_type: &str) -> String {
    if let Ok(default) = ModelSelector::parse(&file.default_model)
        && file
            .provider_entry(&default.provider)
            .is_some_and(|entry| entry.provider_type(&default.provider) == provider_type)
    {
        return default.provider;
    }
    provider_type.to_string()
}

pub(super) fn select_model(
    file: &File,
    env: &HashMap<String, String>,
    session: &SessionDefaults,
    overrides: &Overrides,
) -> Result<ModelSelector, ConfigError> {
    if !overrides.model.is_empty() {
        return selector_from(&overrides.provider, &overrides.model, "--model ");
    }
    if !overrides.provider.is_empty() {
        return Err(ConfigError::new(
            "--provider requires --model: use --model <provider>/<model_id>",
        ));
    }
    let env_model = env_value(env, "OTTO_MODEL");
    let env_provider = env_value(env, "OTTO_PROVIDER");
    if !env_model.is_empty() {
        return selector_from(&env_provider, &env_model, "OTTO_MODEL=");
    }
    if !env_provider.is_empty() {
        return Err(ConfigError::new(
            "OTTO_PROVIDER requires OTTO_MODEL: use OTTO_MODEL=<provider>/<model_id>",
        ));
    }
    if let Ok(selector) = ModelSelector::parse(&session.model_selector) {
        return Ok(selector);
    }
    if !session.provider.is_empty() && !session.model.is_empty() {
        return Ok(ModelSelector {
            provider: provider_for_old_session(file, &session.provider),
            model_id: session.model.clone(),
        });
    }
    if !file.default_model.is_empty() {
        return ModelSelector::parse(&file.default_model)
            .map_err(|error| ConfigError::new(format!("default_model: {error}")));
    }
    Err(ConfigError::new(
        "missing model: set default_model or pass --model <provider>/<model_id>",
    ))
}

/// Fails when the selector names neither a configured nor a built-in provider.
pub(super) fn known_provider(file: &File, selector: &ModelSelector) -> Result<(), ConfigError> {
    if file.providers.contains_key(&selector.provider) || is_builtin_provider(&selector.provider) {
        return Ok(());
    }
    Err(ConfigError::new(format!(
        "unknown provider \"{}\" in model selector \"{selector}\"",
        selector.provider
    )))
}
