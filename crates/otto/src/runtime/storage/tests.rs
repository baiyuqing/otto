use super::*;
use crate::storage::{SqlCapabilities, SqlConnection, SqlDialect, WriteIsolation};
use otto_core::approval::ApprovalMode;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        factory.connection(&endpoint),
        Err(crate::storage::Error::UnsupportedCapability(
            "durable commits"
        ))
    ));
    assert!(factory.approvals(&endpoint).is_err());
    assert_eq!(opens.load(Ordering::Relaxed), 0);
    assert!(!endpoint.parent().unwrap().exists());
}
#[test]
fn default_storage_creates_once_and_reopens_the_same_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new-parent/approvals.db");
    let storage = Storage::default();
    let service = crate::approval::Service::from_store(storage.approvals(&path).unwrap()).unwrap();
    let scope = otto_core::approval::Scope {
        workspace: "/workspace".into(),
        session_id: "session".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    drop(service);
    let service = crate::approval::Service::from_store(storage.approvals(&path).unwrap()).unwrap();
    assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
}
