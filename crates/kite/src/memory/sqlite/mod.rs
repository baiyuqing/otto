//! The SQLite FTS5 memory store.
//!
//! An existing database file written by the previously released binary still
//! opens, so the schema, the pragmas, the timestamp format and every stored
//! JSON blob are byte-compatible rather than merely equivalent.
//!
//! Hardening left out, none of it on the wire: one mutex-guarded connection
//! rather than a four-connection retained pool, no file-descriptor delta proofs
//! around driver opens, no inode retention for the database path, no poisoning
//! or quarantine state machine, and no retry-backoff loop above SQLite's own
//! `busy_timeout`.

pub mod candidates;
pub mod codec;
pub mod cursor;
pub mod query;
pub mod records;
pub mod retriever;
pub mod schema;

use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::guard::ContentGuard;
use super::{Error, ErrorKind, MAX_DUPLICATE_ID_RETRIES, Result, Scope, StoreIdentity, new_id};

const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLite extended result codes the store distinguishes.
const SQLITE_BUSY: i32 = 5;
const SQLITE_LOCKED: i32 = 6;
const SQLITE_CORRUPT: i32 = 11;
const SQLITE_CONSTRAINT: i32 = 19;
const SQLITE_MISUSE: i32 = 21;
const SQLITE_NOTADB: i32 = 26;
const SQLITE_CONSTRAINT_CHECK: i32 = 275;
const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
const SQLITE_CONSTRAINT_NOTNULL: i32 = 1299;
const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1555;
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

/// A driver error never crosses the adapter boundary: it becomes one of the
/// domain errors, so SQLite diagnostics (which can echo row content or file
/// paths) stay inside the store.
pub fn map_sqlite_error(error: rusqlite::Error) -> Error {
    let code = match &error {
        rusqlite::Error::SqliteFailure(failure, _) => failure.extended_code,
        _ => return Error::new(ErrorKind::Unavailable),
    };
    match code {
        SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY => {
            return Error::new(ErrorKind::Conflict);
        }
        SQLITE_CONSTRAINT_CHECK | SQLITE_CONSTRAINT_NOTNULL | SQLITE_CONSTRAINT_FOREIGNKEY => {
            return Error::new(ErrorKind::Corrupt);
        }
        _ => {}
    }
    match code & 0xff {
        SQLITE_BUSY | SQLITE_LOCKED => Error::new(ErrorKind::Busy),
        SQLITE_CORRUPT | SQLITE_NOTADB | SQLITE_CONSTRAINT => Error::new(ErrorKind::Corrupt),
        SQLITE_MISUSE => Error::new(ErrorKind::Closed),
        _ => Error::new(ErrorKind::Unavailable),
    }
}

/// How a [`Store`] is opened.
pub struct Options {
    pub busy_timeout: Duration,
    pub new_id: Box<dyn Fn() -> Result<String> + Send + Sync>,
    pub guard: Box<dyn ContentGuard>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            new_id: Box::new(new_id),
            guard: Box::new(super::guard::DefaultGuard),
        }
    }
}

struct Inner {
    connection: Option<Connection>,
    generation: u64,
}

/// One open memory database.
///
/// Concurrency: every operation takes the connection mutex, so concurrent
/// callers serialize. SQLite's `busy_timeout` still covers contention with the
/// other binary sharing the file.
///
/// ponytail: one global connection lock. Move to a connection pool only if
/// memory operations ever show up as a measured bottleneck.
pub struct Store {
    inner: Mutex<Inner>,
    guard: Box<dyn ContentGuard>,
    new_id: Box<dyn Fn() -> Result<String> + Send + Sync>,
    database_id: String,
    user_scope: Scope,
}

impl Store {
    /// Opens `filename`, creating and initializing it when it does not exist.
    pub fn open(filename: &std::path::Path, options: Options) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error::new(ErrorKind::Unavailable))?;
        }
        let connection = Connection::open_with_flags(
            filename,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_sqlite_error)?;
        configure_connection(&connection, options.busy_timeout)?;

        let ids = super::generate_distinct_ids(2, options.new_id.as_ref())?;
        for id in &ids {
            if !schema::valid_database_id(id) {
                return Err(super::invalid_request("database ID"));
            }
        }
        options.guard.check(&super::GuardInput {
            fields: vec![
                super::GuardField {
                    name: "database ID".into(),
                    value: ids[0].clone(),
                    opaque: true,
                },
                super::GuardField {
                    name: "user scope ID".into(),
                    value: ids[1].clone(),
                    opaque: true,
                },
            ],
        })?;

        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(map_sqlite_error)?;
        let outcome = schema::initialize_schema(&connection, &ids[0], &ids[1]);
        let finish = match outcome {
            Ok(()) => connection.execute_batch("COMMIT").map_err(map_sqlite_error),
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        if let Err(error) = finish {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
        let identity = schema::verify_schema(&connection)?;
        verify_fts_integrity(&connection)?;

        Ok(Self {
            inner: Mutex::new(Inner {
                connection: Some(connection),
                generation: identity.generation,
            }),
            guard: options.guard,
            new_id: options.new_id,
            database_id: identity.database_id,
            user_scope: identity.user_scope,
        })
    }

    pub fn identity(&self) -> Result<StoreIdentity> {
        let inner = self.locked()?;
        Ok(StoreIdentity {
            database_id: self.database_id.clone(),
            user_scope: self.user_scope.clone(),
            schema_version: schema::SCHEMA_VERSION,
            generation: inner.generation,
        })
    }

    pub fn guard(&self) -> &dyn ContentGuard {
        self.guard.as_ref()
    }

    /// Mints an ID that is not already in `taken`.
    pub fn mint_id(&self, taken: &[String]) -> Result<String> {
        for _ in 0..=MAX_DUPLICATE_ID_RETRIES {
            let id = (self.new_id)()?;
            if !taken.contains(&id) {
                return Ok(id);
            }
        }
        Err(Error::new(ErrorKind::Unavailable))
    }

    pub fn close(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        match inner.connection.take() {
            Some(connection) => connection
                .close()
                .map_err(|(_, error)| map_sqlite_error(error)),
            None => Ok(()),
        }
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        if inner.connection.is_none() {
            return Err(Error::new(ErrorKind::Closed));
        }
        Ok(inner)
    }

    /// Runs `body` against the connection without a surrounding transaction.
    pub(crate) fn with_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let inner = self.locked()?;
        let connection = inner.connection.as_ref().expect("checked by locked");
        body(connection)
    }

    /// Runs `body` inside `BEGIN IMMEDIATE`, committing on `Ok` and rolling
    /// back on `Err`. `body` receives the current generation and returns the
    /// value plus the generation to publish.
    pub(crate) fn with_write<T>(
        &self,
        body: impl FnOnce(&Connection) -> Result<(T, u64)>,
    ) -> Result<T> {
        let mut inner = self.locked()?;
        let connection = inner.connection.as_ref().expect("checked by locked");
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(map_sqlite_error)?;
        match body(connection) {
            Ok((value, generation)) => {
                connection
                    .execute_batch("COMMIT")
                    .map_err(map_sqlite_error)?;
                inner.generation = generation;
                Ok(value)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn configure_connection(connection: &Connection, busy_timeout: Duration) -> Result<()> {
    let milliseconds = busy_timeout.as_millis().min(i64::MAX as u128) as i64;
    for statement in [
        "PRAGMA foreign_keys=ON".to_string(),
        "PRAGMA synchronous=FULL".to_string(),
        format!("PRAGMA busy_timeout={milliseconds}"),
        "PRAGMA trusted_schema=OFF".to_string(),
        "PRAGMA writable_schema=OFF".to_string(),
    ] {
        connection
            .execute_batch(&statement)
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
    }
    for (query, want) in [
        ("PRAGMA foreign_keys", 1_i64),
        ("PRAGMA synchronous", 2),
        ("PRAGMA busy_timeout", milliseconds),
        ("PRAGMA trusted_schema", 0),
        ("PRAGMA writable_schema", 0),
    ] {
        let got: i64 = connection
            .query_row(query, [], |row| row.get(0))
            .map_err(|_| Error::new(ErrorKind::Unsupported))?;
        if got != want {
            return Err(Error::new(ErrorKind::Unsupported));
        }
    }
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::new(ErrorKind::Unavailable));
    }
    Ok(())
}

/// A store whose FTS5 index disagrees with its content table would silently
/// lose search results, so the failure is fatal at open time rather than at
/// query time.
fn verify_fts_integrity(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "INSERT INTO memory_records_fts(memory_records_fts) VALUES('integrity-check')",
        )
        .map_err(|_| Error::new(ErrorKind::Corrupt))
}

#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;

    /// Opens a store in a fresh temporary directory, returning both so the
    /// directory outlives the store.
    pub fn open_temp() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&directory.path().join("memory.db"), Options::default())
            .expect("open store");
        (directory, store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_creates_a_verifiable_schema_and_a_distinct_identity() {
        let (_directory, store) = testsupport::open_temp();
        let identity = store.identity().expect("identity");
        assert!(schema::valid_database_id(&identity.database_id));
        assert!(schema::valid_database_id(&identity.user_scope.id));
        assert_ne!(identity.database_id, identity.user_scope.id);
        assert_eq!(identity.schema_version, schema::SCHEMA_VERSION);
        assert_eq!(identity.generation, 0);
        assert_eq!(identity.user_scope.namespace, crate::memory::NAMESPACE_USER);
    }

    #[test]
    fn reopening_verifies_the_existing_schema_and_keeps_the_identity() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("memory.db");
        let first = Store::open(&path, Options::default()).expect("open");
        let identity = first.identity().expect("identity");
        first.close().expect("close");
        let second = Store::open(&path, Options::default()).expect("reopen");
        assert_eq!(second.identity().expect("identity"), identity);
    }

    #[test]
    fn a_closed_store_reports_closed() {
        let (_directory, store) = testsupport::open_temp();
        store.close().expect("close");
        assert!(store.identity().unwrap_err().is(ErrorKind::Closed));
        store.close().expect("closing twice is allowed");
    }

    #[test]
    fn a_file_that_is_not_a_memory_database_is_corrupt() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("memory.db");
        let foreign = Connection::open(&path).expect("create");
        foreign
            .execute_batch("CREATE TABLE other(a TEXT)")
            .expect("create table");
        drop(foreign);
        let error = match Store::open(&path, Options::default()) {
            Ok(_) => panic!("must reject"),
            Err(error) => error,
        };
        assert!(error.is(ErrorKind::Corrupt), "unexpected error: {error:?}");
    }
}
