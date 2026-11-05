//! Model-less infrastructure defaults for the HTTP configuration bootstrap.

use super::*;

/// Resolve normally when a model is selected. Only an absent selection permits
/// a model-less runtime for server infrastructure, never for an agent runner.
/// Provider/credential errors and invalid selections are not suppressed.
pub fn resolve_server_bootstrap(
    file: &File,
    env: &HashMap<String, String>,
    overrides: &Overrides,
) -> Result<Runtime, ConfigError> {
    match select_model(file, env, &SessionDefaults::default(), overrides) {
        Ok(_) => return resolve(file, env, &SessionDefaults::default(), overrides),
        Err(error) if error.to_string().starts_with("missing model:") => {}
        Err(error) => return Err(error),
    }
    file.approval.validate()?;
    let mut shell_timeout = DEFAULT_SHELL_TIMEOUT;
    if !file.agent.shell_timeout.is_empty() {
        let nanos = parse_go_duration(&file.agent.shell_timeout)
            .map_err(|error| ConfigError::new(format!("invalid shell_timeout: {error}")))?;
        if nanos <= 0 {
            return Err(ConfigError::new(
                "invalid shell_timeout: must be greater than zero",
            ));
        }
        shell_timeout = Duration::from_nanos(nanos as u64);
    }
    if overrides.shell_timeout > Duration::ZERO {
        shell_timeout = overrides.shell_timeout;
    }
    let mut max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;
    if file.agent.max_output_bytes > 0 {
        max_output_bytes = file.agent.max_output_bytes;
    }
    if overrides.max_output_bytes > 0 {
        max_output_bytes = overrides.max_output_bytes;
    }
    Ok(Runtime {
        shell_timeout,
        max_output_bytes,
        compaction: resolve_compaction(&file.agent.compaction, &Default::default(), "")?,
        resilience: resolve_resilience(&file.agent)?,
        progress: resolve_progress(&file.agent)?,
        ..Runtime::default()
    })
}
