//! The `<provider>/<model_id>` selector that names one configured or built-in model.

use std::fmt;

use super::ConfigError;

/// A parsed model selector. `provider` is a provider name; `model_id` is the
/// ID the provider API receives and may itself contain `/`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelSelector {
    pub provider: String,
    pub model_id: String,
}

impl ModelSelector {
    /// Splits `text` at the first `/`. Both parts must be non-empty and the
    /// provider name must be valid (see [`validate_provider_name`]).
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let Some((provider, model_id)) = text.split_once('/') else {
            return Err(ConfigError::new(format!(
                "invalid model selector \"{text}\": expected <provider>/<model_id>"
            )));
        };
        if provider.is_empty() || model_id.is_empty() {
            return Err(ConfigError::new(format!(
                "invalid model selector \"{text}\": provider and model ID must not be empty"
            )));
        }
        validate_provider_name(provider).map_err(|reason| {
            ConfigError::new(format!("invalid model selector \"{text}\": {reason}"))
        })?;
        Ok(Self {
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    /// Builds a selector from a separate provider name and model ID, as the
    /// deprecated `--provider` plus `--model` pair supplies them.
    pub fn from_parts(provider: &str, model_id: &str) -> Result<Self, ConfigError> {
        if model_id.is_empty() {
            return Err(ConfigError::new("model ID must not be empty"));
        }
        validate_provider_name(provider).map_err(ConfigError::new)?;
        Ok(Self {
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }
}

impl fmt::Display for ModelSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model_id)
    }
}

/// A provider name uses ASCII letters, digits, `_` and `-`, so it never
/// contains `/`.
pub fn validate_provider_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("provider name must not be empty".into());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "provider name \"{name}\" may only contain ASCII letters, digits, '_' and '-'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_the_first_slash_and_keeps_slashes_in_the_model_id() {
        let selector = ModelSelector::parse("openrouter/anthropic/claude-x").unwrap();
        assert_eq!(selector.provider, "openrouter");
        assert_eq!(selector.model_id, "anthropic/claude-x");
        assert_eq!(selector.to_string(), "openrouter/anthropic/claude-x");
    }

    #[test]
    fn rejects_malformed_selectors() {
        for text in ["", "gpt-5", "/gpt-5", "chatgpt/", "bad name/x", "a.b/x"] {
            let error = ModelSelector::parse(text).unwrap_err().to_string();
            assert!(error.contains("invalid model selector"), "{text}: {error}");
        }
        assert!(
            ModelSelector::parse("gpt-5")
                .unwrap_err()
                .to_string()
                .contains("<provider>/<model_id>")
        );
    }

    #[test]
    fn from_parts_validates_the_provider_name() {
        assert_eq!(
            ModelSelector::from_parts("chatgpt", "a/b")
                .unwrap()
                .to_string(),
            "chatgpt/a/b"
        );
        assert!(ModelSelector::from_parts("a/b", "x").is_err());
        assert!(ModelSelector::from_parts("chatgpt", "").is_err());
    }
}
