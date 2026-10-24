use super::*;
use crate::storage::{SqlCapabilities, SqlConnection, SqlDialect, WriteIsolation};
use otto_core::approval::ApprovalMode;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RegisteredFactory {
    inner: SqlRepositories,
    calls: Arc<AtomicUsize>,
}
impl RepositoryFactory for RegisteredFactory {
    fn approvals(&self, endpoint: Endpoint<'_>) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.approvals(endpoint)
    }
    fn schedule(&self, endpoint: Endpoint<'_>) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
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
fn memory_route_is_accepted_without_a_registered_backend() {
    let directory = tempfile::tempdir().unwrap();
    let approvals = directory.path().join("approvals.db");
    drop(Storage::default().approvals(&approvals).unwrap());
    let mut routes = route("sqlite", approvals.to_str().unwrap());
    routes.insert(
        "memory".into(),
        StorageRoute {
            backend: "unregistered".into(),
            endpoint: "legacy-memory-source".into(),
            namespace_identity: None,
        },
    );
    let storage = Storage::default().with_routes(routes.clone()).unwrap();
    assert!(storage.approvals(Path::new("ignored-default")).is_ok());
    assert_eq!(
        storage.memory_endpoint(Path::new("default")).unwrap(),
        "legacy-memory-source"
    );
    routes.get_mut("memory").unwrap().endpoint = " ".into();
    assert!(Storage::default().with_routes(routes).is_err());
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
    let scope = otto_core::approval::Scope {
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
fn registered_backend_composes_telemetry_repositories() {
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
            .skill_checks(&directory.path().join("skill_checks.db"))
            .unwrap(),
    );
    drop(
        defaults
            .memory_advice(&directory.path().join("memory_advice.db"))
            .unwrap(),
    );
    let routes = ["usage", "tasks", "skill_checks", "memory_advice"]
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
            .usage_events,
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
    assert!(storage.skill_checks(Path::new("unused")).is_ok());
    assert!(storage.memory_advice(Path::new("unused")).is_ok());
    assert_eq!(calls.load(Ordering::Relaxed), 4);
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
        crate::approval::Service::from_store(Storage::default().approvals(&path).unwrap()).unwrap();
    let scope = otto_core::approval::Scope {
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
