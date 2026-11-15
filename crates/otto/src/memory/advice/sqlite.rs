//! SQLite implementation of the memory-advice audit repository.
use crate::memory::advice::audit::{AuditStore, Error, NewRow};
use crate::storage::drivers::sqlite::SqliteDriver;
use crate::storage::{Connection, params};
use crate::storage::{OpenOptions, SqlDriver};
use chrono::{SecondsFormat, Utc};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// The schema version this build writes and expects. A database with another
/// version is left untouched and reported as [`Error::UnknownVersion`].
pub(super) const SCHEMA_VERSION: i64 = 1;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memory_advice (
    id INTEGER PRIMARY KEY,
    created_at TEXT NOT NULL,
    model TEXT NOT NULL,
    auxiliary INTEGER NOT NULL CHECK (auxiliary IN (0,1)),
    candidate_id TEXT,
    scope TEXT NOT NULL,
    request TEXT NOT NULL,
    response TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('ok','timeout','error','invalid')),
    error TEXT,
    latency_ms INTEGER NOT NULL
) STRICT;
"#;

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}
impl Store {
    pub fn open(filename: &Path) -> Result<Self, Error> {
        Self::open_with_driver(&SqliteDriver, filename)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, filename: &Path) -> Result<Self, Error> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error::Io)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::Io)?;
        }
        let connection = Connection::open_with_driver(driver, filename, OpenOptions::default())
            .map_err(|_| Error::Io)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Io)?;
        Self::from_connection(connection)
    }

    pub(crate) fn from_connection(connection: Connection) -> Result<Self, Error> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Io)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| Error::Io)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, Error> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error::Io)?)
    }

    fn initialize(connection: Connection) -> Result<Self, Error> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| Error::Io)?;
        if version == 0 {
            connection.execute_batch(SCHEMA).map_err(|_| Error::Io)?;
            connection
                .pragma_update("user_version", SCHEMA_VERSION)
                .map_err(|_| Error::Io)?;
        } else if version != SCHEMA_VERSION {
            return Err(Error::UnknownVersion);
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Appends one row; `candidate_id` is `None` when the proposal failed.
    pub fn append(&self, row: &NewRow, candidate_id: Option<&str>) -> Result<(), Error> {
        self.connection
            .lock()
            .map_err(|_| Error::Io)?
            .execute(
                "INSERT INTO memory_advice (
                    created_at, model, auxiliary, candidate_id, scope, request,
                    response, outcome, error, latency_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    row.model,
                    i64::from(row.auxiliary),
                    candidate_id.map(str::to_owned),
                    row.scope,
                    row.request,
                    row.response.clone(),
                    row.outcome,
                    row.error.clone(),
                    row.latency_ms,
                ],
            )
            .map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Drops the table so every later [`Self::append`] fails.
    #[cfg(test)]
    pub(crate) fn break_for_test(&self) {
        self.connection
            .lock()
            .expect("audit lock")
            .execute_batch("DROP TABLE memory_advice")
            .expect("drop");
    }

    /// Every row, oldest first, as `(candidate_id, outcome, request,
    /// response, error)`.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> Vec<AuditRow> {
        let connection = self.connection.lock().expect("audit lock");
        let mut statement = connection
            .prepare(
                "SELECT candidate_id, outcome, request, response, error, model, auxiliary, scope
                 FROM memory_advice ORDER BY id",
            )
            .expect("prepare");
        let mut rows = statement.query(()).expect("query");
        let mut out = Vec::new();
        while let Some(row) = rows.next().expect("row") {
            out.push(AuditRow {
                candidate_id: row.get(0).expect("candidate_id"),
                outcome: row.get(1).expect("outcome"),
                request: row.get(2).expect("request"),
                response: row.get(3).expect("response"),
                error: row.get(4).expect("error"),
                model: row.get(5).expect("model"),
                auxiliary: row.get::<i64>(6).expect("auxiliary") == 1,
                scope: row.get(7).expect("scope"),
            });
        }
        out
    }
}

impl AuditStore for Store {
    fn append(&self, row: &NewRow, candidate_id: Option<&str>) -> Result<(), Error> {
        Store::append(self, row, candidate_id)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AuditRow {
    pub candidate_id: Option<String>,
    pub outcome: String,
    pub request: String,
    pub response: Option<String>,
    pub error: Option<String>,
    pub model: String,
    pub auxiliary: bool,
    pub scope: String,
}
