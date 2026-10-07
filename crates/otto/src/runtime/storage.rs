//! The composition boundary for storage backends. Services receive domain ports.
pub mod backend;
pub use backend::{BackendRegistry, NamespaceIdentity, RepositoryRequirements, StorageEndpoint};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use crate::approval::repository::ApprovalStore;
use crate::memory::advice::audit::AuditStore;
use crate::memory::repository::{MemoryOptions, MemoryStore};
use crate::reflection::repository::ReflectionStore;
use crate::schedule::ScheduleStore;
use crate::session::SessionRepository;
use crate::skill::check_repository::CheckStore;
use crate::storage::backends::posix::migration::{LocalCutover, RouteGroup, WriterPermit};
use crate::storage::migration::StorageRoute;
use crate::storage::{Connection, OpenOptions, OpenPolicy, SqlDriver, SqlRequirements};
use crate::subagent::record::TaskRepository;
use crate::usage::repository::UsageRepository;
use crate::workflow::WorkflowStore;

/// Namespace opening intent selected by routing, consumed by each backend.
#[derive(Clone, Copy)]
pub struct Endpoint<'a> {
    pub address: &'a str,
    pub policy: OpenPolicy,
}

impl std::fmt::Debug for Endpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &"<redacted>")
            .field("policy", &self.policy)
            .finish()
    }
}

/// A backend extension implements domain repositories, not application behavior.
/// Endpoint interpretation and dialect/schema compatibility belong to the factory.
#[async_trait::async_trait]
pub trait RepositoryFactory: Send + Sync {
    /// Bind immutable per-instance options. None reuses this factory only when
    /// no options are supplied; options must never be silently ignored.
    async fn configured(
        &self,
        _endpoint: &StorageEndpoint,
        options: &BTreeMap<String, String>,
    ) -> crate::storage::Result<Option<Arc<dyn RepositoryFactory>>> {
        if options.is_empty() {
            Ok(None)
        } else {
            Err(crate::storage::Error::UnsupportedCapability(
                "backend instance options",
            ))
        }
    }
    /// Read-only deployment/profile validation. Never create a missing namespace.
    /// Provider diagnostics must be mapped to the detail-free storage categories.
    async fn validate(
        &self,
        _endpoint: &StorageEndpoint,
        _requirements: &RepositoryRequirements,
        _options: &BTreeMap<String, String>,
    ) -> crate::storage::Result<NamespaceIdentity> {
        Err(crate::storage::Error::UnsupportedCapability(
            "external domain profile",
        ))
    }
    /// Explicit fresh namespace initialization; normal opening never invokes this.
    async fn initialize(
        &self,
        _endpoint: &StorageEndpoint,
        _domains: &[RepositoryRequirements],
        _options: &BTreeMap<String, String>,
    ) -> crate::storage::Result<NamespaceIdentity> {
        Err(crate::storage::Error::UnsupportedCapability(
            "namespace initialization",
        ))
    }
    /// Read-only preparation of an isolated import, including resumable identity.
    async fn prepare_import(
        &self,
        _endpoint: &StorageEndpoint,
        _requirements: &RepositoryRequirements,
        _options: &BTreeMap<String, String>,
        _context: &backend::MigrationContext,
    ) -> crate::storage::migration::Result<backend::PreparedImport> {
        Err(crate::storage::migration::Error::Invalid(
            "backend does not support logical import".into(),
        ))
    }
    async fn export(
        &self,
        _endpoint: &StorageEndpoint,
        _requirements: &RepositoryRequirements,
        _options: &BTreeMap<String, String>,
        _context: &backend::MigrationContext,
    ) -> crate::storage::migration::Result<Arc<dyn crate::storage::migration::SnapshotExport>> {
        Err(crate::storage::migration::Error::Invalid(
            "backend does not support logical export".into(),
        ))
    }
    fn memory(
        &self,
        _endpoint: Endpoint<'_>,
        _options: MemoryOptions,
    ) -> crate::memory::Result<Arc<dyn MemoryStore>> {
        Err(crate::memory::Error::new(
            crate::memory::ErrorKind::Unavailable,
        ))
    }
    fn approvals(
        &self,
        _endpoint: Endpoint<'_>,
    ) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        Err(crate::approval::Error::Unavailable)
    }
    fn schedule(&self, _endpoint: Endpoint<'_>) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
        Err(crate::schedule::Error)
    }
    fn workflow(&self, _endpoint: Endpoint<'_>) -> Result<Arc<dyn WorkflowStore>, String> {
        Err("workflow storage is unavailable".into())
    }
    fn usage(&self, _endpoint: Endpoint<'_>) -> crate::usage::Result<Arc<dyn UsageRepository>> {
        Err(crate::usage::Error)
    }
    fn tasks(
        &self,
        _endpoint: Endpoint<'_>,
    ) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
        Err(crate::subagent::record::Error)
    }
    fn reflection(
        &self,
        _endpoint: Endpoint<'_>,
    ) -> crate::reflection::store::Result<Arc<dyn ReflectionStore>> {
        Err(crate::reflection::store::Error::Io)
    }
    fn skill_checks(
        &self,
        _endpoint: Endpoint<'_>,
    ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
        Err(crate::skill::check_repository::StoreError::Io)
    }
    fn memory_advice(
        &self,
        _endpoint: Endpoint<'_>,
    ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
        Err(crate::memory::advice::audit::Error::Io)
    }
}

/// Reuses the current repository schemas with any conforming SQLite-profile driver.
pub struct SqlRepositories {
    driver: Arc<dyn SqlDriver>,
}

impl SqlRepositories {
    pub fn new(driver: Arc<dyn SqlDriver>) -> Self {
        Self { driver }
    }

    fn connection(
        &self,
        endpoint: Endpoint<'_>,
        fts: bool,
        codec: &dyn crate::storage::backends::turso::migration::SqlDomainCodec,
    ) -> crate::storage::Result<Connection> {
        let requirements = SqlRequirements {
            fts,
            ..if endpoint.address == ":memory:" {
                SqlRequirements::EPHEMERAL
            } else {
                SqlRequirements::DURABLE
            }
        };
        let connection = Connection::open_with_driver(
            self.driver.as_ref(),
            Path::new(endpoint.address),
            OpenOptions {
                read_only: false,
                policy: endpoint.policy,
            },
            requirements,
        )?;
        if endpoint.policy == OpenPolicy::Existing {
            codec
                .validate(&connection)
                .map_err(|_| crate::storage::Error::IncompatibleSchema)?;
        }
        Ok(connection)
    }
}

impl RepositoryFactory for SqlRepositories {
    fn memory(
        &self,
        endpoint: Endpoint<'_>,
        options: MemoryOptions,
    ) -> crate::memory::Result<Arc<dyn MemoryStore>> {
        let connection = self
            .connection(
                endpoint,
                true,
                &crate::storage::backends::turso::migration::memory::MemoryCodec,
            )
            .map_err(|_| crate::memory::Error::new(crate::memory::ErrorKind::Unavailable))?;
        Ok(Arc::new(
            crate::storage::backends::turso::memory::Store::from_connection(connection, options)?,
        ))
    }
    fn approvals(&self, endpoint: Endpoint<'_>) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::ApprovalCodec,
            )
            .map_err(|_| crate::approval::Error::Unavailable)?;
        Ok(Arc::new(
            crate::storage::backends::turso::approval::Store::from_connection(connection)?,
        ))
    }
    fn schedule(&self, endpoint: Endpoint<'_>) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::schedule::ScheduleCodec,
            )
            .map_err(|_| crate::schedule::Error)?;
        Ok(Arc::new(
            crate::storage::backends::turso::schedule::TursoScheduleStore::from_connection(
                connection,
            )?,
        ))
    }
    fn workflow(&self, endpoint: Endpoint<'_>) -> Result<Arc<dyn WorkflowStore>, String> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::workflow::WorkflowCodec,
            )
            .map_err(|_| "workflow storage is unavailable".to_owned())?;
        Ok(Arc::new(
            crate::storage::backends::turso::workflow::TursoWorkflowStore::from_connection(
                connection,
            )?,
        ))
    }
    fn usage(&self, endpoint: Endpoint<'_>) -> crate::usage::Result<Arc<dyn UsageRepository>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::usage::UsageCodec,
            )
            .map_err(|_| crate::usage::Error)?;
        Ok(Arc::new(
            crate::storage::backends::turso::usage::Store::from_connection(connection)?,
        ))
    }
    fn tasks(
        &self,
        endpoint: Endpoint<'_>,
    ) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::tasks::TaskCodec,
            )
            .map_err(|_| crate::subagent::record::Error)?;
        Ok(Arc::new(
            crate::storage::backends::turso::tasks::Store::from_connection(connection)?,
        ))
    }
    fn reflection(
        &self,
        endpoint: Endpoint<'_>,
    ) -> crate::reflection::store::Result<Arc<dyn ReflectionStore>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::reflection::ReflectionCodec,
            )
            .map_err(|_| crate::reflection::store::Error::Io)?;
        Ok(Arc::new(
            crate::storage::backends::turso::reflection::Store::from_connection(connection)?,
        ))
    }
    fn skill_checks(
        &self,
        endpoint: Endpoint<'_>,
    ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::skill_checks::SkillChecksCodec,
            )
            .map_err(|_| crate::skill::check_repository::StoreError::Io)?;
        Ok(Arc::new(
            crate::storage::backends::turso::skill_checks::Store::from_connection(connection)?,
        ))
    }
    fn memory_advice(
        &self,
        endpoint: Endpoint<'_>,
    ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
        let connection = self
            .connection(
                endpoint,
                false,
                &crate::storage::backends::turso::migration::memory_advice::MemoryAdviceCodec,
            )
            .map_err(|_| crate::memory::advice::audit::Error::Io)?;
        Ok(Arc::new(
            crate::storage::backends::turso::memory_advice::Store::from_connection(connection)?,
        ))
    }
}

/// Registry and validated routes owned by one runtime, never global mutable state.
pub struct Storage {
    factories: HashMap<String, Arc<dyn RepositoryFactory>>,
    routes: BTreeMap<String, StorageRoute>,
    session_backends: HashMap<String, Arc<dyn SessionRepository>>,
    _writer: Option<WriterPermit>,
    configured: BTreeMap<String, backend::ConfiguredEndpoint>,
    configured_factories: BTreeMap<String, Arc<dyn RepositoryFactory>>,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            factories: HashMap::from([(
                "sqlite".into(),
                Arc::new(SqlRepositories::new(Arc::new(
                    crate::storage::drivers::turso::TursoDriver,
                ))) as Arc<dyn RepositoryFactory>,
            )]),
            routes: BTreeMap::new(),
            session_backends: HashMap::from([(
                "posix".into(),
                Arc::new(
                    crate::storage::backends::posix::session::PosixSessionRepository::new(
                        Arc::new(crate::storage::drivers::posix::PosixDriver),
                        crate::failover::lease::new_keeper(),
                    ),
                ) as Arc<dyn SessionRepository>,
            )]),
            _writer: None,
            configured: BTreeMap::new(),
            configured_factories: BTreeMap::new(),
        }
    }
}

impl Storage {
    /// Hold writer admission for the runtime lifetime so migration cannot race startup.
    pub async fn open_home(home: &Path) -> Result<Self, String> {
        Self::open_home_with(
            home,
            &BackendRegistry::default(),
            &Default::default(),
            &HashMap::new(),
        )
        .await
    }

    /// Extensions are registered before persisted routes are validated or writers admitted.
    pub async fn open_home_with(
        home: &Path,
        registry: &BackendRegistry,
        configuration: &otto_core::config::storage::Storage,
        environment: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut storage = Self::default();
        for (name, factory) in &registry.factories {
            storage.register(name.clone(), factory.clone())?;
        }
        storage.configured = backend::resolve(configuration, environment, home)?;
        for (instance, binding) in &storage.configured {
            let factory = storage
                .factories
                .get(&binding.factory)
                .ok_or("storage backend is unavailable")?;
            let configured = factory
                .configured(&binding.endpoint, &binding.options)
                .await
                .map_err(|_| "storage backend options are unsupported or invalid")?;
            storage.configured_factories.insert(
                instance.clone(),
                configured.unwrap_or_else(|| factory.clone()),
            );
        }
        let canonical_home = home
            .canonicalize()
            .map_err(|_| "storage home is unavailable")?;
        let home_dir = crate::fsops::open_dir_no_follow(&canonical_home)
            .map_err(|_| "storage home is unavailable")?;
        match crate::fsops::mkdir_at(&home_dir, ".otto", 0o700) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err("storage control directory is unavailable".into()),
        }
        let otto_dir = crate::fsops::open_dir_at_no_follow(&home_dir, ".otto")
            .map_err(|_| "storage control directory is unavailable")?;
        match crate::fsops::mkdir_at(&otto_dir, "storage", 0o700) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err("storage control directory is unavailable".into()),
        }
        use sha2::{Digest, Sha256};
        let identity = format!(
            "local:{:x}",
            Sha256::digest(canonical_home.as_os_str().as_encoded_bytes())
        );
        let authority = LocalCutover::open(
            &canonical_home.join(".otto/storage"),
            RouteGroup {
                identity,
                generation: "initial".into(),
                writer_epoch: "initial".into(),
                domains: BTreeMap::new(),
            },
        )
        .map_err(|_| "storage routing authority is unavailable")?;
        let group = authority
            .routes()
            .await
            .map_err(|_| "storage routing authority is unavailable")?;
        let configured_routes = backend::configured_routes(configuration);
        if configured_routes.iter().any(|(domain, route)| {
            group.domains.get(domain).is_none_or(|published| {
                published.backend != route.backend || published.endpoint != route.endpoint
            })
        }) {
            return Err("configured storage differs from published routes; initialize a fresh installation or transfer existing state first".into());
        }
        storage = storage.with_routes(group.domains)?;
        storage.validate_configured().await?;
        let writer = authority
            .acquire_writer(&group.identity, &group.generation)
            .await
            .map_err(|_| "storage is frozen for migration or its route changed")?;
        storage._writer = Some(writer);
        Ok(storage)
    }

    async fn validate_configured(&self) -> Result<(), String> {
        for (domain, route) in &self.routes {
            let Some(instance) = route.endpoint.strip_prefix("instance:") else {
                if !matches!(route.backend.as_str(), "sqlite" | "posix") {
                    return Err(
                        "external storage routes require a configured backend instance".into(),
                    );
                }
                continue;
            };
            let binding = self
                .configured
                .get(instance)
                .ok_or("published storage instance is not configured")?;
            let factory = self
                .configured_factories
                .get(instance)
                .ok_or("storage backend is unavailable")?;
            let identity = factory
                .validate(
                    &binding.endpoint,
                    &RepositoryRequirements::for_domain(domain)?,
                    &binding.options,
                )
                .await
                .map_err(
                    |_| "storage namespace or domain profile is incompatible or unavailable",
                )?;
            identity
                .validate()
                .map_err(|_| "storage namespace identity is invalid")?;
            if identity.0.is_empty()
                || route.namespace_identity.as_deref() != Some(identity.0.as_str())
            {
                return Err("storage namespace differs from the published identity".into());
            }
        }
        Ok(())
    }
    pub fn register(
        &mut self,
        name: String,
        factory: Arc<dyn RepositoryFactory>,
    ) -> Result<(), String> {
        if name.trim().is_empty() || self.factories.contains_key(&name) {
            return Err("storage backend name is empty or already registered".into());
        }
        self.factories.insert(name, factory);
        Ok(())
    }

    pub fn with_routes(mut self, routes: BTreeMap<String, StorageRoute>) -> Result<Self, String> {
        for (domain, route) in &routes {
            let supported = if domain == "sessions" {
                self.session_backends.contains_key(&route.backend)
            } else {
                self.factories.contains_key(&route.backend)
            };
            if !matches!(
                domain.as_str(),
                "memory"
                    | "approvals"
                    | "workflows"
                    | "schedule"
                    | "usage"
                    | "tasks"
                    | "reflection"
                    | "skill_checks"
                    | "memory_advice"
                    | "sessions"
            ) || !supported
                || route.endpoint.trim().is_empty()
                || route.endpoint.chars().any(char::is_control)
            {
                return Err("storage route is invalid or its backend is unavailable".into());
            }
        }
        self.routes = routes;
        Ok(self)
    }

    pub fn register_sessions(
        &mut self,
        name: String,
        repository: Arc<dyn SessionRepository>,
    ) -> Result<(), String> {
        if name.trim().is_empty() || self.session_backends.contains_key(&name) {
            return Err("session backend name is empty or already registered".into());
        }
        self.session_backends.insert(name, repository);
        Ok(())
    }

    pub fn sessions(&self) -> Result<&dyn SessionRepository, String> {
        let name = self
            .routes
            .get("sessions")
            .map_or("posix", |route| route.backend.as_str());
        Ok(self
            .session_backends
            .get(name)
            .ok_or("session backend is unavailable")?
            .as_ref())
    }

    pub fn session_root(&self, default: &Path) -> Result<std::path::PathBuf, String> {
        match self.routes.get("sessions") {
            Some(route) => {
                let root = std::path::PathBuf::from(&route.endpoint);
                self.sessions()?.validate_existing_root(&root)?;
                Ok(root)
            }
            None => Ok(default.to_owned()),
        }
    }

    fn route<'a>(
        &'a self,
        domain: &str,
        default: &'a Path,
    ) -> Result<(&'a dyn RepositoryFactory, Endpoint<'a>), String> {
        let (backend, endpoint) = match self.routes.get(domain) {
            Some(route) => (
                route.backend.as_str(),
                if route.endpoint.starts_with("instance:") {
                    backend::endpoint(route, &self.configured)?
                } else {
                    Endpoint {
                        address: &route.endpoint,
                        policy: OpenPolicy::Existing,
                    }
                },
            ),
            None => (
                "sqlite",
                Endpoint {
                    address: default.to_str().ok_or("storage endpoint is not UTF-8")?,
                    policy: OpenPolicy::CreateIfMissing,
                },
            ),
        };
        Ok((
            self.routes
                .get(domain)
                .and_then(|route| route.endpoint.strip_prefix("instance:"))
                .and_then(|instance| self.configured_factories.get(instance))
                .or_else(|| self.factories.get(backend))
                .ok_or("storage backend is unavailable")?
                .as_ref(),
            endpoint,
        ))
    }

    pub fn memory(
        &self,
        default: &Path,
        options: MemoryOptions,
    ) -> crate::memory::Result<Arc<dyn MemoryStore>> {
        let (factory, endpoint) = self
            .route("memory", default)
            .map_err(|_| crate::memory::Error::new(crate::memory::ErrorKind::Unavailable))?;
        factory.memory(endpoint, options)
    }

    pub fn memory_endpoint(&self, default: &Path) -> Result<String, String> {
        match self.routes.get("memory") {
            Some(route) => Ok(route.endpoint.clone()),
            None => Ok(default
                .to_str()
                .ok_or("storage endpoint is not UTF-8")?
                .to_owned()),
        }
    }
    pub fn memory_backend<'a>(&'a self, default: &'a str) -> &'a str {
        self.routes
            .get("memory")
            .filter(|route| route.backend != "sqlite")
            .map_or(default, |route| route.backend.as_str())
    }
    pub fn approvals(&self, default: &Path) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        let (factory, endpoint) = self
            .route("approvals", default)
            .map_err(|_| crate::approval::Error::Unavailable)?;
        factory.approvals(endpoint)
    }
    pub fn schedule(&self, default: &Path) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
        let (factory, endpoint) = self
            .route("schedule", default)
            .map_err(|_| crate::schedule::Error)?;
        factory.schedule(endpoint)
    }
    pub fn workflow(&self, default: &Path) -> Result<Arc<dyn WorkflowStore>, String> {
        let (factory, endpoint) = self.route("workflows", default)?;
        factory.workflow(endpoint)
    }

    pub fn usage(&self, default: &Path) -> crate::usage::Result<Arc<dyn UsageRepository>> {
        let (factory, endpoint) = self
            .route("usage", default)
            .map_err(|_| crate::usage::Error)?;
        factory.usage(endpoint)
    }

    pub fn tasks(
        &self,
        default: &Path,
    ) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
        let (factory, endpoint) = self
            .route("tasks", default)
            .map_err(|_| crate::subagent::record::Error)?;
        factory.tasks(endpoint)
    }

    pub fn reflection(
        &self,
        default: &Path,
    ) -> crate::reflection::store::Result<Arc<dyn ReflectionStore>> {
        let (factory, endpoint) = self
            .route("reflection", default)
            .map_err(|_| crate::reflection::store::Error::Io)?;
        factory.reflection(endpoint)
    }

    pub fn skill_checks(
        &self,
        default: &Path,
    ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
        let (factory, endpoint) = self
            .route("skill_checks", default)
            .map_err(|_| crate::skill::check_repository::StoreError::Io)?;
        factory.skill_checks(endpoint)
    }

    pub fn memory_advice(
        &self,
        default: &Path,
    ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
        let (factory, endpoint) = self
            .route("memory_advice", default)
            .map_err(|_| crate::memory::advice::audit::Error::Io)?;
        factory.memory_advice(endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalMode;
    use crate::storage::{SqlCapabilities, SqlConnection, SqlDialect, WriteIsolation};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RegisteredFactory {
        inner: SqlRepositories,
        calls: Arc<AtomicUsize>,
    }
    impl RepositoryFactory for RegisteredFactory {
        fn memory(
            &self,
            endpoint: Endpoint<'_>,
            options: MemoryOptions,
        ) -> crate::memory::Result<Arc<dyn MemoryStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.memory(endpoint, options)
        }
        fn approvals(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.approvals(endpoint)
        }
        fn schedule(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.schedule(endpoint)
        }
        fn workflow(&self, endpoint: Endpoint<'_>) -> Result<Arc<dyn WorkflowStore>, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.workflow(endpoint)
        }
        fn usage(&self, endpoint: Endpoint<'_>) -> crate::usage::Result<Arc<dyn UsageRepository>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.usage(endpoint)
        }
        fn tasks(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.tasks(endpoint)
        }
        fn reflection(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::reflection::store::Result<Arc<dyn ReflectionStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.reflection(endpoint)
        }
        fn skill_checks(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.skill_checks(endpoint)
        }
        fn memory_advice(
            &self,
            endpoint: Endpoint<'_>,
        ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.memory_advice(endpoint)
        }
    }
    fn factory(calls: Arc<AtomicUsize>) -> Arc<dyn RepositoryFactory> {
        Arc::new(RegisteredFactory {
            inner: SqlRepositories::new(Arc::new(crate::storage::drivers::turso::TursoDriver)),
            calls,
        })
    }
    fn route(backend: &str, endpoint: &str) -> BTreeMap<String, StorageRoute> {
        BTreeMap::from([(
            "approvals".into(),
            StorageRoute {
                backend: backend.into(),
                endpoint: endpoint.into(),
                namespace_identity: None,
            },
        )])
    }

    #[test]
    fn registry_rejects_duplicate_backends_and_unusable_routes() {
        let mut storage = Storage::default();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(
            storage
                .register("sqlite".into(), factory(calls.clone()))
                .is_err()
        );
        assert!(
            storage
                .register("  ".into(), factory(calls.clone()))
                .is_err()
        );
        storage
            .register("extension".into(), factory(calls.clone()))
            .unwrap();
        assert!(
            storage
                .register("extension".into(), factory(calls.clone()))
                .is_err()
        );
        assert!(
            Storage::default()
                .with_routes(route("missing", ":memory:"))
                .is_err()
        );
        assert!(
            Storage::default()
                .with_routes(route("sqlite", " "))
                .is_err()
        );
        assert!(
            Storage::default()
                .with_routes(route("sqlite", "endpoint\n"))
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn registered_backend_composes_with_unchanged_approval_service() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut storage = Storage::default();
        storage
            .register("extension".into(), factory(calls.clone()))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("approvals.db");
        drop(Storage::default().approvals(&path).unwrap());
        let storage = storage
            .with_routes(route("extension", path.to_str().unwrap()))
            .unwrap();
        let service =
            crate::approval::Service::from_store(storage.approvals(Path::new("unused")).unwrap())
                .unwrap();
        let scope = crate::approval::Scope {
            workspace: "/workspace".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Auto);
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn registered_backend_composes_telemetry_and_reflection_repositories() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut storage = Storage::default();
        storage
            .register("extension".into(), factory(calls.clone()))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let defaults = Storage::default();
        drop(defaults.usage(&directory.path().join("usage.db")).unwrap());
        drop(defaults.tasks(&directory.path().join("tasks.db")).unwrap());
        drop(
            defaults
                .reflection(&directory.path().join("reflection.db"))
                .unwrap(),
        );
        drop(
            defaults
                .skill_checks(&directory.path().join("skill_checks.db"))
                .unwrap(),
        );
        drop(
            defaults
                .memory_advice(&directory.path().join("memory_advice.db"))
                .unwrap(),
        );
        let routes = [
            "usage",
            "tasks",
            "reflection",
            "skill_checks",
            "memory_advice",
        ]
        .into_iter()
        .map(|domain| {
            (
                domain.to_owned(),
                StorageRoute {
                    backend: "extension".into(),
                    endpoint: directory
                        .path()
                        .join(format!("{domain}.db"))
                        .to_str()
                        .unwrap()
                        .into(),
                    namespace_identity: None,
                },
            )
        })
        .collect();
        let storage = storage.with_routes(routes).unwrap();

        assert_eq!(
            storage
                .usage(Path::new("unused"))
                .unwrap()
                .summary(None)
                .unwrap()
                .requests,
            0
        );
        assert!(
            storage
                .tasks(Path::new("unused"))
                .unwrap()
                .list(&Default::default())
                .unwrap()
                .tasks
                .is_empty()
        );
        assert_eq!(
            storage
                .reflection(Path::new("unused"))
                .unwrap()
                .watermark("session")
                .unwrap(),
            None
        );
        assert!(storage.skill_checks(Path::new("unused")).is_ok());
        assert!(storage.memory_advice(Path::new("unused")).is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 5);
    }

    struct IncapableDriver {
        opens: Arc<AtomicUsize>,
    }
    impl SqlDriver for IncapableDriver {
        fn capabilities(&self, _: &Path) -> SqlCapabilities {
            SqlCapabilities {
                dialect: SqlDialect::Sqlite,
                write_isolation: WriteIsolation::ImmediateSerializable,
                atomic_transactions: true,
                durable_commits: false,
                multiprocess_writers: false,
                fts: false,
            }
        }
        fn open(&self, _: &Path, _: OpenOptions) -> crate::storage::Result<Box<dyn SqlConnection>> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Err(crate::storage::Error::Unavailable)
        }
    }
    #[test]
    fn unsupported_driver_is_rejected_before_native_endpoint_creation() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("new-parent/state.db");
        let opens = Arc::new(AtomicUsize::new(0));
        let factory = SqlRepositories::new(Arc::new(IncapableDriver {
            opens: opens.clone(),
        }));
        assert!(matches!(
            factory.connection(
                Endpoint {
                    address: endpoint.to_str().unwrap(),
                    policy: OpenPolicy::CreateIfMissing
                },
                false,
                &crate::storage::backends::turso::migration::ApprovalCodec
            ),
            Err(crate::storage::Error::UnsupportedCapability(
                "durable commits"
            ))
        ));
        assert!(
            factory
                .approvals(Endpoint {
                    address: endpoint.to_str().unwrap(),
                    policy: OpenPolicy::CreateIfMissing
                })
                .is_err()
        );
        assert!(matches!(
            factory.connection(
                Endpoint {
                    address: ":memory:",
                    policy: OpenPolicy::CreateIfMissing
                },
                true,
                &crate::storage::backends::turso::migration::memory::MemoryCodec
            ),
            Err(crate::storage::Error::UnsupportedCapability(_))
        ));
        assert_eq!(opens.load(Ordering::Relaxed), 0);
        assert!(!endpoint.parent().unwrap().exists());
    }
    #[test]
    fn active_sql_routes_refuse_missing_empty_or_wrong_family_without_initializing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-parent/authority.db");
        let storage = Storage::default()
            .with_routes(route("sqlite", path.to_str().unwrap()))
            .unwrap();
        assert!(storage.approvals(Path::new("ignored-default")).is_err());
        assert!(!path.parent().unwrap().exists());

        let path = directory.path().join("empty.db");
        std::fs::write(&path, b"").unwrap();
        let storage = Storage::default()
            .with_routes(route("sqlite", path.to_str().unwrap()))
            .unwrap();
        assert!(storage.approvals(Path::new("ignored-default")).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        let path = directory.path().join("wrong-family.db");
        drop(Storage::default().usage(&path).unwrap());
        let storage = Storage::default()
            .with_routes(route("sqlite", path.to_str().unwrap()))
            .unwrap();
        assert!(storage.approvals(Path::new("ignored-default")).is_err());
        let connection = Connection::open_read_only(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='approvals'",
                    (),
                    |row| row.get::<i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn defaults_create_once_but_deleted_active_route_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new-parent/authority.db");
        let service =
            crate::approval::Service::from_store(Storage::default().approvals(&path).unwrap())
                .unwrap();
        let scope = crate::approval::Scope {
            workspace: "/workspace".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        };
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        drop(service);
        let storage = Storage::default()
            .with_routes(route("sqlite", path.to_str().unwrap()))
            .unwrap();
        let service = crate::approval::Service::from_store(
            storage.approvals(Path::new("ignored-default")).unwrap(),
        )
        .unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
        drop(service);
        std::fs::remove_file(&path).unwrap();
        assert!(storage.approvals(Path::new("ignored-default")).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn configured_session_root_must_already_exist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-sessions");
        let storage = Storage::default()
            .with_routes(BTreeMap::from([(
                "sessions".into(),
                StorageRoute {
                    backend: "posix".into(),
                    endpoint: path.to_str().unwrap().into(),
                    namespace_identity: None,
                },
            )]))
            .unwrap();
        assert!(storage.session_root(Path::new("ignored-default")).is_err());
        assert!(!path.exists());

        let empty = directory.path().join("empty-sessions");
        std::fs::create_dir(&empty).unwrap();
        let empty_storage = Storage::default()
            .with_routes(BTreeMap::from([(
                "sessions".into(),
                StorageRoute {
                    backend: "posix".into(),
                    endpoint: empty.to_str().unwrap().into(),
                    namespace_identity: None,
                },
            )]))
            .unwrap();
        assert!(
            empty_storage
                .session_root(Path::new("ignored-default"))
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);

        let fresh_default = directory.path().join("lazy-default");
        assert_eq!(
            Storage::default().session_root(&fresh_default).unwrap(),
            fresh_default
        );
        assert!(!fresh_default.exists());
    }
    struct PolicyDriver(Arc<std::sync::atomic::AtomicBool>);
    impl SqlDriver for PolicyDriver {
        fn capabilities(&self, _: &Path) -> SqlCapabilities {
            SqlCapabilities {
                dialect: SqlDialect::Sqlite,
                write_isolation: WriteIsolation::ImmediateSerializable,
                atomic_transactions: true,
                durable_commits: true,
                multiprocess_writers: true,
                fts: false,
            }
        }
        fn open(
            &self,
            path: &Path,
            options: OpenOptions,
        ) -> crate::storage::Result<Box<dyn SqlConnection>> {
            assert_eq!(path, Path::new("opaque:namespace"));
            self.0
                .store(options.policy == OpenPolicy::Existing, Ordering::Relaxed);
            Err(crate::storage::Error::Unavailable)
        }
    }
    #[test]
    fn active_open_policy_reaches_custom_driver_without_local_path_assumptions() {
        let existing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut storage = Storage::default();
        storage
            .register(
                "driver".into(),
                Arc::new(SqlRepositories::new(Arc::new(PolicyDriver(
                    existing.clone(),
                )))),
            )
            .unwrap();
        let storage = storage
            .with_routes(route("driver", "opaque:namespace"))
            .unwrap();
        assert!(storage.approvals(Path::new("unused")).is_err());
        assert!(existing.load(Ordering::Relaxed));
    }
}
