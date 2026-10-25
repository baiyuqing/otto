//! Storage capability families and portable migration contracts.
//! SQL compatibility remains SQLite-specific. External dialects implement
//! the domain repositories through the runtime factory boundary.
pub mod backends;
#[cfg(feature = "storage-conformance")]
pub mod conformance;
pub mod contracts;
pub mod drivers;
pub mod files;
pub mod migration;
pub mod sql;
pub mod worker;
pub use contracts::*;
pub use sql::*;

use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

#[macro_export]
macro_rules! storage_params {
    ($($value:expr),* $(,)?) => {{
        use $crate::storage::IntoValue as _;
        [$((($value).to_owned()).into_value()),*]
    }};
}
pub use crate::storage_params as params;

pub struct Connection {
    inner: Box<dyn SqlConnection>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SqlConnection")
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
        Self::open_with_driver(
            &drivers::sqlite::SqliteDriver,
            path,
            OpenOptions {
                read_only,
                policy: if read_only {
                    OpenPolicy::Existing
                } else {
                    OpenPolicy::CreateIfMissing
                },
            },
            if path == Path::new(":memory:") {
                SqlRequirements::EPHEMERAL
            } else {
                SqlRequirements::DURABLE
            },
        )
    }

    pub fn open_with_driver(
        driver: &dyn SqlDriver,
        path: &Path,
        options: OpenOptions,
        requirements: SqlRequirements,
    ) -> Result<Self> {
        // Reject unsupported routes before opening or creating any state.
        driver.capabilities(path).validate(requirements)?;
        Self::from_driver(driver.open(path, options)?, requirements)
    }

    pub fn from_driver(
        inner: Box<dyn SqlConnection>,
        requirements: SqlRequirements,
    ) -> Result<Self> {
        inner.capabilities().validate(requirements)?;
        Ok(Self { inner })
    }

    pub fn capabilities(&self) -> SqlCapabilities {
        self.inner.capabilities()
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    pub fn busy_timeout(&self, duration: Duration) -> Result<()> {
        self.inner.busy_timeout(duration)
    }

    pub fn pragma_update(&self, name: &str, value: impl std::fmt::Display) -> Result<()> {
        self.inner.pragma_update(name, &value.to_string())
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.inner.execute_batch(sql)
    }

    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        self.inner.execute(sql, params.into_params()?)
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        self.inner.prepare(sql).map(Statement)
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
        self.inner.begin()?;
        Ok(Transaction {
            connection: self,
            finished: false,
        })
    }

    pub fn close(self) -> Result<()> {
        self.inner.close()
    }

    /// Backfill before publishing a renamed migration file.
    pub fn checkpoint(&self) -> Result<()> {
        self.inner.checkpoint()
    }
}

pub struct Statement(Box<dyn SqlStatement>);

impl Statement {
    pub fn query(&mut self, params: impl IntoParams) -> Result<Rows> {
        self.0.query(params.into_params()?).map(|inner| Rows {
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
    inner: Box<dyn SqlRows>,
    current: Option<Row>,
}

impl Rows {
    // A fallible lending iterator; std::Iterator cannot express this lifetime.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<&Row>> {
        self.current = self.inner.next()?;
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
        let result = self.connection.inner.commit();
        // An unknown acknowledgement does not authorize rollback or retry.
        self.finished = result.is_ok() || matches!(result, Err(Error::CommitUnknown));
        result
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.connection.inner.rollback();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct EchoDriver;
    #[derive(Default)]
    struct EchoConnection {
        unknown_commit: bool,
        rolled_back: Arc<AtomicBool>,
    }
    struct EchoStatement;
    struct EchoRows(Option<Row>);

    fn capabilities() -> SqlCapabilities {
        SqlCapabilities {
            dialect: SqlDialect::Sqlite,
            write_isolation: WriteIsolation::ImmediateSerializable,
            atomic_transactions: true,
            durable_commits: false,
            multiprocess_writers: false,
        }
    }
    impl SqlDriver for EchoDriver {
        fn capabilities(&self, _: &Path) -> SqlCapabilities {
            capabilities()
        }
        fn open(&self, _: &Path, _: OpenOptions) -> Result<Box<dyn SqlConnection>> {
            Ok(Box::new(EchoConnection::default()))
        }
    }
    impl SqlConnection for EchoConnection {
        fn capabilities(&self) -> SqlCapabilities {
            capabilities()
        }
        fn busy_timeout(&self, _: Duration) -> Result<()> {
            Ok(())
        }
        fn pragma_update(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn execute_batch(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn execute(&self, _: &str, params: Params) -> Result<usize> {
            Ok(params.0.len())
        }
        fn prepare(&self, _: &str) -> Result<Box<dyn SqlStatement>> {
            Ok(Box::new(EchoStatement))
        }
        fn begin(&self) -> Result<()> {
            Ok(())
        }
        fn commit(&self) -> Result<()> {
            if self.unknown_commit {
                Err(Error::CommitUnknown)
            } else {
                Ok(())
            }
        }
        fn rollback(&self) -> Result<()> {
            self.rolled_back.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn checkpoint(&self) -> Result<()> {
            Ok(())
        }
        fn close(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }
    impl SqlStatement for EchoStatement {
        fn query(&mut self, params: Params) -> Result<Box<dyn SqlRows>> {
            Ok(Box::new(EchoRows(Some(Row(params.0)))))
        }
    }
    impl SqlRows for EchoRows {
        fn next(&mut self) -> Result<Option<Row>> {
            Ok(self.0.take())
        }
    }

    #[test]
    fn injected_driver_uses_owned_values_and_streams() {
        let connection = Connection::open_with_driver(
            &EchoDriver,
            Path::new("unused"),
            OpenOptions::default(),
            SqlRequirements::EPHEMERAL,
        )
        .unwrap();
        assert_eq!(
            connection
                .execute("ignored", params![1, "two", None::<String>])
                .unwrap(),
            3
        );
        let mut statement = connection.prepare("ignored").unwrap();
        let mut rows = statement
            .query(params![7, "hello", vec![0_u8, 255], None::<i64>])
            .unwrap();
        let row = rows.next().unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 7);
        assert_eq!(row.get::<String>(1).unwrap(), "hello");
        assert_eq!(row.get::<Vec<u8>>(2).unwrap(), vec![0, 255]);
        assert_eq!(row.get::<Option<i64>>(3).unwrap(), None);
        assert!(rows.next().unwrap().is_none());
        assert!(connection.execute("ignored", params![u64::MAX]).is_err());
    }

    struct UnsupportedDriver(Arc<AtomicBool>);
    impl SqlDriver for UnsupportedDriver {
        fn capabilities(&self, _: &Path) -> SqlCapabilities {
            capabilities()
        }
        fn open(&self, _: &Path, _: OpenOptions) -> Result<Box<dyn SqlConnection>> {
            self.0.store(true, Ordering::Relaxed);
            Ok(Box::new(EchoConnection::default()))
        }
    }
    #[test]
    fn capability_rejection_precedes_open_and_checks_connection() {
        let opened = Arc::new(AtomicBool::new(false));
        let driver = UnsupportedDriver(opened.clone());
        assert!(matches!(
            Connection::open_with_driver(
                &driver,
                Path::new("unused"),
                OpenOptions::default(),
                SqlRequirements::DURABLE
            ),
            Err(Error::UnsupportedCapability(_))
        ));
        assert!(!opened.load(Ordering::Relaxed));
        assert!(matches!(
            Connection::from_driver(
                Box::new(EchoConnection::default()),
                SqlRequirements::DURABLE
            ),
            Err(Error::UnsupportedCapability(_))
        ));
        for invalid in [
            SqlCapabilities {
                dialect: SqlDialect::Other,
                ..capabilities()
            },
            SqlCapabilities {
                write_isolation: WriteIsolation::Other,
                ..capabilities()
            },
            SqlCapabilities {
                atomic_transactions: false,
                ..capabilities()
            },
        ] {
            assert!(invalid.validate(SqlRequirements::EPHEMERAL).is_err());
        }
        assert!(
            capabilities()
                .validate(SqlRequirements {
                    multiprocess_writers: true,
                    ..SqlRequirements::EPHEMERAL
                })
                .is_err()
        );
    }

    #[test]
    fn unknown_commit_does_not_trigger_rollback() {
        let rolled_back = Arc::new(AtomicBool::new(false));
        let connection = Connection::from_driver(
            Box::new(EchoConnection {
                unknown_commit: true,
                rolled_back: rolled_back.clone(),
            }),
            SqlRequirements::EPHEMERAL,
        )
        .unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        assert!(matches!(transaction.commit(), Err(Error::CommitUnknown)));
        assert!(!rolled_back.load(Ordering::Relaxed));
    }

    #[test]
    fn closing_connection_rejects_retained_statement() {
        let connection = Connection::open_in_memory().unwrap();
        let mut statement = connection.prepare("SELECT 1").unwrap();
        connection.close().unwrap();
        assert!(matches!(statement.query(()), Err(Error::Closed)));
    }

    #[test]
    fn injected_sqlite_rolls_back_whole_transaction_on_drop() {
        let connection = Connection::open_with_driver(
            &drivers::sqlite::SqliteDriver,
            Path::new(":memory:"),
            OpenOptions::default(),
            SqlRequirements::EPHEMERAL,
        )
        .unwrap();
        connection
            .execute_batch("CREATE TABLE mutations (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        {
            let transaction = connection.unchecked_transaction().unwrap();
            transaction
                .execute("INSERT INTO mutations VALUES (?1, ?2)", params![1, "first"])
                .unwrap();
            assert!(connection.unchecked_transaction().is_err());
            assert!(
                transaction
                    .execute(
                        "INSERT INTO mutations VALUES (?1, ?2)",
                        params![1, "conflict"]
                    )
                    .is_err()
            );
        }
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM mutations", (), |row| row
                    .get::<i64>(0))
                .unwrap(),
            0
        );
        let transaction = connection.unchecked_transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO mutations VALUES (?1, ?2)",
                params![2, "committed"],
            )
            .unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT value FROM mutations", (), |row| row
                    .get::<String>(0))
                .unwrap(),
            "committed"
        );
    }
}
