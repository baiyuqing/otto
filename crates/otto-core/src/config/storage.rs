//! Storage routing preferences. Resolved connection descriptors stay native-only.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ConfigError;

pub const DOMAINS: &[&str] = &[
    "approvals",
    "workflows",
    "schedule",
    "usage",
    "tasks",
    "reflection",
    "skill_checks",
    "memory_advice",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    #[serde(default)]
    pub backends: BTreeMap<String, Backend>,
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub backend: String,
    pub path: Option<String>,
    pub endpoint_env: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl Storage {
    pub fn is_default(&self) -> bool {
        self.backends.is_empty() && self.routes.is_empty()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, backend) in &self.backends {
            if !identifier(name)
                || !identifier(&backend.backend)
                || backend.path.is_some() == backend.endpoint_env.is_some()
                || backend
                    .path
                    .as_ref()
                    .is_some_and(|p| p.trim().is_empty() || p.chars().any(char::is_control))
                || backend
                    .endpoint_env
                    .as_ref()
                    .is_some_and(|name| !environment_name(name))
            {
                return Err(ConfigError::new("invalid storage backend configuration"));
            }
        }
        for (domain, instance) in &self.routes {
            if domain == "memory" {
                return Err(ConfigError::new(
                    "storage.routes.memory is not supported; memory is stored as files under ~/.otto/memory",
                ));
            }
            if !DOMAINS.contains(&domain.as_str()) || !self.backends.contains_key(instance) {
                return Err(ConfigError::new("invalid storage domain route"));
            }
        }
        Ok(())
    }

    pub fn credential_variables(&self) -> impl Iterator<Item = &str> {
        self.backends
            .values()
            .filter_map(|backend| backend.endpoint_env.as_deref())
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(crate) fn environment_name(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {

    #[test]
    fn validates_named_routes_without_accepting_inline_service_credentials() {
        let file = super::super::parse("[storage.backends.primary]\nbackend='postgres'\nendpoint_env='DB_DSN'\n[storage.routes]\napprovals='primary'\n").unwrap();
        assert!(file.storage.validate().is_ok());
        for text in [
            "[storage.backends.p]\nbackend='mysql'\npath='x'\nendpoint_env='DB_DSN'",
            "[storage.backends.p]\nbackend='mysql'\nendpoint_env='BAD-NAME'",
            "[storage.routes]\nsessions='primary'",
            "[storage.routes]\napprovals='missing'",
        ] {
            assert!(
                super::super::parse(text)
                    .unwrap()
                    .storage
                    .validate()
                    .is_err()
            );
        }
        assert!(
            super::super::parse("[storage.backends.p]\nbackend='mysql'\ndsn='inline'\n").is_err()
        );
    }

    #[test]
    fn memory_route_is_rejected_and_names_the_key() {
        let file = super::super::parse(
            "[storage.backends.p]\nbackend='sqlite'\npath='x'\n[storage.routes]\nmemory='p'\n",
        )
        .unwrap();
        let error = file.storage.validate().unwrap_err();
        assert!(error.to_string().contains("storage.routes.memory"));
    }
}
