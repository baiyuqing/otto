//! Turso local I/O. No Tokio runtime is created or entered.
use crate::storage::{
    Error, OpenOptions, OpenPolicy, Params, Result, Row, SqlCapabilities, SqlConnection,
    SqlDialect, SqlDriver, SqlRows, SqlStatement, Value, WriteIsolation,
};
use std::future::Future;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn complete<T>(future: impl Future<Output = T>) -> T {
    // The engine compiles large memory predicates on the calling thread.
    // Grow once at this boundary so Tokio/test worker stack sizes are irrelevant.
    stacker::grow(16 << 20, || complete_on_stack(future))
}

fn complete_on_stack<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[derive(Debug, Default)]
pub struct TursoDriver;
impl SqlDriver for TursoDriver {
    fn capabilities(&self, path: &Path) -> SqlCapabilities {
        SqlCapabilities {
            dialect: SqlDialect::Sqlite,
            write_isolation: WriteIsolation::ImmediateSerializable,
            atomic_transactions: true,
            durable_commits: path != Path::new(":memory:"),
            multiprocess_writers: path != Path::new(":memory:"),
        }
    }
    fn open(&self, path: &Path, options: OpenOptions) -> Result<Box<dyn SqlConnection>> {
        let capabilities = self.capabilities(path);
        let path = path
            .to_str()
            .ok_or_else(|| Error::Misuse("database path is not UTF-8".into()))?;
        if options.policy == OpenPolicy::Existing && path == ":memory:" {
            return Err(Error::Unavailable);
        }
        // Local-path preparation belongs to this driver, never injected
        // repository constructors or factories for other storage families.
        if path != ":memory:" {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            let path = Path::new(path);
            if !options.read_only
                && options.policy == OpenPolicy::CreateIfMissing
                && let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|_| Error::Unavailable)?;
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!options.read_only)
                .create(!options.read_only && options.policy == OpenPolicy::CreateIfMissing)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)
                .map_err(|_| Error::Unavailable)?;
            let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
            if !metadata.is_file() {
                return Err(Error::Unavailable);
            }
            if options.policy == OpenPolicy::Existing && metadata.len() == 0 {
                return Err(Error::IncompatibleSchema);
            }
            if !options.read_only && options.policy == OpenPolicy::CreateIfMissing {
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| Error::Unavailable)?;
            }
        }
        let mut builder = turso::Builder::new_local(path)
            .read_only(options.read_only)
            .experimental_index_method(true)
            .experimental_multiprocess_wal(path != ":memory:");
        if options.policy == OpenPolicy::Existing {
            builder = builder.with_io_impl(Arc::new(ExistingDatabaseIo {
                inner: turso::core::SyscallIO::new().map_err(|_| Error::Unavailable)?,
                database_path: path.into(),
            }));
        }
        let database = complete(builder.build()).map_err(map_error)?;
        let inner = database.connect().map_err(map_error)?;
        if !options.read_only {
            complete(inner.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON"))
                .map_err(map_error)?;
        }
        Ok(Box::new(TursoConnection {
            inner,
            _database: database,
            capabilities,
            poisoned: Arc::new(AtomicBool::new(false)),
        }))
    }
}

/// The SDK's writable Builder always requests Create. Strip it at the actual
/// main-file open, including a deletion between our no-follow check and SDK I/O.
/// WAL files retain normal creation semantics for an existing database.
struct ExistingDatabaseIo {
    inner: turso::core::SyscallIO,
    database_path: String,
}
impl turso::core::Clock for ExistingDatabaseIo {
    fn current_time_monotonic(&self) -> turso::core::MonotonicInstant {
        turso::core::Clock::current_time_monotonic(&self.inner)
    }
    fn current_time_wall_clock(&self) -> turso::core::WallClockInstant {
        turso::core::Clock::current_time_wall_clock(&self.inner)
    }
}
impl turso::core::IO for ExistingDatabaseIo {
    fn open_file(
        &self,
        path: &str,
        mut flags: turso::core::OpenFlags,
        direct: bool,
    ) -> turso::core::Result<Arc<dyn turso::core::File>> {
        if path == self.database_path {
            flags.remove(turso::core::OpenFlags::Create);
        }
        turso::core::IO::open_file(&self.inner, path, flags, direct)
    }
    fn remove_file(&self, path: &str) -> turso::core::Result<()> {
        turso::core::IO::remove_file(&self.inner, path)
    }
    fn supports_shared_wal_coordination(&self) -> bool {
        turso::core::IO::supports_shared_wal_coordination(&self.inner)
    }
}

struct TursoConnection {
    inner: turso::Connection,
    _database: turso::Database,
    capabilities: SqlCapabilities,
    poisoned: Arc<AtomicBool>,
}
impl TursoConnection {
    fn check(&self) -> Result<()> {
        check(&self.poisoned)
    }
}
fn check(poisoned: &AtomicBool) -> Result<()> {
    if poisoned.load(Ordering::Acquire) {
        Err(Error::Closed)
    } else {
        Ok(())
    }
}
impl SqlConnection for TursoConnection {
    fn capabilities(&self) -> SqlCapabilities {
        self.capabilities
    }
    fn busy_timeout(&self, duration: Duration) -> Result<()> {
        self.check()?;
        self.inner.busy_timeout(duration).map_err(map_error)
    }
    fn pragma_update(&self, name: &str, value: &str) -> Result<()> {
        self.check()?;
        complete(self.inner.pragma_update(name, value))
            .map(|_| ())
            .map_err(map_error)
    }
    fn execute_batch(&self, sql: &str) -> Result<()> {
        self.check()?;
        complete(self.inner.execute_batch(sql)).map_err(map_error)
    }
    fn execute(&self, sql: &str, params: Params) -> Result<usize> {
        self.check()?;
        let count = complete(self.inner.execute(sql, parameters(params))).map_err(map_error)?;
        usize::try_from(count)
            .map_err(|_| Error::ConversionFailure("affected row count overflow".into()))
    }
    fn prepare(&self, sql: &str) -> Result<Box<dyn SqlStatement>> {
        self.check()?;
        let inner = complete(self.inner.prepare(sql)).map_err(map_error)?;
        Ok(Box::new(TursoStatement {
            inner,
            _database: self._database.clone(),
            poisoned: self.poisoned.clone(),
        }))
    }
    fn begin(&self) -> Result<()> {
        self.execute_batch("BEGIN IMMEDIATE")
    }
    fn commit(&self) -> Result<()> {
        self.check()?;
        match complete(self.inner.execute_batch("COMMIT")) {
            Ok(()) => Ok(()),
            Err(
                error @ (turso::Error::Busy(_)
                | turso::Error::BusySnapshot(_)
                | turso::Error::Constraint(_)),
            ) => Err(map_error(error)),
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::CommitUnknown)
            }
        }
    }
    fn rollback(&self) -> Result<()> {
        self.check()?;
        let result = complete(self.inner.execute_batch("ROLLBACK")).map_err(map_error);
        if result.is_err() {
            self.poisoned.store(true, Ordering::Release);
        }
        result
    }
    fn checkpoint(&self) -> Result<()> {
        self.check()?;
        let mut rows =
            complete(self.inner.query("PRAGMA wal_checkpoint(TRUNCATE)", ())).map_err(map_error)?;
        while let Some(row) = complete(rows.next()).map_err(map_error)? {
            if row.get::<i64>(0).map_err(map_error)? != 0 {
                return Err(Error::Busy("checkpoint could not finish".into()));
            }
        }
        Ok(())
    }
    fn close(self: Box<Self>) -> Result<()> {
        self.poisoned.store(true, Ordering::Release);
        drop(self);
        Ok(())
    }
}
struct TursoStatement {
    inner: turso::Statement,
    _database: turso::Database,
    poisoned: Arc<AtomicBool>,
}
impl SqlStatement for TursoStatement {
    fn query(&mut self, params: Params) -> Result<Box<dyn SqlRows>> {
        check(&self.poisoned)?;
        let inner = complete(self.inner.query(parameters(params))).map_err(map_error)?;
        Ok(Box::new(TursoRows {
            inner,
            _database: self._database.clone(),
            poisoned: self.poisoned.clone(),
        }))
    }
}
struct TursoRows {
    inner: turso::Rows,
    _database: turso::Database,
    poisoned: Arc<AtomicBool>,
}
impl SqlRows for TursoRows {
    fn next(&mut self) -> Result<Option<Row>> {
        check(&self.poisoned)?;
        complete(self.inner.next())
            .map_err(map_error)?
            .map(|row| {
                (0..row.column_count())
                    .map(|i| row.get_value(i).map(value).map_err(map_error))
                    .collect::<Result<Vec<_>>>()
                    .map(Row)
            })
            .transpose()
    }
}
fn parameters(params: Params) -> Vec<turso::Value> {
    params
        .0
        .into_iter()
        .map(|value| match value {
            Value::Null => turso::Value::Null,
            Value::Integer(value) => turso::Value::Integer(value),
            Value::Real(value) => turso::Value::Real(value),
            Value::Text(value) => turso::Value::Text(value),
            Value::Blob(value) => turso::Value::Blob(value),
        })
        .collect()
}
fn value(value: turso::Value) -> Value {
    match value {
        turso::Value::Null => Value::Null,
        turso::Value::Integer(value) => Value::Integer(value),
        turso::Value::Real(value) => Value::Real(value),
        turso::Value::Text(value) => Value::Text(value),
        turso::Value::Blob(value) => Value::Blob(value),
    }
}
fn map_error(error: turso::Error) -> Error {
    // Never let SQL text, bound values or an SDK error cross this boundary.
    match error {
        turso::Error::QueryReturnedNoRows => Error::QueryReturnedNoRows,
        turso::Error::ConversionFailure(_) => {
            Error::ConversionFailure("column conversion failed".into())
        }
        turso::Error::ToSqlConversionFailure(_) => {
            Error::ToSqlConversionFailure("invalid SQL parameter".into())
        }
        turso::Error::Busy(_) => Error::Busy("writer busy".into()),
        turso::Error::BusySnapshot(_) => Error::BusySnapshot("snapshot busy".into()),
        turso::Error::Misuse(_) => Error::Misuse("invalid connection operation".into()),
        turso::Error::Constraint(message) => {
            // Existing repositories distinguish duplicate identity from other
            // constraints. Preserve the category, not names or record data.
            let message = message.to_ascii_lowercase();
            let category = if message.contains("unique") {
                "UNIQUE constraint failed"
            } else if message.contains("primary key") {
                "PRIMARY KEY constraint failed"
            } else {
                "constraint failed"
            };
            Error::Constraint(category.into())
        }
        turso::Error::Corrupt(_) => Error::Corrupt("corrupt database".into()),
        turso::Error::NotAdb(_) => Error::NotAdb("invalid database".into()),
        _ => Error::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_policy_removes_sdk_create_at_actual_main_file_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.db");
        let io = ExistingDatabaseIo {
            inner: turso::core::SyscallIO::new().unwrap(),
            database_path: path.to_str().unwrap().into(),
        };
        assert!(
            turso::core::IO::open_file(
                &io,
                path.to_str().unwrap(),
                turso::core::OpenFlags::Create,
                false
            )
            .is_err()
        );
        assert!(!path.exists());
        let wal = directory.path().join("missing.db-wal");
        assert!(
            turso::core::IO::open_file(
                &io,
                wal.to_str().unwrap(),
                turso::core::OpenFlags::Create,
                false
            )
            .is_ok()
        );
        assert!(wal.exists());
    }
}
