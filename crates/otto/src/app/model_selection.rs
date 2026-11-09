//! Model listing and switching state, and thinking level.

use super::*;

impl Controller {
    /// The configured model selectors, sorted: every model table plus
    /// `default_model`.
    pub fn models(&self) -> Vec<String> {
        self.model_summaries()
            .into_iter()
            .map(|summary| summary.selector)
            .collect()
    }

    pub fn model_summaries(&self) -> Vec<ModelSummary> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let current = self.lock().current.as_ref().map(|current| {
            (
                current.info.model_selector.clone(),
                current.info.thinking.clone(),
            )
        });
        let config = &self.builder.config;
        let mut selectors: Vec<otto_core::config::ModelSelector> = Vec::new();
        for (provider_name, provider) in &config.providers {
            for model_id in provider.models.keys() {
                selectors.push(otto_core::config::ModelSelector {
                    provider: provider_name.clone(),
                    model_id: model_id.clone(),
                });
            }
        }
        if let Ok(default) = otto_core::config::ModelSelector::parse(&config.default_model) {
            selectors.push(default);
        }
        let mut rows: Vec<ModelSummary> = selectors
            .into_iter()
            .filter_map(|selector| {
                let provider = config.provider_entry(&selector.provider)?;
                let model = config.model_entry(&selector)?;
                let text = selector.to_string();
                let thinking = current
                    .as_ref()
                    .filter(|(current_selector, _)| *current_selector == text)
                    .map(|(_, thinking)| thinking.clone())
                    .unwrap_or(model.thinking);
                Some(ModelSummary {
                    provider_type: provider.provider_type(&selector.provider).to_string(),
                    provider: selector.provider,
                    model_id: selector.model_id,
                    selector: text,
                    thinking,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.selector.cmp(&b.selector));
        rows.dedup_by(|a, b| a.selector == b.selector);
        rows
    }

    pub fn set_default_model(&self, model: &str) -> Result<(), String> {
        if self.lock().is_closed() {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(MODEL_SWITCH_UNAVAILABLE.to_string());
        }
        crate::config::set_default_model_file(&self.builder.config_path, model)
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    pub fn model_effective_thinking(&self, model: &str) -> String {
        if !self.dynamic_content_available() {
            return String::new();
        }
        self.builder
            .resolve_model(model)
            .map(|runtime| runtime.thinking)
            .unwrap_or_default()
    }

    pub async fn set_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(MODEL_SWITCH_UNAVAILABLE.to_string());
        }
        let (session, runtime) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            let metadata = metadata_of(&current.info);
            let mut runtime = self.replacement_runtime(&metadata)?;
            runtime.thinking = normalize_thinking_arg(thinking);
            (current.session.clone(), runtime)
        };
        let runner = Arc::new(self.builder.build_runner_quiet(&session, &runtime).await?);
        session
            .update_thinking_level(&runtime.thinking)
            .map_err(|error| self.builder.redact_error(&error, Some(&runtime)))?;
        {
            let mut state = self.lock();
            let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
            current.info = self.builder.runtime_info(&runtime);
            let old = std::mem::replace(&mut current.runner, runner);
            old.close();
        }
        drop(admission);
        Ok(())
    }

    pub fn save_model_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        if self.lock().is_closed() {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(MODEL_SWITCH_UNAVAILABLE.to_string());
        }
        let selector = self.info().model_selector;
        if selector.is_empty() {
            return Err("current model is empty".to_string());
        }
        crate::config::set_model_thinking_file(
            &self.builder.config_path,
            &selector,
            &normalize_thinking_arg(thinking),
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    // ---- session replacement ----
}
