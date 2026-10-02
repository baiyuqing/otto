//! The reflection database, `~/.otto/reflection.db`.
//!
//! Holds one row per run and one watermark per session: the id of the last
//! transcript entry a completed run covered. Rows are appended and never
//! rewritten; only the watermark is replaced, and only after a run completes,
//! so a failed or canceled run is covered again by the next one.
//!
//! The database never stores transcript text, only ids, ranges, counts, and
//! statuses.
//!
//! Ownership and concurrency: the store owns one connection behind a mutex;
//! every method takes `&self` and holds the lock for one statement or one
//! transaction.
//!
//! Errors: a failure carries no SQLite text, paths, or row values.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// The schema version this build writes and expects. A database with another
/// version is left untouched and reported as [`Error::UnknownVersion`].
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    trigger TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('ok','noop','failed','canceled')),
    from_entry TEXT NOT NULL,
    to_entry TEXT NOT NULL,
    input_bytes INTEGER NOT NULL CHECK (input_bytes >= 0),
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    memories_proposed INTEGER NOT NULL CHECK (memories_proposed >= 0),
    memories_dropped INTEGER NOT NULL CHECK (memories_dropped >= 0),
    detail TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id, started_at);
CREATE TABLE IF NOT EXISTS watermarks (
    session_id TEXT PRIMARY KEY,
    entry_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Io,
    /// `PRAGMA user_version` did not match this build's schema.
    UnknownVersion,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => formatter.write_str("reflection store unavailable"),
            Self::UnknownVersion => {
                formatter.write_str("reflection database has an unrecognized schema version")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The run asked the model and applied its output.
    Ok,
    /// There was nothing to reflect on; no model call was made.
    Noop,
    Failed,
    Canceled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Noop => "noop",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

/// One finished run, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: String,
    pub session_id: String,
    pub trigger: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: Status,
    pub from_entry: String,
    pub to_entry: String,
    pub input_bytes: usize,
    pub truncated: bool,
    pub memories_proposed: usize,
    pub memories_dropped: usize,
    /// A short, non-sensitive note such as the failure category.
    pub detail: String,
}

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error::Io)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::Io)?;
        }
        let connection = Connection::open_with_flags(
            filename,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| Error::Io)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Io)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Io)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|_| Error::Io)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error::Io)?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| Error::Io)?;
        if version == 0 {
            connection.execute_batch(SCHEMA).map_err(|_| Error::Io)?;
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|_| Error::Io)?;
        } else if version != SCHEMA_VERSION {
            return Err(Error::UnknownVersion);
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| Error::Io)
    }

    /// The id of the last entry a completed run covered for `session_id`.
    pub fn watermark(&self, session_id: &str) -> Result<Option<String>> {
        self.lock()?
            .query_row(
                "SELECT entry_id FROM watermarks WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| Error::Io)
    }

    /// Appends `row`. A run that completed (`Ok` or `Noop`) also advances the
    /// session's watermark to `row.to_entry`, in the same transaction, when
    /// that id is non-empty. Failed and canceled runs leave it in place.
    pub fn record(&self, row: &RunRow) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(|_| Error::Io)?;
        transaction
            .execute(
                "INSERT INTO runs (id, session_id, trigger, started_at, finished_at, status, \
                 from_entry, to_entry, input_bytes, truncated, memories_proposed, \
                 memories_dropped, detail) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    row.id,
                    row.session_id,
                    row.trigger,
                    row.started_at,
                    row.finished_at,
                    row.status.as_str(),
                    row.from_entry,
                    row.to_entry,
                    row.input_bytes as i64,
                    i64::from(row.truncated),
                    row.memories_proposed as i64,
                    row.memories_dropped as i64,
                    row.detail,
                ],
            )
            .map_err(|_| Error::Io)?;
        if matches!(row.status, Status::Ok | Status::Noop) && !row.to_entry.is_empty() {
            transaction
                .execute(
                    "INSERT INTO watermarks (session_id, entry_id, updated_at) VALUES (?1,?2,?3) \
                     ON CONFLICT(session_id) DO UPDATE SET entry_id = excluded.entry_id, \
                     updated_at = excluded.updated_at",
                    params![row.session_id, row.to_entry, row.finished_at],
                )
                .map_err(|_| Error::Io)?;
        }
        transaction.commit().map_err(|_| Error::Io)
    }

    /// The runs recorded for `session_id`, oldest first.
    #[cfg(test)]
    pub fn runs(&self, session_id: &str) -> Result<Vec<(String, Status)>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT id, status FROM runs WHERE session_id = ?1 ORDER BY started_at, id")
            .map_err(|_| Error::Io)?;
        let rows = statement
            .query_map(params![session_id], |row| {
                let status: String = row.get(1)?;
                Ok((
                    row.get::<_, String>(0)?,
                    match status.as_str() {
                        "ok" => Status::Ok,
                        "noop" => Status::Noop,
                        "canceled" => Status::Canceled,
                        _ => Status::Failed,
                    },
                ))
            })
            .map_err(|_| Error::Io)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: Status, to_entry: &str) -> RunRow {
        RunRow {
            id: id.into(),
            session_id: "session-1".into(),
            trigger: "manual".into(),
            started_at: format!("2026-10-02T00:00:0{}Z", id.len() % 10),
            finished_at: "2026-10-02T00:01:00Z".into(),
            status,
            from_entry: String::new(),
            to_entry: to_entry.into(),
            input_bytes: 10,
            truncated: false,
            memories_proposed: 1,
            memories_dropped: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn a_completed_run_advances_the_watermark() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.watermark("session-1").expect("read"), None);
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        store
            .record(&row("bb", Status::Noop, "bbbbbbbb"))
            .expect("noop");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("bbbbbbbb")
        );
    }

    #[test]
    fn failed_and_canceled_runs_leave_the_watermark_in_place() {
        let store = Store::open_in_memory().expect("open");
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        store
            .record(&row("bb", Status::Failed, "bbbbbbbb"))
            .expect("failed");
        store
            .record(&row("ccc", Status::Canceled, "cccccccc"))
            .expect("canceled");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        assert_eq!(store.runs("session-1").expect("runs").len(), 3);
    }

    #[test]
    fn watermarks_are_per_session() {
        let store = Store::open_in_memory().expect("open");
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        assert_eq!(store.watermark("session-2").expect("read"), None);
    }

    #[test]
    fn an_unrecognized_schema_version_is_refused() {
        let connection = Connection::open_in_memory().expect("open");
        connection
            .pragma_update(None, "user_version", 999)
            .expect("set version");
        assert_eq!(
            Store::initialize(connection).expect_err("version"),
            Error::UnknownVersion
        );
    }

    #[test]
    fn a_file_store_reopens_with_its_rows_and_private_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("reflection.db");
        Store::open(&path)
            .expect("open")
            .record(&row("a", Status::Ok, "aaaaaaaa"))
            .expect("record");
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        let mode =
            |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().expect("parent")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }
}
