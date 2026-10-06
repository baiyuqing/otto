//! Usage collection and aggregate query contracts.

pub mod migration;
pub mod repository;

use std::fmt;
use std::sync::Arc;

use chrono::{Days, NaiveDate, SecondsFormat, Utc};
use otto_core::agent::Event;
use otto_core::model::Usage;
use serde::Serialize;

/// A storage failure carries no database text, paths, or row values across the
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

/// Collection writes through this boundary and has no storage dependency.
pub trait Sink: Send + Sync {
    fn append(&self, record: &UsageRecord) -> Result<()>;
}

pub use crate::storage::backends::turso::usage::Store;

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

    pub(crate) fn empty_range(start: NaiveDate, today: NaiveDate, days: u16) -> Result<Self> {
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

    pub(crate) fn total(&mut self) {
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

pub(crate) fn date_range(days: u16) -> Result<(NaiveDate, NaiveDate)> {
    if !(1..=365).contains(&days) {
        return Err(Error);
    }
    let today = Utc::now().date_naive();
    let start = today
        .checked_sub_days(Days::new(u64::from(days - 1)))
        .ok_or(Error)?;
    Ok((start, today))
}

/// Maps neutral agent events into the small persisted usage contract.
#[derive(Clone)]
pub struct Collector {
    sink: Arc<dyn Sink>,
    context: Context,
}

impl Collector {
    pub fn new<S: Sink + 'static>(sink: Arc<S>, context: Context) -> Self {
        Self::from_sink(sink, context)
    }

    pub fn from_sink(sink: Arc<dyn Sink>, context: Context) -> Self {
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use chrono::{Days, SecondsFormat, Utc};
    use otto_core::agent::{CompactionResult, Event};
    use otto_core::model::Usage;

    use super::{Collector, Context, Sink, Store, UsageRecord};

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<UsageRecord>>);

    impl Sink for RecordingSink {
        fn append(&self, record: &UsageRecord) -> super::Result<()> {
            self.0.lock().expect("records").push(record.clone());
            Ok(())
        }
    }

    #[test]
    fn collector_uses_an_injected_sink() {
        let sink = Arc::new(RecordingSink::default());
        let collector = Collector::new(
            Arc::clone(&sink),
            Context {
                session_id: "injected".into(),
                ..Context::default()
            },
        );

        collector
            .record(&Event::ProviderUsage {
                usage: usage(12, 3, 4),
                present: true,
            })
            .expect("record");
        let records = sink.0.lock().expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].context.session_id, "injected");
        assert_eq!(records[0].usage, usage(12, 3, 4));
    }

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
