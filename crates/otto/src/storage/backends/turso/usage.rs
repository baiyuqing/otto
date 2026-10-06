//! Turso usage repository.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use crate::storage::{Connection, params};
use crate::usage::repository::UsageRepository;
use crate::usage::{Analysis, DailyPoint, Error, Result, Sink, Summary, UsageRecord, date_range};
use otto_core::model::Usage;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS usage_events (
    id INTEGER PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    profile TEXT NOT NULL,
    model TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('provider','compaction')),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL CHECK (
        cached_input_tokens >= 0 AND cached_input_tokens <= input_tokens
    ),
    usage_present INTEGER NOT NULL CHECK (usage_present IN (0, 1)),
    CHECK (usage_present = 1 OR (
        input_tokens = 0 AND output_tokens = 0 AND cached_input_tokens = 0
    ))
) STRICT;
CREATE INDEX IF NOT EXISTS usage_events_session
ON usage_events(session_id, occurred_at);
CREATE INDEX IF NOT EXISTS usage_events_model
ON usage_events(provider, model, occurred_at);
"#;

/// One local Turso database shared by collection and read-only analysis.
/// Each operation holds the connection only for one short statement.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open(filename).map_err(|_| Error)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        Self::from_connection(connection)
    }

    pub fn from_connection(connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| Error)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error)?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        connection.execute_batch(SCHEMA).map_err(|_| Error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn append_record(&self, record: &UsageRecord) -> Result<()> {
        record.usage.validate().map_err(|_| Error)?;
        if !record.usage_present && record.usage != Usage::default() {
            return Err(Error);
        }
        self.connection
            .lock()
            .map_err(|_| Error)?
            .execute(
                "INSERT INTO usage_events (
                    occurred_at, workspace, session_id, task_id, provider, profile, model,
                    kind, input_tokens, output_tokens, cached_input_tokens, usage_present
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    record.occurred_at,
                    record.context.workspace,
                    record.context.session_id,
                    record.context.task_id,
                    record.context.provider,
                    record.context.profile,
                    record.context.model,
                    record.kind,
                    record.usage.input_tokens,
                    record.usage.output_tokens,
                    record.usage.cached_input_tokens,
                    record.usage_present,
                ],
            )
            .map_err(|_| Error)?;
        Ok(())
    }

    /// Returns all recorded usage, or one session when `session_id` is set.
    pub fn summary(&self, session_id: Option<&str>) -> Result<Summary> {
        let connection = self.connection.lock().map_err(|_| Error)?;
        let mut statement = connection
            .prepare(
                "SELECT
                    COUNT(*),
                    COALESCE(SUM(usage_present), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0)
                 FROM usage_events
                 WHERE (?1 IS NULL OR session_id = ?1)",
            )
            .map_err(|_| Error)?;
        let mut summary = statement
            .query_row([session_id], |row| {
                Ok(Summary {
                    requests: row.get(0)?,
                    reported_requests: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cached_input_tokens: row.get(4)?,
                    cache_hit_rate: 0.0,
                })
            })
            .map_err(|_| Error)?;
        if summary.input_tokens > 0 {
            summary.cache_hit_rate =
                summary.cached_input_tokens as f64 / summary.input_tokens as f64;
        }
        Ok(summary)
    }

    /// Returns a zero-filled UTC day series, including today.
    pub fn daily(&self, days: u16, session_id: Option<&str>) -> Result<Analysis> {
        let (start, today) = date_range(days)?;
        let since = format!("{start}T00:00:00.000000000Z");
        let connection = self.connection.lock().map_err(|_| Error)?;
        let mut statement = connection
            .prepare(
                "SELECT
                    substr(occurred_at, 1, 10),
                    COUNT(*),
                    COALESCE(SUM(usage_present), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0)
                 FROM usage_events
                 WHERE occurred_at >= ?1 AND (?2 IS NULL OR session_id = ?2)
                 GROUP BY substr(occurred_at, 1, 10)
                 ORDER BY substr(occurred_at, 1, 10)",
            )
            .map_err(|_| Error)?;
        let rows = statement
            .query_map(params![since, session_id], |row| {
                Ok(DailyPoint {
                    date: row.get(0)?,
                    requests: row.get(1)?,
                    reported_requests: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    cached_input_tokens: row.get(5)?,
                })
            })
            .map_err(|_| Error)?;
        let mut recorded = BTreeMap::new();
        for row in rows {
            let point = row.map_err(|_| Error)?;
            recorded.insert(point.date.clone(), point);
        }
        let mut analysis = Analysis::empty_range(start, today, days)?;
        for point in &mut analysis.daily {
            if let Some(recorded) = recorded.remove(&point.date) {
                *point = recorded;
            }
        }
        analysis.total();
        Ok(analysis)
    }
}

impl Sink for Store {
    fn append(&self, record: &UsageRecord) -> Result<()> {
        self.append_record(record)
    }
}

impl UsageRepository for Store {
    fn summary(&self, session_id: Option<&str>) -> Result<Summary> {
        Store::summary(self, session_id)
    }

    fn daily(&self, days: u16, session_id: Option<&str>) -> Result<Analysis> {
        Store::daily(self, days, session_id)
    }
}
