//! Provider and model rules of the layered merge: the project-scope filter,
//! the provider type-change rule and the managed-policy checks.

use std::collections::BTreeMap;

use toml::{Table, Value};

use super::{Diagnostic, ManagedPolicy, Origin};
use crate::config::providers::{
    CLAUDE_DEFAULT_API_KEY_ENV, CLAUDE_DEFAULT_BASE_URL, is_builtin_provider,
};
use crate::config::resolve::Runtime;
use crate::config::{ConfigError, DEFAULT_CLAUDE_API_VERSION, PROVIDER_CLAUDE};

const PROJECT_MODEL_FIELDS: [&str; 3] = ["thinking", "context_window", "compaction_window"];
const CONNECTION_FIELDS: [&str; 6] = [
    "base_url",
    "api_key_env",
    "api_version",
    "workspace_id_env",
    "trusted_origins",
    "allow_http_origins",
];

fn model_key(provider: &str, model_id: &str) -> String {
    if model_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        format!("providers.{provider}.models.{model_id}")
    } else {
        format!("providers.{provider}.models.\"{model_id}\"")
    }
}

/// Reduces the `providers` table of a project source to the model fields a
/// project may set, for built-in providers and providers that `existing`
/// (the lower layers) already defines. The caller re-inserts the returned
/// table after the generic project filter, which does not know provider keys.
pub(super) fn filter_project_providers(
    table: &mut Table,
    existing: &Table,
    source: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Table> {
    let Some(Value::Table(providers)) = table.remove("providers") else {
        return None;
    };
    let defined_below = existing.get("providers").and_then(Value::as_table);
    let mut kept = Table::new();
    let mut reject = |key: String, reason: &str| {
        diagnostics.push(Diagnostic {
            key,
            source: source.into(),
            reason: reason.into(),
        });
    };
    for (name, value) in providers {
        if !is_builtin_provider(&name)
            && !defined_below.is_some_and(|below| below.contains_key(&name))
        {
            reject(
                format!("providers.{name}"),
                "project configuration cannot define a provider",
            );
            continue;
        }
        let Value::Table(fields) = value else {
            continue;
        };
        let mut kept_provider = Table::new();
        for (field, field_value) in fields {
            let (Value::Table(models), "models") = (&field_value, field.as_str()) else {
                reject(
                    format!("providers.{name}.{field}"),
                    "field is restricted to user or managed configuration",
                );
                continue;
            };
            let mut kept_models = Table::new();
            for (model_id, model_value) in models {
                let Value::Table(model_fields) = model_value else {
                    continue;
                };
                let mut kept_model = Table::new();
                for (model_field, v) in model_fields {
                    if PROJECT_MODEL_FIELDS.contains(&model_field.as_str()) {
                        kept_model.insert(model_field.clone(), v.clone());
                    } else {
                        reject(
                            format!("{}.{model_field}", model_key(&name, model_id)),
                            "field is restricted to user or managed configuration",
                        );
                    }
                }
                kept_models.insert(model_id.clone(), Value::Table(kept_model));
            }
            kept_provider.insert("models".into(), Value::Table(kept_models));
        }
        kept.insert(name, Value::Table(kept_provider));
    }
    (!kept.is_empty()).then_some(kept)
}

fn effective_type(name: &str, provider: &Table) -> Option<String> {
    provider
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| is_builtin_provider(name).then(|| name.to_string()))
}

/// When `incoming` sets a provider `type` different from the one already
/// merged, drops the merged connection fields (and records their origins as
/// overridden) while keeping the merged model tables.
pub(super) fn drop_connection_on_type_change(
    values: &mut Table,
    origins: &mut BTreeMap<String, Origin>,
    overridden: &mut BTreeMap<String, Vec<Origin>>,
    incoming: &Table,
) {
    let Some(incoming_providers) = incoming.get("providers").and_then(Value::as_table) else {
        return;
    };
    let Some(existing_providers) = values.get_mut("providers").and_then(Value::as_table_mut) else {
        return;
    };
    for (name, new_provider) in incoming_providers {
        let Some(new_type) = new_provider
            .as_table()
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(old_provider) = existing_providers
            .get_mut(name)
            .and_then(Value::as_table_mut)
        else {
            continue;
        };
        if effective_type(name, old_provider).is_none_or(|old| old == new_type) {
            continue;
        }
        for field in CONNECTION_FIELDS {
            old_provider.remove(field);
            let full = format!("providers.{name}.{field}");
            if let Some(previous) = origins.remove(&full) {
                overridden.entry(full).or_default().push(previous);
            }
        }
    }
}

impl ManagedPolicy {
    /// Rejects `force` keys of the removed profile format so a forced value is never dropped.
    pub(super) fn reject_legacy_force_keys(&self) -> Result<(), ConfigError> {
        for key in ["default_profile", "profiles"] {
            if self.force.contains_key(key) {
                return Err(ConfigError::new(format!(
                    "managed policy force.{key} is no longer supported; use force.default_model, \
                     force.providers.<name>.<field> or force.providers.<name>.models.\"<id>\".thinking"
                )));
            }
        }
        Ok(())
    }

    /// Checks the resolved model and provider against the forced `default_model`,
    /// `providers.<name>.<field>` and `providers.<name>.models."<id>".thinking`.
    pub(super) fn model_runtime(&self, runtime: &Runtime) -> Result<(), ConfigError> {
        if self
            .force
            .get("default_model")
            .and_then(Value::as_str)
            .is_some_and(|forced| forced != runtime.model_selector.to_string())
        {
            return Err(ConfigError::new("model is forced by managed policy"));
        }
        let Some(forced) = self
            .force
            .get("providers")
            .and_then(Value::as_table)
            .and_then(|providers| providers.get(&runtime.provider_name))
            .and_then(Value::as_table)
        else {
            return Ok(());
        };
        for (key, actual) in [
            ("type", &runtime.provider_type),
            ("base_url", &runtime.base_url),
            ("api_key_env", &runtime.api_key_env),
            ("api_version", &runtime.api_version),
            ("workspace_id_env", &runtime.workspace_id_env),
        ] {
            let Some(value) = forced.get(key).and_then(Value::as_str) else {
                continue;
            };
            let value = if runtime.provider_type == PROVIDER_CLAUDE && value.is_empty() {
                match key {
                    "base_url" => CLAUDE_DEFAULT_BASE_URL,
                    "api_key_env" => CLAUDE_DEFAULT_API_KEY_ENV,
                    "api_version" => DEFAULT_CLAUDE_API_VERSION,
                    _ => value,
                }
            } else {
                value
            };
            let differs = if key == "base_url" {
                crate::config::resolve::normalize_base_url(value)
                    .is_ok_and(|normalized| normalized != *actual)
            } else {
                value != actual
            };
            if differs {
                return Err(ConfigError::new(format!(
                    "provider field is forced by managed policy: {key}"
                )));
            }
        }
        if forced
            .get("models")
            .and_then(Value::as_table)
            .and_then(|models| models.get(&runtime.model))
            .and_then(Value::as_table)
            .and_then(|model| model.get("thinking"))
            .and_then(Value::as_str)
            .is_some_and(|thinking| thinking != runtime.thinking)
        {
            return Err(ConfigError::new(
                "model field is forced by managed policy: thinking",
            ));
        }
        Ok(())
    }
}
