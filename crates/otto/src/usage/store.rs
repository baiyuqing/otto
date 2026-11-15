//! SQLite usage repository.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use super::repository::UsageRepository;
use super::{
    Analysis, ApiOperationRecord, DailyPoint, Error, Result, Sink, Summary, UsageRecord, date_range,
};
use crate::storage::drivers::sqlite::SqliteDriver;
use crate::storage::{Connection, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
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
    cached_breakdown_present INTEGER CHECK (cached_breakdown_present IN (0, 1)),
    cache_write_input_tokens INTEGER CHECK (cache_write_input_tokens IS NULL OR (
        usage_present=1 AND cache_write_input_tokens >= 0 AND cache_write_input_tokens <= input_tokens - cached_input_tokens)),
    cache_write_1h_input_tokens INTEGER CHECK (cache_write_1h_input_tokens IS NULL OR (
        cache_write_input_tokens IS NOT NULL AND cache_write_1h_input_tokens >= 0 AND cache_write_1h_input_tokens <= cache_write_input_tokens)),
    CHECK (usage_present = 1 OR (
        input_tokens = 0 AND output_tokens = 0 AND cached_input_tokens = 0
    )),
    CHECK (usage_present = 1 OR cached_breakdown_present IS NULL)
) STRICT;
CREATE INDEX IF NOT EXISTS usage_events_session
ON usage_events(session_id, occurred_at);
CREATE INDEX IF NOT EXISTS usage_events_model
ON usage_events(provider, model, occurred_at);
CREATE TABLE IF NOT EXISTS provider_api_operations (
    id INTEGER PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    profile TEXT NOT NULL,
    model TEXT NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN (
        'ordinary','local_compaction','remote_compaction'
    )),
    mode TEXT,
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    status TEXT NOT NULL CHECK (status IN ('ok','canceled','error')),
    disposition TEXT NOT NULL CHECK (disposition IN (
        'succeeded','error','cancelled','deadline_exceeded','interrupted'
    )),
    effect_certainty TEXT NOT NULL CHECK (effect_certainty IN (
        'not_started','known_no_effect','completed','unknown'
    )),
    stop_reason TEXT CHECK (stop_reason IN (
        'user_cancellation','deadline','shutdown','migration','transport_lost','process_lost'
    )),
    CHECK (effect_certainty != 'not_started' OR attempts = 0)
) STRICT;
CREATE INDEX IF NOT EXISTS provider_api_operations_session
ON provider_api_operations(session_id, occurred_at);
CREATE INDEX IF NOT EXISTS provider_api_operations_kind
ON provider_api_operations(kind, occurred_at);
"#;

/// One local SQLite database shared by collection and read-only analysis.
/// Each operation holds the connection only for one short statement.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(filename: &Path) -> Result<Self> {
        Self::open_with_driver(&SqliteDriver, filename)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, filename: &Path) -> Result<Self> {
        if let Some(parent) = filename.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open_with_driver(
            driver,
            filename,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
        .map_err(|_| Error)?;
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
        // The original usage database had no user_version. Detect its shape
        // directly, add only nullable metadata, and never infer history.
        let transaction = connection.unchecked_transaction().map_err(|_| Error)?;
        let has_usage_table: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='usage_events'",
                (),
                |row| row.get(0),
            )
            .map_err(|_| Error)?;
        if has_usage_table != 0 {
            let mut columns = std::collections::HashSet::new();
            for name in transaction
                .prepare("PRAGMA table_info(usage_events)")
                .map_err(|_| Error)?
                .query_map((), |row| row.get::<String>(1))
                .map_err(|_| Error)?
            {
                columns.insert(name.map_err(|_| Error)?);
            }
            for (column, declaration) in [
                (
                    "cached_breakdown_present",
                    "INTEGER CHECK (cached_breakdown_present IN (0, 1))",
                ),
                (
                    "cache_write_input_tokens",
                    "INTEGER CHECK (cache_write_input_tokens IS NULL OR (
                        usage_present=1 AND cache_write_input_tokens >= 0 AND cache_write_input_tokens <= input_tokens - cached_input_tokens))",
                ),
                (
                    "cache_write_1h_input_tokens",
                    "INTEGER CHECK (cache_write_1h_input_tokens IS NULL OR (
                        cache_write_input_tokens IS NOT NULL AND cache_write_1h_input_tokens >= 0 AND cache_write_1h_input_tokens <= cache_write_input_tokens))",
                ),
            ] {
                if !columns.contains(column) {
                    transaction
                        .execute_batch(&format!(
                            "ALTER TABLE usage_events ADD COLUMN {column} {declaration}"
                        ))
                        .map_err(|_| Error)?;
                }
            }
        }
        transaction.execute_batch(SCHEMA).map_err(|_| Error)?;
        transaction.commit().map_err(|_| Error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn append_record(&self, record: &UsageRecord) -> Result<()> {
        record.usage.validate().map_err(|_| Error)?;
        if !record.usage_present
            && (record.usage != Usage::default() || record.cached_input_tokens_present)
        {
            return Err(Error);
        }
        self.connection
            .lock()
            .map_err(|_| Error)?
            .execute(
                "INSERT INTO usage_events (
                    occurred_at, workspace, session_id, task_id, provider, profile, model,
                    kind, input_tokens, output_tokens, cached_input_tokens, usage_present,
                    cached_breakdown_present, cache_write_input_tokens, cache_write_1h_input_tokens
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    record
                        .usage_present
                        .then_some(record.cached_input_tokens_present),
                    record.usage.cache_write_input_tokens,
                    record.usage.cache_write_1h_input_tokens,
                ],
            )
            .map_err(|_| Error)?;
        Ok(())
    }

    fn append_operation(&self, record: &ApiOperationRecord) -> Result<()> {
        if record.operation_id.is_empty()
            || (record.effect_certainty == "not_started" && record.attempts != 0)
        {
            return Err(Error);
        }
        self.connection
            .lock()
            .map_err(|_| Error)?
            .execute(
                "INSERT INTO provider_api_operations (
                    occurred_at, workspace, session_id, task_id, provider, profile, model,
                    operation_id, kind, mode, attempts, status, disposition, effect_certainty,
                    stop_reason
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    record.occurred_at,
                    record.context.workspace,
                    record.context.session_id,
                    record.context.task_id,
                    record.context.provider,
                    record.context.profile,
                    record.context.model,
                    record.operation_id,
                    record.kind,
                    record.mode,
                    i64::from(record.attempts),
                    record.status,
                    record.disposition,
                    record.effect_certainty,
                    record.stop_reason,
                ],
            )
            .map_err(|_| Error)?;
        Ok(())
    }

    /// Returns all recorded usage and operations, or one session when set.
    pub fn summary(&self, session_id: Option<&str>) -> Result<Summary> {
        let connection = self.connection.lock().map_err(|_| Error)?;
        let token = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(usage_present),0),
                    COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cached_breakdown_present IS NOT 1 THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(CASE WHEN cached_breakdown_present=1 THEN cached_input_tokens ELSE 0 END),0)
                    END,
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cache_write_input_tokens IS NULL THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(cache_write_input_tokens),0)
                    END,
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cache_write_1h_input_tokens IS NULL THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(cache_write_1h_input_tokens),0)
                    END
                 FROM usage_events WHERE (?1 IS NULL OR session_id=?1)",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            )
            .map_err(|_| Error)?;
        let operation = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(attempts),0),
                    COALESCE(SUM(disposition='succeeded'),0), COALESCE(SUM(disposition='error'),0),
                    COALESCE(SUM(disposition='cancelled'),0), COALESCE(SUM(disposition='deadline_exceeded'),0),
                    COALESCE(SUM(disposition='interrupted'),0),
                    COALESCE(SUM(effect_certainty='not_started'),0), COALESCE(SUM(effect_certainty='known_no_effect'),0),
                    COALESCE(SUM(effect_certainty='completed'),0), COALESCE(SUM(effect_certainty='unknown'),0)
                 FROM provider_api_operations WHERE (?1 IS NULL OR session_id=?1)",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?)),
            )
            .map_err(|_| Error)?;
        let mut summary = Summary {
            usage_events: token.0,
            reported_usage_events: token.1,
            input_tokens: token.2,
            output_tokens: token.3,
            cached_input_tokens: token.4,
            cache_write_input_tokens: token.5,
            cache_write_1h_input_tokens: token.6,
            logical_operations: operation.0,
            actual_attempts: operation.1,
            succeeded_operations: operation.2,
            failed_operations: operation.3,
            canceled_operations: operation.4,
            deadline_exceeded_operations: operation.5,
            interrupted_operations: operation.6,
            not_started_operations: operation.7,
            known_no_effect_operations: operation.8,
            completed_effect_operations: operation.9,
            unknown_effect_operations: operation.10,
            cache_hit_rate: None,
        };
        summary.cache_hit_rate = summary
            .cached_input_tokens
            .filter(|_| summary.input_tokens > 0)
            .map(|cached| cached as f64 / summary.input_tokens as f64);
        Ok(summary)
    }

    /// Returns a zero-filled UTC day series, including today.
    pub fn daily(&self, days: u16, session_id: Option<&str>) -> Result<Analysis> {
        let (start, today) = date_range(days)?;
        let since = format!("{start}T00:00:00.000000000Z");
        let connection = self.connection.lock().map_err(|_| Error)?;
        let mut recorded = BTreeMap::new();
        let mut statement = connection
            .prepare(
                "SELECT substr(occurred_at,1,10), COUNT(*), COALESCE(SUM(usage_present),0),
                    COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cached_breakdown_present IS NOT 1 THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(CASE WHEN cached_breakdown_present=1 THEN cached_input_tokens ELSE 0 END),0)
                    END,
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cache_write_input_tokens IS NULL THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(cache_write_input_tokens),0)
                    END,
                    CASE
                      WHEN COALESCE(SUM(usage_present),0)=0 THEN NULL
                      WHEN SUM(CASE WHEN usage_present=1 AND cache_write_1h_input_tokens IS NULL THEN 1 ELSE 0 END)>0 THEN NULL
                      ELSE COALESCE(SUM(cache_write_1h_input_tokens),0)
                    END
                 FROM usage_events
                 WHERE occurred_at>=?1 AND (?2 IS NULL OR session_id=?2)
                 GROUP BY substr(occurred_at,1,10)",
            )
            .map_err(|_| Error)?;
        for row in statement
            .query_map(params![since, session_id], |row| {
                Ok(DailyPoint {
                    date: row.get(0)?,
                    usage_events: row.get(1)?,
                    reported_usage_events: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    cached_input_tokens: row.get(5)?,
                    cache_write_input_tokens: row.get(6)?,
                    cache_write_1h_input_tokens: row.get(7)?,
                    ..DailyPoint::default()
                })
            })
            .map_err(|_| Error)?
        {
            let point = row.map_err(|_| Error)?;
            recorded.insert(point.date.clone(), point);
        }
        let mut operation_statement = connection
            .prepare(
                "SELECT substr(occurred_at,1,10), COUNT(*), COALESCE(SUM(attempts),0),
                    COALESCE(SUM(disposition='succeeded'),0), COALESCE(SUM(disposition='error'),0),
                    COALESCE(SUM(disposition='cancelled'),0), COALESCE(SUM(disposition='deadline_exceeded'),0),
                    COALESCE(SUM(disposition='interrupted'),0),
                    COALESCE(SUM(effect_certainty='not_started'),0), COALESCE(SUM(effect_certainty='known_no_effect'),0),
                    COALESCE(SUM(effect_certainty='completed'),0), COALESCE(SUM(effect_certainty='unknown'),0)
                 FROM provider_api_operations
                 WHERE occurred_at>=?1 AND (?2 IS NULL OR session_id=?2)
                 GROUP BY substr(occurred_at,1,10)",
            )
            .map_err(|_| Error)?;
        for row in operation_statement
            .query_map(params![since, session_id], |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<i64>(2)?,
                    row.get::<i64>(3)?,
                    row.get::<i64>(4)?,
                    row.get::<i64>(5)?,
                    row.get::<i64>(6)?,
                    row.get::<i64>(7)?,
                    row.get::<i64>(8)?,
                    row.get::<i64>(9)?,
                    row.get::<i64>(10)?,
                    row.get::<i64>(11)?,
                ))
            })
            .map_err(|_| Error)?
        {
            let (
                date,
                operations,
                attempts,
                succeeded,
                failed,
                canceled,
                deadline_exceeded,
                interrupted,
                not_started,
                known_no_effect,
                completed_effect,
                unknown_effect,
            ) = row.map_err(|_| Error)?;
            let point = recorded.entry(date.clone()).or_insert_with(|| DailyPoint {
                date,
                ..DailyPoint::default()
            });
            point.logical_operations = operations;
            point.actual_attempts = attempts;
            point.succeeded_operations = succeeded;
            point.failed_operations = failed;
            point.canceled_operations = canceled;
            point.deadline_exceeded_operations = deadline_exceeded;
            point.interrupted_operations = interrupted;
            point.not_started_operations = not_started;
            point.known_no_effect_operations = known_no_effect;
            point.completed_effect_operations = completed_effect;
            point.unknown_effect_operations = unknown_effect;
        }
        let mut analysis = Analysis::empty_range(start, today, days)?;
        for point in &mut analysis.daily {
            if let Some(value) = recorded.remove(&point.date) {
                *point = value;
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

    fn append_api_operation(&self, record: &ApiOperationRecord) -> Result<()> {
        self.append_operation(record)
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

#[cfg(test)]
mod cache_usage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_usage_table_gains_the_cache_breakdown_column() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .execute_batch(
                "CREATE TABLE usage_events (
                    id INTEGER PRIMARY KEY,
                    occurred_at TEXT NOT NULL,
                    workspace TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    task_id TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    model TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    cached_input_tokens INTEGER NOT NULL,
                    usage_present INTEGER NOT NULL
                ) STRICT;
                INSERT INTO usage_events (occurred_at, workspace, session_id, task_id,
                    provider, profile, model, kind, input_tokens, output_tokens,
                    cached_input_tokens, usage_present)
                VALUES ('2026-01-01T00:00:00Z', 'w', 's', '', 'chatgpt', 'p', 'm',
                    'provider', 10, 2, 0, 1);",
            )
            .expect("legacy table");

        let store = Store::initialize(connection).expect("upgrade");

        let summary = store.summary(None).expect("summary");
        assert_eq!(summary.usage_events, 1);
        assert_eq!(summary.input_tokens, 10);
        // A legacy row never recorded whether the breakdown was present.
        assert_eq!(summary.cached_input_tokens, None);
        assert_eq!(summary.cache_write_input_tokens, None);
        assert_eq!(summary.cache_write_1h_input_tokens, None);
    }
}
