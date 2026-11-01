//! Native external-backend registration; no dialect or provider SDK types.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use otto_core::config::storage::Storage as Configuration;

use super::{Endpoint, RepositoryFactory};
use crate::storage::migration::StorageRoute;

/// Connection descriptors may contain credentials and are never serialized.
#[derive(Clone)]
pub enum StorageEndpoint {
    Local(std::path::PathBuf),
    Service(Arc<str>),
}

impl std::fmt::Debug for StorageEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Local(_) => "Local(<redacted>)",
            Self::Service(_) => "Service(<redacted>)",
        })
    }
}

impl StorageEndpoint {
    /// Expose only at the backend boundary; never interpolate into diagnostics.
    pub fn address(&self) -> Result<&str, String> {
        match self {
            Self::Local(path) => path
                .to_str()
                .ok_or_else(|| "storage path is not UTF-8".into()),
            Self::Service(descriptor) => Ok(descriptor),
        }
    }
}

/// Requirements describe domain behavior rather than a SQL dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryRequirements {
    pub domain: String,
    pub profile_version: u32,
}

impl RepositoryRequirements {
    pub fn for_domain(domain: &str) -> Result<Self, String> {
        if !otto_core::config::storage::DOMAINS.contains(&domain) {
            return Err("unsupported storage domain".into());
        }
        Ok(Self {
            domain: domain.into(),
            profile_version: 1,
        })
    }
}

/// Returned only after the factory validates the deployment and domain contract.
/// Stable across restarts; changing credentials must not change namespace identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceIdentity(pub String);

impl NamespaceIdentity {
    pub fn validate(&self) -> crate::storage::Result<()> {
        if self.0.is_empty()
            || self.0.len() > 256
            || !self
                .0
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
        {
            return Err(crate::storage::Error::IncompatibleSchema);
        }
        Ok(())
    }
}

pub struct MigrationContext {
    pub migration_id: String,
    pub source_identity: String,
    pub destination_identity: String,
    pub generation: String,
    pub fence: Arc<dyn crate::storage::migration::FreezeGuard>,
}

pub struct PreparedImport {
    pub namespace_identity: NamespaceIdentity,
    pub stage: Arc<dyn crate::storage::migration::StageImport>,
}

#[derive(Clone, Default)]
pub struct BackendRegistry {
    pub(super) factories: HashMap<String, Arc<dyn RepositoryFactory>>,
}

impl BackendRegistry {
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
    pub async fn configured_factory(
        &self,
        name: &str,
        endpoint: &StorageEndpoint,
        options: &BTreeMap<String, String>,
    ) -> Result<Arc<dyn RepositoryFactory>, String> {
        let factory = self.factory(name)?;
        let configured = factory
            .configured(endpoint, options)
            .await
            .map_err(|_| "storage backend options are unsupported or invalid")?;
        Ok(configured.unwrap_or(factory))
    }
    pub fn factory(&self, name: &str) -> Result<Arc<dyn RepositoryFactory>, String> {
        self.factories
            .get(name)
            .cloned()
            .ok_or_else(|| "storage backend is unavailable".into())
    }
    pub fn register(
        &mut self,
        name: String,
        factory: Arc<dyn RepositoryFactory>,
    ) -> Result<(), String> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || matches!(name.as_str(), "sqlite" | "posix")
            || self.factories.contains_key(&name)
        {
            return Err("storage backend name is invalid or already registered".into());
        }
        self.factories.insert(name, factory);
        Ok(())
    }
}

#[derive(Clone)]
pub struct ConfiguredEndpoint {
    pub factory: String,
    pub endpoint: StorageEndpoint,
    pub options: BTreeMap<String, String>,
}

pub fn resolve(
    configuration: &Configuration,
    environment: &HashMap<String, String>,
    _home: &Path,
) -> Result<BTreeMap<String, ConfiguredEndpoint>, String> {
    configuration
        .validate()
        .map_err(|_| "storage configuration is invalid")?;
    configuration
        .backends
        .iter()
        .map(|(name, backend)| {
            let endpoint = if let Some(path) = &backend.path {
                let path = Path::new(path);
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err("storage paths must be absolute without traversal".into());
                }
                StorageEndpoint::Local(path.to_owned())
            } else {
                let variable = backend
                    .endpoint_env
                    .as_ref()
                    .ok_or("storage endpoint is missing")?;
                let descriptor = environment
                    .get(variable)
                    .filter(|v| !v.trim().is_empty() && !v.chars().any(char::is_control))
                    .ok_or("storage endpoint environment variable is missing or invalid")?;
                StorageEndpoint::Service(Arc::from(descriptor.as_str()))
            };
            Ok((
                name.clone(),
                ConfiguredEndpoint {
                    factory: backend.backend.clone(),
                    endpoint,
                    options: backend.options.clone(),
                },
            ))
        })
        .collect()
}

pub fn configured_routes(configuration: &Configuration) -> BTreeMap<String, StorageRoute> {
    configuration
        .routes
        .iter()
        .map(|(domain, instance)| {
            (
                domain.clone(),
                StorageRoute {
                    backend: configuration.backends[instance].backend.clone(),
                    endpoint: format!("instance:{instance}"),
                    namespace_identity: None,
                },
            )
        })
        .collect()
}

pub(crate) fn endpoint<'a>(
    route: &'a StorageRoute,
    configured: &'a BTreeMap<String, ConfiguredEndpoint>,
) -> Result<Endpoint<'a>, String> {
    let address = if let Some(instance) = route.endpoint.strip_prefix("instance:") {
        let binding = configured
            .get(instance)
            .ok_or("storage backend instance is not configured")?;
        if binding.factory != route.backend {
            return Err("storage backend instance does not match the published route".into());
        }
        binding.endpoint.address()?
    } else if route.backend == "sqlite" || route.backend == "posix" {
        &route.endpoint
    } else {
        return Err("external storage routes require a named backend instance".into());
    };
    Ok(Endpoint {
        address,
        policy: crate::storage::OpenPolicy::Existing,
    })
}
