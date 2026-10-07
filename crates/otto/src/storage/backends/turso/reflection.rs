//! Turso repository for reflection records and skill ownership.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use crate::reflection::repository::ReflectionStore;
use crate::reflection::store::{Error, GeneratedSkill, Result, RunRow, SkillWrite, Status};
use crate::storage::drivers::turso::TursoDriver;
use crate::storage::{Connection, OptionalExtension, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};

/// The schema version this build writes and expects. A database with another
/// version is left untouched and reported as [`Error::UnknownVersion`].
pub(super) const SCHEMA_VERSION: i64 = 2;

pub(super) const SCHEMA: &str = r#"
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
    detail TEXT NOT NULL,
    skills_written INTEGER NOT NULL DEFAULT 0 CHECK (skills_written >= 0)
) STRICT;
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id, started_at);
CREATE TABLE IF NOT EXISTS watermarks (
    session_id TEXT PRIMARY KEY,
    entry_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS generated_skills (
    name TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    run_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS skill_versions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    hash TEXT NOT NULL,
    run_id TEXT NOT NULL,
    saved_at TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS skill_versions_name ON skill_versions(name, id);
"#;

/// What a version 1 database lacks: the skill ownership tables and the
/// per-run skill count.
const MIGRATE_1_TO_2: &str = r#"
ALTER TABLE runs ADD COLUMN skills_written INTEGER NOT NULL DEFAULT 0 CHECK (skills_written >= 0);
CREATE TABLE IF NOT EXISTS generated_skills (
    name TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    run_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS skill_versions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    hash TEXT NOT NULL,
    run_id TEXT NOT NULL,
    saved_at TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS skill_versions_name ON skill_versions(name, id);
"#;

fn generated_from_row(row: &crate::storage::Row) -> crate::storage::Result<GeneratedSkill> {
    Ok(GeneratedSkill {
        name: row.get(0)?,
        hash: row.get(1)?,
        run_id: row.get(2)?,
        session_id: row.get(3)?,
        reason: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(filename: &Path) -> Result<Self> {
        Self::open_with_driver(&TursoDriver, filename)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error::Io)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::Io)?;
        }
        let connection = Connection::open_with_driver(
            driver,
            filename,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
        .map_err(|_| Error::Io)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Io)?;
        Self::from_connection(connection)
    }

    pub fn from_connection(connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Io)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| Error::Io)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error::Io)?)
    }

    fn initialize(mut connection: Connection) -> Result<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| Error::Io)?;
        if version == 0 {
            connection.execute_batch(SCHEMA).map_err(|_| Error::Io)?;
            connection
                .pragma_update("user_version", SCHEMA_VERSION)
                .map_err(|_| Error::Io)?;
        } else if version == 1 {
            let transaction = connection.transaction().map_err(|_| Error::Io)?;
            transaction
                .execute_batch(MIGRATE_1_TO_2)
                .map_err(|_| Error::Io)?;
            transaction
                .pragma_update("user_version", SCHEMA_VERSION)
                .map_err(|_| Error::Io)?;
            transaction.commit().map_err(|_| Error::Io)?;
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
                 memories_dropped, detail, skills_written) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
                    row.skills_written as i64,
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

    /// When the most recent automatic (not `manual` or `requested`) run for `session_id`
    /// started, as the RFC 3339 text it was recorded with.
    pub fn last_auto_started(&self, session_id: &str) -> Result<Option<String>> {
        self.lock()?
            .query_row(
                "SELECT MAX(started_at) FROM runs WHERE session_id = ?1 \
                 AND trigger NOT IN ('manual', 'requested')",
                params![session_id],
                |row| row.get::<Option<String>>(0),
            )
            .map_err(|_| Error::Io)
    }

    /// The skill reflection owns under `name`, if any.
    pub fn generated(&self, name: &str) -> Result<Option<GeneratedSkill>> {
        self.lock()?
            .query_row(
                "SELECT name, hash, run_id, session_id, reason, created_at, updated_at \
                 FROM generated_skills WHERE name = ?1",
                params![name],
                generated_from_row,
            )
            .optional()
            .map_err(|_| Error::Io)
    }

    /// Every skill reflection has written and not reverted, by name.
    pub fn list_generated(&self) -> Result<Vec<GeneratedSkill>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT name, hash, run_id, session_id, reason, created_at, updated_at \
                 FROM generated_skills ORDER BY name",
            )
            .map_err(|_| Error::Io)?;
        let rows = statement
            .query_map((), generated_from_row)
            .map_err(|_| Error::Io)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Io)
    }

    /// How many skills reflection currently owns.
    pub fn generated_count(&self) -> Result<usize> {
        self.lock()?
            .query_row("SELECT COUNT(*) FROM generated_skills", (), |row| {
                row.get::<i64>(0)
            })
            .map(|count| count as usize)
            .map_err(|_| Error::Io)
    }

    /// Records that reflection wrote `write` as the current content of its
    /// skill: it becomes (or stays) owned, and the content joins its versions.
    pub fn record_skill_write(&self, write: &SkillWrite) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(|_| Error::Io)?;
        transaction
            .execute(
                "INSERT INTO generated_skills \
                 (name, hash, run_id, session_id, reason, created_at, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?6) \
                 ON CONFLICT(name) DO UPDATE SET hash = excluded.hash, run_id = excluded.run_id, \
                 session_id = excluded.session_id, reason = excluded.reason, \
                 updated_at = excluded.updated_at",
                params![
                    write.name,
                    write.hash,
                    write.run_id,
                    write.session_id,
                    write.reason,
                    write.at
                ],
            )
            .map_err(|_| Error::Io)?;
        transaction
            .execute(
                "INSERT INTO skill_versions (name, hash, run_id, saved_at) VALUES (?1,?2,?3,?4)",
                params![write.name, write.hash, write.run_id, write.at],
            )
            .map_err(|_| Error::Io)?;
        transaction.commit().map_err(|_| Error::Io)
    }

    /// The content hashes reflection wrote for `name`, oldest first.
    pub fn skill_versions(&self, name: &str) -> Result<Vec<String>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT hash FROM skill_versions WHERE name = ?1 ORDER BY id")
            .map_err(|_| Error::Io)?;
        let rows = statement
            .query_map(params![name], |row| row.get::<String>(0))
            .map_err(|_| Error::Io)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Io)
    }

    /// Drops the newest version of `name` and makes the one before it current,
    /// returning its hash; with no earlier version, forgets the skill and
    /// returns `None`.
    pub fn pop_skill_version(&self, name: &str, at: &str) -> Result<Option<String>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(|_| Error::Io)?;
        transaction
            .execute(
                "DELETE FROM skill_versions WHERE id = \
                 (SELECT MAX(id) FROM skill_versions WHERE name = ?1)",
                params![name],
            )
            .map_err(|_| Error::Io)?;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT hash FROM skill_versions WHERE name = ?1 ORDER BY id DESC LIMIT 1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| Error::Io)?;
        match &previous {
            Some(hash) => {
                transaction
                    .execute(
                        "UPDATE generated_skills SET hash = ?2, updated_at = ?3 WHERE name = ?1",
                        params![name, hash, at],
                    )
                    .map_err(|_| Error::Io)?;
            }
            None => {
                transaction
                    .execute(
                        "DELETE FROM generated_skills WHERE name = ?1",
                        params![name],
                    )
                    .map_err(|_| Error::Io)?;
            }
        }
        transaction.commit().map_err(|_| Error::Io)?;
        Ok(previous)
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
                    row.get::<String>(0)?,
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

impl ReflectionStore for Store {
    fn watermark(&self, session_id: &str) -> Result<Option<String>> {
        Store::watermark(self, session_id)
    }
    fn record(&self, row: &RunRow) -> Result<()> {
        Store::record(self, row)
    }
    fn last_auto_started(&self, session_id: &str) -> Result<Option<String>> {
        Store::last_auto_started(self, session_id)
    }
    fn generated(&self, name: &str) -> Result<Option<GeneratedSkill>> {
        Store::generated(self, name)
    }
    fn list_generated(&self) -> Result<Vec<GeneratedSkill>> {
        Store::list_generated(self)
    }
    fn generated_count(&self) -> Result<usize> {
        Store::generated_count(self)
    }
    fn record_skill_write(&self, write: &SkillWrite) -> Result<()> {
        Store::record_skill_write(self, write)
    }
    fn skill_versions(&self, name: &str) -> Result<Vec<String>> {
        Store::skill_versions(self, name)
    }
    fn pop_skill_version(&self, name: &str, at: &str) -> Result<Option<String>> {
        Store::pop_skill_version(self, name, at)
    }
    #[cfg(test)]
    fn runs(&self, session_id: &str) -> Result<Vec<(String, Status)>> {
        Store::runs(self, session_id)
    }
}
