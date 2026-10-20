
use super::*;
use crate::storage::{
    OpenOptions, Params, SqlCapabilities, SqlConnection, SqlDriver, SqlStatement,
};
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
#[derive(Default)]
struct Faults {
    lose_ack: AtomicBool,
    fail_open: AtomicBool,
    poisoned_calls: AtomicUsize,
    opens: AtomicUsize,
}
struct Driver(Arc<Faults>);
impl SqlDriver for Driver {
    fn capabilities(&self, path: &Path) -> SqlCapabilities {
        crate::storage::drivers::turso::TursoDriver.capabilities(path)
    }
    fn open(
        &self,
        path: &Path,
        options: OpenOptions,
    ) -> crate::storage::Result<Box<dyn SqlConnection>> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_open.load(Ordering::SeqCst) {
            return Err(crate::storage::Error::Unavailable);
        }
        Ok(Box::new(LostAck {
            inner: crate::storage::drivers::turso::TursoDriver.open(path, options)?,
            faults: self.0.clone(),
            poisoned: AtomicBool::new(false),
        }))
    }
}
struct LostAck {
    inner: Box<dyn SqlConnection>,
    faults: Arc<Faults>,
    poisoned: AtomicBool,
}
impl LostAck {
    fn check(&self) -> crate::storage::Result<()> {
        if self.poisoned.load(Ordering::SeqCst) {
            self.faults.poisoned_calls.fetch_add(1, Ordering::SeqCst);
            return Err(crate::storage::Error::Closed);
        }
        Ok(())
    }
}
impl SqlConnection for LostAck {
    fn capabilities(&self) -> SqlCapabilities {
        self.inner.capabilities()
    }
    fn busy_timeout(&self, value: Duration) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.busy_timeout(value)
    }
    fn pragma_update(&self, name: &str, value: &str) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.pragma_update(name, value)
    }
    fn execute_batch(&self, sql: &str) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.execute_batch(sql)
    }
    fn execute(&self, sql: &str, params: Params) -> crate::storage::Result<usize> {
        self.check()?;
        self.inner.execute(sql, params)
    }
    fn prepare(&self, sql: &str) -> crate::storage::Result<Box<dyn SqlStatement>> {
        self.check()?;
        self.inner.prepare(sql)
    }
    fn begin(&self) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.begin()
    }
    fn commit(&self) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.commit()?;
        if self.faults.lose_ack.swap(false, Ordering::SeqCst) {
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(crate::storage::Error::CommitUnknown);
        }
        Ok(())
    }
    fn rollback(&self) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.rollback()
    }
    fn checkpoint(&self) -> crate::storage::Result<()> {
        self.check()?;
        self.inner.checkpoint()
    }
    fn close(self: Box<Self>) -> crate::storage::Result<()> {
        self.inner.close()
    }
}
struct Fence;
impl FreezeGuard for Fence {
    fn validate(&self, _: &str) -> Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn committed_chunk_lost_ack_uses_fresh_authoritative_receipt_or_remains_unknown() {
    for fail_reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let service = crate::approval::Service::open(&source_path).unwrap();
        service
            .set_mode(
                &otto_core::approval::Scope {
                    workspace: "/w".into(),
                    session_id: "s".into(),
                    task_id: String::new(),
                    run_id: String::new(),
                },
                otto_core::approval::ApprovalMode::Manual,
            )
            .unwrap();
        drop(service);
        let codec = Arc::new(ApprovalCodec);
        let source = Source::open(
            &source_path,
            "source".into(),
            "frozen".into(),
            Arc::new(Fence),
            codec.clone(),
        )
        .unwrap();
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            migration_id: "copy".into(),
            destination_identity: "target".into(),
            expected_route_generation: "route".into(),
            max_chunk_bytes: 64 * 1024,
            source: source.descriptor().await.unwrap(),
            progress: BTreeMap::from([(
                "approvals".into(),
                Progress {
                    cursor: String::new(),
                    complete: false,
                    chunks: vec![],
                },
            )]),
            destination_write_started: true,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        };
        let faults = Arc::new(Faults::default());
        let stage = Stage::with_driver(
            target.clone(),
            "target".into(),
            codec,
            Arc::new(Driver(faults.clone())),
        );
        stage.begin(&manifest).await.unwrap();
        let chunk = source
            .read_chunk("approvals", "", manifest.max_chunk_bytes)
            .await
            .unwrap();
        let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
        let opens = faults.opens.load(Ordering::SeqCst);
        faults.lose_ack.store(true, Ordering::SeqCst);
        faults.fail_open.store(fail_reopen, Ordering::SeqCst);
        let outcome = stage.apply_chunk("copy", "approvals", &chunk, &hash).await;
        if fail_reopen {
            assert!(matches!(outcome, Err(Error::CommitUnknown)));
            assert!(stage.chunk_receipt("copy", "approvals", "").await.is_err());
            faults.fail_open.store(false, Ordering::SeqCst);
        } else {
            assert!(outcome.is_ok());
        }
        assert!(faults.opens.load(Ordering::SeqCst) > opens);
        assert_eq!(faults.poisoned_calls.load(Ordering::SeqCst), 0);
        let receipt = stage
            .chunk_receipt("copy", "approvals", "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.sha256, hash);
        assert_eq!(
            stage
                .apply_chunk("copy", "approvals", &chunk, &hash)
                .await
                .unwrap(),
            receipt
        );
        let connection = Connection::open(&target).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM approval_modes", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM otto_migration_chunks", (), |r| r
                    .get::<i64>(0))
                .unwrap(),
            1
        );
        stage.close().await.unwrap();
        source.close().await.unwrap();
    }
}
