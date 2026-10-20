//! Profile listing and switching state, and thinking level.

use super::*;

impl Controller {
    /// The configured profile names, sorted.
    pub fn profiles(&self) -> Vec<String> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let mut names: Vec<String> = self.builder.config.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn profile_summaries(&self) -> Vec<ProfileSummary> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let current = self
            .lock()
            .current
            .as_ref()
            .map(|current| (current.info.profile.clone(), current.info.thinking.clone()));
        let mut rows: Vec<ProfileSummary> = self
            .builder
            .config
            .profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                thinking: current
                    .as_ref()
                    .filter(|(current_name, _)| current_name == name)
                    .map(|(_, thinking)| thinking.clone())
                    .unwrap_or_else(|| profile.thinking.clone()),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    pub fn set_default_profile(&self, profile: &str) -> Result<(), String> {
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        crate::config::set_default_profile_file(&self.builder.config_path, profile)
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    pub fn profile_effective_thinking(&self, profile: &str) -> String {
        if !self.dynamic_content_available() {
            return String::new();
        }
        self.builder
            .resolve_profile(profile)
            .map(|runtime| runtime.thinking)
            .unwrap_or_default()
    }

    pub async fn set_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let (session, runtime) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            let metadata = RuntimeMetadata {
                profile: current.info.profile.clone(),
                provider: current.info.provider.clone(),
                model: current.info.model.clone(),
            };
            let mut runtime = self.replacement_runtime(&metadata)?;
            runtime.profile = current.info.profile.clone();
            runtime.provider = current.info.provider.clone();
            runtime.model = current.info.model.clone();
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
        self.bind_reflection();
        drop(admission);
        Ok(())
    }

    pub fn save_profile_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let profile = self.info().profile;
        if profile.is_empty() {
            return Err("current profile is empty".to_string());
        }
        crate::config::set_profile_thinking_file(
            &self.builder.config_path,
            &profile,
            &normalize_thinking_arg(thinking),
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    // ---- session replacement ----
}
