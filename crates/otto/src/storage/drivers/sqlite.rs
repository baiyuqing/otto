//! SQLite local I/O through `rusqlite` (bundled SQLite). No Tokio runtime is
//! created or entered.
//!
//! One `rusqlite::Connection` sits behind a mutex in [`ConnectionState`].
//! Query results stream through [`row_stream::SqliteRows`], which owns a
//! prepared statement and takes that mutex for one `sqlite3_step` at a time,
//! so other statements can run on the connection while rows are open.
use crate::storage::{
    Error, OpenOptions, OpenPolicy, Params, Result, SqlConnection, SqlDriver, SqlRows,
    SqlStatement, Value,
};
use rusqlite::OpenFlags;
use std::path::Path;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

mod row_stream;
#[cfg(test)]
mod row_stream_tests;
mod statement_boundary;
#[cfg(test)]
mod statement_boundary_tests;

/// SQLITE_BUSY_SNAPSHOT, SQLITE_CONSTRAINT_PRIMARYKEY and SQLITE_CONSTRAINT_UNIQUE.
const BUSY_SNAPSHOT: i32 = 517;
const CONSTRAINT_PRIMARYKEY: i32 = 1555;
const CONSTRAINT_UNIQUE: i32 = 2067;

#[derive(Debug, Default)]
pub struct SqliteDriver;
impl SqlDriver for SqliteDriver {
    fn open(&self, path: &Path, options: OpenOptions) -> Result<Box<dyn SqlConnection>> {
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
        let flags = if options.read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else if options.policy == OpenPolicy::Existing {
            // No SQLITE_OPEN_CREATE: SQLite itself refuses to create the main file.
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let inner = rusqlite::Connection::open_with_flags(path, flags).map_err(map_error)?;
        if !options.read_only {
            if path != ":memory:" {
                let mode: String = inner
                    .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                    .map_err(map_error)?;
                if !mode.eq_ignore_ascii_case("wal") {
                    return Err(Error::Unavailable);
                }
            }
            inner
                .execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON")
                .map_err(map_error)?;
        }
        Ok(Box::new(SqliteConnection {
            state: Arc::new(ConnectionState {
                connection: Mutex::new(inner),
                poisoned: AtomicBool::new(false),
            }),
        }))
    }
}

/// Connection shared by the connection handle, its statements and open rows.
/// The `rusqlite::Connection` closes when the last of them is dropped, so rows
/// never outlive the database handle they were prepared on.
struct ConnectionState {
    connection: Mutex<rusqlite::Connection>,
    /// Set by `close`, a failed `rollback` and an uncertain `commit`.
    poisoned: AtomicBool,
}
type SharedState = Arc<ConnectionState>;

fn lock(state: &ConnectionState) -> Result<MutexGuard<'_, rusqlite::Connection>> {
    state.connection.lock().map_err(|_| Error::Unavailable)
}

struct SqliteConnection {
    state: SharedState,
}
impl SqliteConnection {
    fn guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>> {
        check(&self.state.poisoned)?;
        lock(&self.state)
    }
}
fn check(poisoned: &AtomicBool) -> Result<()> {
    if poisoned.load(Ordering::Acquire) {
        Err(Error::Closed)
    } else {
        Ok(())
    }
}
impl SqlConnection for SqliteConnection {
    fn busy_timeout(&self, duration: Duration) -> Result<()> {
        self.guard()?.busy_timeout(duration).map_err(map_error)
    }
    fn pragma_update(&self, name: &str, value: &str) -> Result<()> {
        self.guard()?
            .pragma_update(None, name, value)
            .map_err(map_error)
    }
    fn execute_batch(&self, sql: &str) -> Result<()> {
        self.guard()?.execute_batch(sql).map_err(map_error)
    }
    fn execute(&self, sql: &str, params: Params) -> Result<usize> {
        check(&self.state.poisoned)?;
        statement_boundary::validate(sql)?;
        let connection = self.guard()?;
        let mut statement = connection.prepare_cached(sql).map_err(map_error)?;
        statement
            .execute(rusqlite::params_from_iter(parameters(params)))
            .map_err(map_error)
    }
    fn prepare(&self, sql: &str) -> Result<Box<dyn SqlStatement>> {
        check(&self.state.poisoned)?;
        statement_boundary::validate(sql)?;
        // Compile once now so syntax errors surface at prepare time, but only
        // after scratch validation has rejected scripts without live effects.
        let connection = self.guard()?;
        connection.prepare(sql).map_err(map_error)?;
        Ok(Box::new(SqliteStatement {
            state: self.state.clone(),
            sql: sql.to_string(),
        }))
    }
    fn begin(&self) -> Result<()> {
        self.execute_batch("BEGIN IMMEDIATE")
    }
    fn commit(&self) -> Result<()> {
        match self.guard()?.execute_batch("COMMIT") {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    map_error_ref(&error),
                    Error::Busy(_) | Error::BusySnapshot(_) | Error::Constraint(_)
                ) =>
            {
                Err(map_error(error))
            }
            Err(_) => {
                self.state.poisoned.store(true, Ordering::Release);
                Err(Error::CommitUnknown)
            }
        }
    }
    fn rollback(&self) -> Result<()> {
        let result = self.guard()?.execute_batch("ROLLBACK").map_err(map_error);
        if result.is_err() {
            self.state.poisoned.store(true, Ordering::Release);
        }
        result
    }
    fn checkpoint(&self) -> Result<()> {
        let busy: i64 = self
            .guard()?
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(map_error)?;
        if busy != 0 {
            return Err(Error::Busy("checkpoint could not finish".into()));
        }
        Ok(())
    }
    fn close(self: Box<Self>) -> Result<()> {
        self.state.poisoned.store(true, Ordering::Release);
        drop(self);
        Ok(())
    }
}

/// The shared connection and the SQL text. Each `query` prepares the SQL
/// again and returns rows that own their statement (see `row_stream`);
/// `prepare` already compiled it once, so syntax errors surface there.
struct SqliteStatement {
    state: SharedState,
    sql: String,
}
impl SqlStatement for SqliteStatement {
    fn query(&mut self, params: Params) -> Result<Box<dyn SqlRows>> {
        row_stream::query(&self.state, &self.sql, params)
    }
}
fn parameters(params: Params) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value as Sql;
    params
        .0
        .into_iter()
        .map(|value| match value {
            Value::Null => Sql::Null,
            Value::Integer(value) => Sql::Integer(value),
            Value::Real(value) => Sql::Real(value),
            Value::Text(value) => Sql::Text(value),
            Value::Blob(value) => Sql::Blob(value),
        })
        .collect()
}
fn map_error(error: rusqlite::Error) -> Error {
    map_error_ref(&error)
}
fn map_error_ref(error: &rusqlite::Error) -> Error {
    use rusqlite::ErrorCode;
    // Never let SQL text, bound values or an SDK error cross this boundary.
    match error {
        rusqlite::Error::QueryReturnedNoRows => Error::QueryReturnedNoRows,
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::Utf8Error(..) => {
            Error::ConversionFailure("column conversion failed".into())
        }
        rusqlite::Error::ToSqlConversionFailure(_) => {
            Error::ToSqlConversionFailure("invalid SQL parameter".into())
        }
        rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
            ErrorCode::DatabaseBusy if failure.extended_code == BUSY_SNAPSHOT => {
                Error::BusySnapshot("snapshot busy".into())
            }
            ErrorCode::DatabaseBusy => Error::Busy("writer busy".into()),
            ErrorCode::ApiMisuse => Error::Misuse("invalid connection operation".into()),
            ErrorCode::ConstraintViolation => {
                // Existing repositories distinguish duplicate identity from other
                // constraints. Preserve the category, not names or record data.
                let category = match failure.extended_code {
                    CONSTRAINT_UNIQUE => "UNIQUE constraint failed",
                    CONSTRAINT_PRIMARYKEY => "PRIMARY KEY constraint failed",
                    _ => "constraint failed",
                };
                Error::Constraint(category.into())
            }
            ErrorCode::DatabaseCorrupt => Error::Corrupt("corrupt database".into()),
            ErrorCode::NotADatabase => Error::NotAdb("invalid database".into()),
            _ => Error::Unavailable,
        },
        _ => Error::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_policy_on_missing_path_fails_and_creates_no_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.db");
        let options = OpenOptions {
            read_only: false,
            policy: OpenPolicy::Existing,
        };
        assert!(SqliteDriver.open(&path, options).is_err());
        assert!(!path.exists());
        assert!(!directory.path().join("missing.db-wal").exists());
    }
}
