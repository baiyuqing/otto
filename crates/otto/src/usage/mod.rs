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

/// One token-usage report. This is deliberately not an API-operation record:
/// a provider operation may report no usage, and compaction must not duplicate
/// the provider usage event that supplied its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub occurred_at: String,
    pub context: Context,
    pub kind: &'static str,
    pub usage: Usage,
    pub usage_present: bool,
    pub cached_input_tokens_present: bool,
}

/// One completed logical provider API operation. It contains classifications
/// and counts only, never request or response content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiOperationRecord {
    pub occurred_at: String,
    pub context: Context,
    pub operation_id: String,
    pub kind: &'static str,
    pub mode: Option<&'static str>,
    pub attempts: u32,
    pub status: &'static str,
    pub disposition: &'static str,
    pub effect_certainty: &'static str,
    pub stop_reason: Option<&'static str>,
}

/// Collection writes through this boundary and has no storage dependency.
pub trait Sink: Send + Sync {
    fn append(&self, record: &UsageRecord) -> Result<()>;

    /// Kept defaulted so third-party sinks compiled against the token-only
    /// contract remain source-compatible; durable stores override it.
    fn append_api_operation(&self, _record: &ApiOperationRecord) -> Result<()> {
        Ok(())
    }
}

pub use crate::storage::backends::turso::usage::Store;

/// Persisted token and provider-operation totals. Usage-event counts are not
/// provider attempts. Cache totals/rate are absent until a provider explicitly
/// reports whether the cached-token breakdown was present.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub usage_events: i64,
    pub reported_usage_events: i64,
    pub logical_operations: i64,
    pub actual_attempts: i64,
    pub succeeded_operations: i64,
    pub failed_operations: i64,
    pub canceled_operations: i64,
    pub deadline_exceeded_operations: i64,
    pub interrupted_operations: i64,
    pub not_started_operations: i64,
    pub known_no_effect_operations: i64,
    pub completed_effect_operations: i64,
    pub unknown_effect_operations: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: Option<i64>,
    pub cache_hit_rate: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DailyPoint {
    pub date: String,
    pub usage_events: i64,
    pub reported_usage_events: i64,
    pub logical_operations: i64,
    pub actual_attempts: i64,
    pub succeeded_operations: i64,
    pub failed_operations: i64,
    pub canceled_operations: i64,
    pub deadline_exceeded_operations: i64,
    pub interrupted_operations: i64,
    pub not_started_operations: i64,
    pub known_no_effect_operations: i64,
    pub completed_effect_operations: i64,
    pub unknown_effect_operations: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: Option<i64>,
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
        let mut cache_known = true;
        let mut cached_total = 0_i64;
        for point in &self.daily {
            summary.usage_events = summary.usage_events.saturating_add(point.usage_events);
            summary.reported_usage_events = summary
                .reported_usage_events
                .saturating_add(point.reported_usage_events);
            summary.logical_operations = summary
                .logical_operations
                .saturating_add(point.logical_operations);
            summary.actual_attempts = summary
                .actual_attempts
                .saturating_add(point.actual_attempts);
            summary.succeeded_operations = summary
                .succeeded_operations
                .saturating_add(point.succeeded_operations);
            summary.failed_operations = summary
                .failed_operations
                .saturating_add(point.failed_operations);
            summary.canceled_operations = summary
                .canceled_operations
                .saturating_add(point.canceled_operations);
            summary.deadline_exceeded_operations = summary
                .deadline_exceeded_operations
                .saturating_add(point.deadline_exceeded_operations);
            summary.interrupted_operations = summary
                .interrupted_operations
                .saturating_add(point.interrupted_operations);
            summary.not_started_operations = summary
                .not_started_operations
                .saturating_add(point.not_started_operations);
            summary.known_no_effect_operations = summary
                .known_no_effect_operations
                .saturating_add(point.known_no_effect_operations);
            summary.completed_effect_operations = summary
                .completed_effect_operations
                .saturating_add(point.completed_effect_operations);
            summary.unknown_effect_operations = summary
                .unknown_effect_operations
                .saturating_add(point.unknown_effect_operations);
            summary.input_tokens = summary.input_tokens.saturating_add(point.input_tokens);
            summary.output_tokens = summary.output_tokens.saturating_add(point.output_tokens);
            match point.cached_input_tokens {
                Some(value) => cached_total = cached_total.saturating_add(value),
                None if point.reported_usage_events > 0 => cache_known = false,
                None => {}
            }
        }
        if cache_known && summary.reported_usage_events > 0 {
            summary.cached_input_tokens = Some(cached_total);
            summary.cache_hit_rate = (summary.input_tokens > 0)
                .then_some(cached_total as f64 / summary.input_tokens as f64);
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
        let occurred_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        match event {
            Event::ProviderUsage {
                usage,
                present,
                cached_input_tokens_present,
            } => self.sink.append(&UsageRecord {
                occurred_at,
                context: self.context.clone(),
                kind: "provider",
                usage: *usage,
                usage_present: *present,
                cached_input_tokens_present: *present && *cached_input_tokens_present,
            }),
            Event::CompactionCompleted { compaction } if !compaction.noop => {
                self.sink.append(&UsageRecord {
                    occurred_at,
                    context: self.context.clone(),
                    kind: "compaction",
                    usage: compaction.usage,
                    usage_present: compaction.usage_present,
                    cached_input_tokens_present: compaction.usage_present
                        && compaction.cached_input_tokens_present,
                })
            }
            Event::ProviderApiCall {
                operation_id,
                kind,
                attempts,
                status,
                outcome,
                ..
            } => self.sink.append_api_operation(&ApiOperationRecord {
                occurred_at,
                context: self.context.clone(),
                operation_id: operation_id.to_string(),
                kind: kind.name(),
                mode: None,
                attempts: *attempts,
                status: status.name(),
                disposition: disposition_name(outcome.disposition),
                effect_certainty: effect_certainty_name(outcome.effect_certainty),
                stop_reason: outcome.stop_reason.map(stop_reason_name),
            }),
            _ => Ok(()),
        }
    }
}

fn disposition_name(value: otto_core::model::OperationDisposition) -> &'static str {
    use otto_core::model::OperationDisposition::*;
    match value {
        Succeeded => "succeeded",
        Error => "error",
        Cancelled => "cancelled",
        DeadlineExceeded => "deadline_exceeded",
        Interrupted => "interrupted",
    }
}

fn effect_certainty_name(value: otto_core::model::EffectCertainty) -> &'static str {
    use otto_core::model::EffectCertainty::*;
    match value {
        NotStarted => "not_started",
        KnownNoEffect => "known_no_effect",
        Completed => "completed",
        Unknown => "unknown",
    }
}

fn stop_reason_name(value: otto_core::model::OperationStopReason) -> &'static str {
    use otto_core::model::OperationStopReason::*;
    match value {
        UserCancellation => "user_cancellation",
        Deadline => "deadline",
        Shutdown => "shutdown",
        Migration => "migration",
        TransportLost => "transport_lost",
        ProcessLost => "process_lost",
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
                cached_input_tokens_present: true,
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
                cached_input_tokens_present: true,
            })
            .expect("provider usage");
        collector
            .record(&Event::CompactionCompleted {
                compaction: CompactionResult {
                    usage: usage(50, 10, 35),
                    usage_present: true,
                    cached_input_tokens_present: true,
                    ..CompactionResult::default()
                },
            })
            .expect("compaction usage");
        collector
            .for_task("task-1")
            .record(&Event::ProviderUsage {
                usage: usage(50, 5, 25),
                present: true,
                cached_input_tokens_present: true,
            })
            .expect("sub-agent usage");

        let summary = store.summary(Some("s1")).expect("summary");
        assert_eq!(summary.usage_events, 3);
        assert_eq!(summary.reported_usage_events, 3);
        assert_eq!(summary.logical_operations, 0);
        assert_eq!(summary.actual_attempts, 0);
        assert_eq!(summary.input_tokens, 200);
        assert_eq!(summary.output_tokens, 35);
        assert_eq!(summary.cached_input_tokens, Some(100));
        assert_eq!(summary.cache_hit_rate, Some(0.5));
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
                cached_input_tokens_present: false,
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
        assert_eq!(summary.usage_events, 1);
        assert_eq!(summary.reported_usage_events, 0);
        assert_eq!(summary.input_tokens, 0);
        assert_eq!(summary.cache_hit_rate, None);
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
                cached_input_tokens_present: true,
            })
            .expect("record");
        }

        let summary = Store::open(&path)
            .expect("reopen")
            .summary(Some("persisted"))
            .expect("summary");
        assert_eq!(summary.input_tokens, 8);
        assert_eq!(summary.output_tokens, 3);
        assert_eq!(summary.cached_input_tokens, Some(2));

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
                    cached_input_tokens_present: true,
                })
                .expect("append");
        }

        let analysis = store.daily(3, Some("s1")).expect("analysis");
        assert_eq!(analysis.daily.len(), 3);
        assert_eq!(analysis.daily[0].input_tokens, 0);
        assert_eq!(analysis.daily[1].date, yesterday.to_string());
        assert_eq!(analysis.daily[2].date, today.to_string());
        assert_eq!(analysis.summary.usage_events, 2);
        assert_eq!(analysis.summary.input_tokens, 30);
        assert_eq!(analysis.summary.output_tokens, 5);
        assert_eq!(analysis.summary.cached_input_tokens, Some(10));
        assert_eq!(analysis.summary.cache_hit_rate, Some(1.0 / 3.0));
    }
}
