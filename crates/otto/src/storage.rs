//! Synchronous native storage boundary backed exclusively by Turso.
//!
//! Existing services serialize connections with their own mutexes. Driving
//! Turso's local I/O here keeps their synchronous ownership and cancellation
//! contracts intact, including when called from a Tokio current-thread runtime.
//! No Tokio runtime is entered or created. Rows stay streaming and bounded by
//! the caller's query; transactions roll back on drop.

use std::future::Future;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

pub use turso::{Error, IntoParams, Result, Row, Value, params_from_iter};

#[macro_export]
macro_rules! storage_params {
    ($($value:expr),* $(,)?) => {{
        use turso::params::IntoValue as _;
        [$((($value).to_owned()).into_value()),*]
    }};
}
pub use crate::storage_params as params;

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

pub struct Connection {
    inner: turso::Connection,
    // Retain the database until its connection and statements are gone.
    _database: turso::Database,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TursoConnection")
    }
}

impl Connection {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_options(path.as_ref(), false)
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_options(path.as_ref(), true)
    }

    fn open_options(path: &Path, read_only: bool) -> Result<Self> {
        let path = path
            .to_str()
            .ok_or_else(|| Error::Misuse("database path is not UTF-8".into()))?;
        let database = complete(
            turso::Builder::new_local(path)
                .read_only(read_only)
                .experimental_index_method(true)
                .experimental_multiprocess_wal(path != ":memory:")
                .build(),
        )?;
        let inner = database.connect()?;
        if !read_only {
            complete(inner.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON"))?;
        }
        Ok(Self {
            inner,
            _database: database,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    pub fn busy_timeout(&self, duration: Duration) -> Result<()> {
        self.inner.busy_timeout(duration)
    }

    pub fn pragma_update(&self, name: &str, value: impl std::fmt::Display) -> Result<()> {
        complete(self.inner.pragma_update(name, value)).map(|_| ())
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        complete(self.inner.execute_batch(sql))
    }

    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        usize::try_from(complete(self.inner.execute(sql, params))?)
            .map_err(|_| Error::ConversionFailure("affected row count overflow".into()))
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        complete(self.inner.prepare(sql)).map(Statement)
    }

    pub fn query_row<T>(
        &self,
        sql: &str,
        params: impl IntoParams,
        decode: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<T> {
        self.prepare(sql)?.query_row(params, decode)
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.unchecked_transaction()
    }

    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        self.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Transaction {
            connection: self,
            finished: false,
        })
    }

    pub fn close(self) -> Result<()> {
        // Commits are durable in WAL; closing must not wait for other readers.
        drop(self);
        Ok(())
    }

    /// Backfill all WAL frames before publishing a renamed migration file.
    pub fn checkpoint(&self) -> Result<()> {
        let mut statement = self.prepare("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let mut rows = statement.query(())?;
        while let Some(row) = rows.next()? {
            let busy: i64 = row.get(0)?;
            if busy != 0 {
                return Err(Error::Busy("checkpoint could not finish".into()));
            }
        }
        Ok(())
    }
}

pub struct Statement(turso::Statement);

impl Statement {
    pub fn query(&mut self, params: impl IntoParams) -> Result<Rows> {
        complete(self.0.query(params)).map(|inner| Rows {
            inner,
            current: None,
        })
    }

    pub fn query_row<T>(
        &mut self,
        params: impl IntoParams,
        decode: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<T> {
        let mut rows = self.query(params)?;
        let row = rows.next()?.ok_or(Error::QueryReturnedNoRows)?;
        decode(row)
    }

    pub fn query_map<T, F>(
        &mut self,
        params: impl IntoParams,
        decode: F,
    ) -> Result<MappedRows<T, F>>
    where
        F: FnMut(&Row) -> Result<T>,
    {
        Ok(MappedRows {
            rows: self.query(params)?,
            decode,
        })
    }
}

pub struct Rows {
    inner: turso::Rows,
    current: Option<Row>,
}

impl Rows {
    // A fallible lending iterator; std::Iterator cannot express this lifetime.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<&Row>> {
        self.current = complete(self.inner.next())?;
        Ok(self.current.as_ref())
    }
}

pub struct MappedRows<T, F: FnMut(&Row) -> Result<T>> {
    rows: Rows,
    decode: F,
}

impl<T, F: FnMut(&Row) -> Result<T>> Iterator for MappedRows<T, F> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.rows.next() {
            Ok(Some(row)) => Some((self.decode)(row)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

pub struct Transaction<'a> {
    connection: &'a Connection,
    finished: bool,
}

impl Deref for Transaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection
    }
}

impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.connection.execute_batch("COMMIT")?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }
}

pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
