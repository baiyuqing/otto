//! Provider token usage collection, SQLite storage, and aggregate queries.

use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Days, NaiveDate, SecondsFormat, Utc};
use otto_core::agent::Event;
use otto_core::model::Usage;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

const SCHEMA: &str = r#"
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

/// A storage failure carries no SQLite text, paths, or row values across the
/// adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("usage store unavailable")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Runtime metadata attached to every usage event. It deliberately contains
/// no prompt, tool arguments, tool output, or response text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub workspace: String,
    pub session_id: String,
    pub task_id: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
}

/// The collection-to-storage contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub occurred_at: String,
    pub context: Context,
    pub kind: &'static str,
    pub usage: Usage,
    pub usage_present: bool,
}

/// Collection writes through this boundary and has no SQLite dependency.
pub trait Sink: Send + Sync {
    fn append(&self, record: &UsageRecord) -> Result<()>;
}

/// Persisted token totals. Cache hit rate is weighted by input tokens rather
/// than averaged across requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub requests: i64,
    pub reported_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_hit_rate: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DailyPoint {
    pub date: String,
    pub requests: i64,
    pub reported_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    pub summary: Summary,
    pub daily: Vec<DailyPoint>,
}

impl Analysis {
    pub fn empty(days: u16) -> Result<Self> {
        let (start, today) = date_range(days)?;
        Self::empty_range(start, today, days)
    }

    fn empty_range(start: NaiveDate, today: NaiveDate, days: u16) -> Result<Self> {
        let mut date = start;
        let mut daily = Vec::with_capacity(days as usize);
        loop {
            daily.push(DailyPoint {
                date: date.to_string(),
                ..DailyPoint::default()
            });
            if date == today {
                break;
            }
            date = date.checked_add_days(Days::new(1)).ok_or(Error)?;
        }
        Ok(Self {
            daily,
            ..Self::default()
        })
    }

    fn total(&mut self) {
        let mut summary = Summary::default();
        for point in &self.daily {
            summary.requests = summary.requests.saturating_add(point.requests);
            summary.reported_requests = summary
                .reported_requests
                .saturating_add(point.reported_requests);
            summary.input_tokens = summary.input_tokens.saturating_add(point.input_tokens);
            summary.output_tokens = summary.output_tokens.saturating_add(point.output_tokens);
            summary.cached_input_tokens = summary
                .cached_input_tokens
                .saturating_add(point.cached_input_tokens);
        }
        if summary.input_tokens > 0 {
            summary.cache_hit_rate =
                summary.cached_input_tokens as f64 / summary.input_tokens as f64;
        }
        self.summary = summary;
    }
}

fn date_range(days: u16) -> Result<(NaiveDate, NaiveDate)> {
    if !(1..=365).contains(&days) {
        return Err(Error);
    }
    let today = Utc::now().date_naive();
    let start = today
        .checked_sub_days(Days::new(u64::from(days - 1)))
        .ok_or(Error)?;
    Ok((start, today))
}

/// One local SQLite database shared by collection and read-only analysis.
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
        let connection = Connection::open_with_flags(
            filename,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| Error)?;
        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
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

/// Maps neutral agent events into the small persisted usage contract.
#[derive(Clone)]
pub struct Collector {
    sink: Arc<dyn Sink>,
    context: Context,
}

impl Collector {
    pub fn new<S: Sink + 'static>(sink: Arc<S>, context: Context) -> Self {
        Self { sink, context }
    }

    pub fn for_task(&self, task_id: &str) -> Self {
        let mut collector = self.clone();
        collector.context.task_id = task_id.to_string();
        collector
    }

    pub fn record(&self, event: &Event) -> Result<()> {
        let (kind, usage, present) = match event {
            Event::ProviderUsage { usage, present } => ("provider", *usage, *present),
            Event::CompactionCompleted { compaction } if !compaction.noop => {
                ("compaction", compaction.usage, compaction.usage_present)
            }
            _ => return Ok(()),
        };
        self.sink.append(&UsageRecord {
            occurred_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            context: self.context.clone(),
            kind,
            usage,
            usage_present: present,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Days, SecondsFormat, Utc};
    use otto_core::agent::{CompactionResult, Event};
    use otto_core::model::Usage;

    use super::{Collector, Context, Sink, Store, UsageRecord};

    fn usage(input: i64, output: i64, cached: i64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
        }
    }

    #[test]
    fn collector_persists_usage_for_aggregate_analysis() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let collector = Collector::new(
            Arc::clone(&store),
            Context {
                workspace: "/work".into(),
                session_id: "s1".into(),
                provider: "openai-compatible".into(),
                profile: "default".into(),
                model: "gpt-test".into(),
                task_id: String::new(),
            },
        );

        collector
            .record(&Event::ProviderUsage {
                usage: usage(100, 20, 40),
                present: true,
            })
            .expect("provider usage");
        collector
            .record(&Event::CompactionCompleted {
                compaction: CompactionResult {
                    usage: usage(50, 10, 35),
                    usage_present: true,
                    ..CompactionResult::default()
                },
            })
            .expect("compaction usage");
        collector
            .for_task("task-1")
            .record(&Event::ProviderUsage {
                usage: usage(50, 5, 25),
                present: true,
            })
            .expect("sub-agent usage");

        let summary = store.summary(Some("s1")).expect("summary");
        assert_eq!(summary.requests, 3);
        assert_eq!(summary.reported_requests, 3);
        assert_eq!(summary.input_tokens, 200);
        assert_eq!(summary.output_tokens, 35);
        assert_eq!(summary.cached_input_tokens, 100);
        assert_eq!(summary.cache_hit_rate, 0.5);
    }

    #[test]
    fn missing_usage_is_counted_but_not_invented_and_noop_compaction_is_skipped() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let collector = Collector::new(
            Arc::clone(&store),
            Context {
                session_id: "s1".into(),
                ..Context::default()
            },
        );

        collector
            .record(&Event::ProviderUsage {
                usage: Usage::default(),
                present: false,
            })
            .expect("missing usage");
        collector
            .record(&Event::CompactionCompleted {
                compaction: CompactionResult {
                    noop: true,
                    ..CompactionResult::default()
                },
            })
            .expect("noop");

        let summary = store.summary(None).expect("summary");
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.reported_requests, 0);
        assert_eq!(summary.input_tokens, 0);
        assert_eq!(summary.cache_hit_rate, 0.0);
    }

    #[test]
    fn file_store_reopens_with_committed_usage() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("usage").join("usage.db");
        {
            let store = Arc::new(Store::open(&path).expect("open"));
            Collector::new(
                Arc::clone(&store),
                Context {
                    session_id: "persisted".into(),
                    ..Context::default()
                },
            )
            .record(&Event::ProviderUsage {
                usage: usage(8, 3, 2),
                present: true,
            })
            .expect("record");
        }

        let summary = Store::open(&path)
            .expect("reopen")
            .summary(Some("persisted"))
            .expect("summary");
        assert_eq!(summary.input_tokens, 8);
        assert_eq!(summary.output_tokens, 3);
        assert_eq!(summary.cached_input_tokens, 2);

        use std::os::unix::fs::PermissionsExt;
        let directory_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn daily_analysis_fills_the_range_and_sums_its_points() {
        let store = Store::open_in_memory().expect("store");
        let today = Utc::now().date_naive();
        let yesterday = today.checked_sub_days(Days::new(1)).expect("yesterday");
        for (date, tokens) in [(yesterday, usage(10, 2, 4)), (today, usage(20, 3, 6))] {
            store
                .append(&UsageRecord {
                    occurred_at: date
                        .and_hms_opt(12, 0, 0)
                        .expect("time")
                        .and_utc()
                        .to_rfc3339_opts(SecondsFormat::Nanos, true),
                    context: Context {
                        session_id: "s1".into(),
                        ..Context::default()
                    },
                    kind: "provider",
                    usage: tokens,
                    usage_present: true,
                })
                .expect("append");
        }

        let analysis = store.daily(3, Some("s1")).expect("analysis");
        assert_eq!(analysis.daily.len(), 3);
        assert_eq!(analysis.daily[0].input_tokens, 0);
        assert_eq!(analysis.daily[1].date, yesterday.to_string());
        assert_eq!(analysis.daily[2].date, today.to_string());
        assert_eq!(analysis.summary.requests, 2);
        assert_eq!(analysis.summary.input_tokens, 30);
        assert_eq!(analysis.summary.output_tokens, 5);
        assert_eq!(analysis.summary.cached_input_tokens, 10);
        assert_eq!(analysis.summary.cache_hit_rate, 1.0 / 3.0);
    }
}
