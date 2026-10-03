//! The local Turso memory store.
//!
//! One mutex guards a local connection; logical imports preserve records and
//! identity while rebuilding the native search index.

pub mod candidates;
pub mod codec;
pub mod cursor;
pub mod query;
pub mod records;
pub mod retriever;
pub mod schema;

use std::sync::Mutex;
use std::time::Duration;

use crate::storage::Connection;

use super::guard::ContentGuard;
use super::{Error, ErrorKind, MAX_DUPLICATE_ID_RETRIES, Result, Scope, StoreIdentity, new_id};

const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Driver diagnostics can contain row data; only domain error kinds escape.
pub fn map_storage_error(error: crate::storage::Error) -> Error {
    use crate::storage::Error as DriverError;
    let kind = match error {
        DriverError::Busy(_) | DriverError::BusySnapshot(_) => ErrorKind::Busy,
        DriverError::Constraint(message) => {
            if message.starts_with("UNIQUE constraint failed")
                || message.starts_with("PRIMARY KEY constraint failed")
            {
                ErrorKind::Conflict
            } else {
                ErrorKind::Corrupt
            }
        }
        DriverError::Corrupt(_) | DriverError::NotAdb(_) => ErrorKind::Corrupt,
        DriverError::Misuse(_) => ErrorKind::Closed,
        _ => ErrorKind::Unavailable,
    };
    Error::new(kind)
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
/// callers serialize. Turso's busy timeout still covers contention with the
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
        let connection = Connection::open(filename).map_err(map_storage_error)?;
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
            .map_err(map_storage_error)?;
        let outcome = schema::initialize_schema(&connection, &ids[0], &ids[1]);
        let finish = match outcome {
            Ok(()) => connection
                .execute_batch("COMMIT")
                .map_err(map_storage_error),
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
            Some(connection) => connection.close().map_err(map_storage_error),
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
            .map_err(map_storage_error)?;
        match body(connection) {
            Ok((value, generation)) => {
                connection
                    .execute_batch("COMMIT")
                    .map_err(map_storage_error)?;
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
    connection
        .busy_timeout(busy_timeout)
        .map_err(map_storage_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON")
        .map_err(map_storage_error)?;
    let enabled: i64 = connection
        .query_row("PRAGMA foreign_keys", (), |row| row.get(0))
        .map_err(map_storage_error)?;
    if enabled != 1 {
        return Err(Error::new(ErrorKind::Unsupported));
    }
    Ok(())
}

/// Refuse missing, duplicate, tombstoned, or stale search copies at open time.
fn verify_fts_integrity(connection: &Connection) -> Result<()> {
    let invalid: i64 = connection.query_row(
        "SELECT count(*) FROM (
            SELECT r.id FROM memory_records r LEFT JOIN memory_records_fts f ON f.record_id=r.id
            WHERE r.state='active' GROUP BY r.id
            HAVING count(f.record_id)<>1 OR min(f.text_value)<>min(r.text_value)
                OR min(f.kind)<>min(r.kind) OR min(f.semantic_key)<>min(r.semantic_key)
            UNION ALL
            SELECT f.record_id FROM memory_records_fts f LEFT JOIN memory_records r ON f.record_id=r.id
            WHERE r.id IS NULL OR r.state<>'active'
        )", (), |row| row.get(0)).map_err(map_storage_error)?;
    if invalid != 0 {
        return Err(Error::new(ErrorKind::Corrupt));
    }
    let mut statement = connection.prepare(
        "SELECT r.labels_json,f.labels FROM memory_records r JOIN memory_records_fts f ON f.record_id=r.id WHERE r.state='active'"
    ).map_err(map_storage_error)?;
    let mut rows = statement.query(()).map_err(map_storage_error)?;
    while let Some(row) = rows.next().map_err(map_storage_error)? {
        let json: String = row.get(0).map_err(map_storage_error)?;
        let labels: Vec<String> =
            serde_json::from_str(&json).map_err(|_| Error::new(ErrorKind::Corrupt))?;
        let indexed: String = row.get(1).map_err(map_storage_error)?;
        if indexed != codec::fts_labels(&labels) {
            return Err(Error::new(ErrorKind::Corrupt));
        }
    }
    Ok(())
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
    fn driver_error_details_never_escape_the_domain_boundary() {
        for (driver, expected) in [
            (
                crate::storage::Error::Constraint("UNIQUE constraint failed: secret".into()),
                ErrorKind::Conflict,
            ),
            (
                crate::storage::Error::Constraint("CHECK constraint failed: secret".into()),
                ErrorKind::Corrupt,
            ),
            (
                crate::storage::Error::Busy("secret".into()),
                ErrorKind::Busy,
            ),
            (
                crate::storage::Error::NotAdb("secret".into()),
                ErrorKind::Corrupt,
            ),
        ] {
            let error = map_storage_error(driver);
            assert!(error.is(expected));
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }

    #[test]
    fn opening_rejects_a_missing_search_copy() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("memory.db");
        let store = Store::open(&path, Options::default()).expect("open");
        let scope = store.identity().expect("identity").user_scope;
        store
            .upsert(&crate::memory::UpsertRequest {
                record: records::testsupport::sample_record(
                    "rec-1",
                    &scope,
                    "editor",
                    "uses neovim",
                ),
                expected_revision: None,
            })
            .expect("upsert");
        store.close().expect("close");
        let connection = Connection::open(&path).expect("open database");
        connection
            .execute("DELETE FROM memory_records_fts", ())
            .expect("remove copy");
        connection.close().expect("close database");
        let error = match Store::open(&path, Options::default()) {
            Ok(_) => panic!("must reject"),
            Err(error) => error,
        };
        assert!(error.is(ErrorKind::Corrupt));
    }

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
