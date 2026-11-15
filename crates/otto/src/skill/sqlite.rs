//! SQLite implementation of the skill-check repository.
use crate::skill::check_repository::{
    CheckStore, NewRow, ResultJson, Row, StoreError, StoreResult,
};
use otto_storage::{Connection, params};
#[cfg(any(test, feature = "test-support"))]
use otto_storage::{OpenOptions, drivers::sqlite::SqliteDriver};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const SCHEMA_VERSION: i64 = 1;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS skill_checks (
    id INTEGER PRIMARY KEY,
    content_sha256 TEXT NOT NULL,
    questions_sha256 TEXT NOT NULL,
    skill TEXT NOT NULL,
    path TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('rules','typesafe')),
    model TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    result TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS skill_checks_key
ON skill_checks(content_sha256, questions_sha256, id);
"#;

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}
impl Store {
    /// Opens the store at `filename` through the default SQLite driver.
    /// Production code receives its connection from the storage registry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open(filename: &std::path::Path) -> StoreResult<Self> {
        let connection =
            Connection::open_with_driver(&SqliteDriver, filename, OpenOptions::default())
                .map_err(|_| StoreError::Io)?;
        Self::from_connection(connection)
    }
    pub(crate) fn from_connection(connection: Connection) -> StoreResult<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| StoreError::Io)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| StoreError::Io)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> StoreResult<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| StoreError::Io)?)
    }

    pub(crate) fn initialize(connection: Connection) -> StoreResult<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| StoreError::Io)?;
        if version == 0 {
            connection
                .execute_batch(SCHEMA)
                .map_err(|_| StoreError::Io)?;
            connection
                .pragma_update("user_version", SCHEMA_VERSION)
                .map_err(|_| StoreError::Io)?;
        } else if version != SCHEMA_VERSION {
            return Err(StoreError::UnknownVersion);
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn append(&self, row: &NewRow) -> StoreResult<()> {
        let result = serde_json::to_string(row.result).map_err(|_| StoreError::Io)?;
        self.connection
            .lock()
            .map_err(|_| StoreError::Io)?
            .execute(
                "INSERT INTO skill_checks (
                    content_sha256, questions_sha256, skill, path, source, model, checked_at, result
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.content_sha256,
                    row.questions_sha256,
                    row.skill,
                    row.path,
                    row.source,
                    row.model,
                    row.checked_at,
                    result,
                ],
            )
            .map_err(|_| StoreError::Io)?;
        Ok(())
    }

    /// The row with the largest id for this key, if any.
    pub(crate) fn latest(
        &self,
        content_sha256: &str,
        questions_sha256: &str,
    ) -> StoreResult<Option<Row>> {
        let connection = self.connection.lock().map_err(|_| StoreError::Io)?;
        let mut statement = connection
            .prepare(
                "SELECT source, model, checked_at, result FROM skill_checks
                 WHERE content_sha256 = ?1 AND questions_sha256 = ?2
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|_| StoreError::Io)?;
        let mut rows = statement
            .query(params![content_sha256, questions_sha256])
            .map_err(|_| StoreError::Io)?;
        let Some(row) = rows.next().map_err(|_| StoreError::Io)? else {
            return Ok(None);
        };
        let source: String = row.get(0).map_err(|_| StoreError::Io)?;
        let model: String = row.get(1).map_err(|_| StoreError::Io)?;
        let checked_at: String = row.get(2).map_err(|_| StoreError::Io)?;
        let result_text: String = row.get(3).map_err(|_| StoreError::Io)?;
        let result: ResultJson = serde_json::from_str(&result_text).map_err(|_| StoreError::Io)?;
        Ok(Some(Row {
            source,
            model,
            checked_at,
            result,
        }))
    }
}
impl CheckStore for Store {
    fn append(&self, row: &NewRow<'_>) -> StoreResult<()> {
        Store::append(self, row)
    }
    fn latest(&self, content_sha256: &str, questions_sha256: &str) -> StoreResult<Option<Row>> {
        Store::latest(self, content_sha256, questions_sha256)
    }
}
