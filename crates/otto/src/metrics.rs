//! Prometheus text exposition for `otto serve`.
//!
//! Every counter, gauge and histogram is hand-rolled: the output is the
//! `version=0.0.4` text format, written in the order and with the label sets
//! defined in this module; a change to a metric name or label set changes what
//! existing scrape configs and queries select.
//!
//! Ordering: each label-key map is written in sorted key order. The maps here
//! are [`BTreeMap`]s, so iteration order already gives that without a sort
//! step.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod process;

const PROCESS_SAMPLE_WARNING_INTERVAL: Duration = Duration::from_secs(300);

use otto_core::diagnostics::TurnStop;
use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationId, OperationOutcome, OperationStopReason,
    Usage,
};

const HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const TURN_TOOL_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// One cumulative histogram over a fixed bucket list.
#[derive(Debug, Clone)]
struct Histogram {
    buckets: &'static [f64],
    bin_counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn new(buckets: &'static [f64]) -> Self {
        Self {
            buckets,
            bin_counts: vec![0; buckets.len()],
            sum: 0.0,
            count: 0,
        }
    }

    /// Finds the first bucket whose upper bound is at least `value`; a value
    /// past the last bound is counted only in `+Inf`.
    fn observe(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        let index = self.buckets.partition_point(|bound| *bound < value);
        if index < self.buckets.len() {
            self.bin_counts[index] += 1;
        }
    }
}

/// One open session's context accounting, sampled when `/metrics` is served.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub context_window: i64,
    pub context_input_tokens: i64,
    pub context_input_tokens_present: bool,
    pub context_input_tokens_pending: bool,
}

/// Where a task is in its lifecycle, as far as task metrics distinguish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    /// Queued or otherwise not yet running and not finished.
    Pending,
    Running,
    /// No further update can leave this status.
    Finished,
}

/// One task as observed by [`Metrics::diff_tasks`].
#[derive(Debug, Clone)]
pub struct TaskSample {
    pub id: String,
    /// The status label recorded in `tasks_finished_total`.
    pub status: String,
    pub phase: TaskPhase,
}

pub struct ProviderOperationMetric<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub status: &'a str,
    pub elapsed: Duration,
    pub operation_id: &'a OperationId,
    pub attempts: u32,
    pub outcome: &'a OperationOutcome,
}

#[derive(Debug, Clone, Default)]
struct SessionContextValues {
    context_window: i64,
    context_input_tokens: i64,
    context_input_tokens_present: bool,
    context_input_tokens_pending: i64,
}

type HttpKey = (String, String, String);
type ToolKey = (String, String);
type OperationUnknownKey = (String, String);
type ProviderApiKey = (String, String, String);
type ProviderApiDurationKey = (String, String);
type ProviderOutcomeKey = (String, String, String, String, String);
type SessionContextKey = (String, String, String);

#[derive(Debug, Default)]
struct State {
    http_total: BTreeMap<HttpKey, u64>,
    http_duration: BTreeMap<String, Histogram>,

    provider_api_total: BTreeMap<ProviderApiKey, u64>,
    provider_api_duration: BTreeMap<ProviderApiDurationKey, Histogram>,
    provider_attempts_total: BTreeMap<ProviderApiDurationKey, u64>,
    provider_outcomes_total: BTreeMap<ProviderOutcomeKey, u64>,

    sessions_open: i64,
    turns_active: i64,
    /// Sessions the idle sweeper closed. Visible so an operator can tell an
    /// eviction from a client closing a session, which bumps it not at all.
    sessions_evicted_total: u64,

    turns_total: BTreeMap<String, u64>,
    turn_stops_total: BTreeMap<String, u64>,
    turn_duration: Option<Histogram>,

    tool_calls_total: BTreeMap<ToolKey, u64>,
    tool_duration: BTreeMap<String, Histogram>,
    operation_attempts_total: BTreeMap<String, u64>,
    operation_unknown_effects_total: BTreeMap<OperationUnknownKey, u64>,

    tokens_total: BTreeMap<String, u64>,

    session_context: BTreeMap<SessionContextKey, SessionContextValues>,

    stream_clients: i64,

    tasks_started: u64,
    tasks_finished_total: BTreeMap<String, u64>,
    tasks_running: i64,
    workflow_runs: BTreeMap<String, i64>,
    workflow_steps: BTreeMap<String, i64>,

    schedule_runs_total: BTreeMap<String, u64>,
    schedule_deliveries_total: BTreeMap<ToolKey, u64>,
}

/// The whole metric registry. Every method is `&self`: one registry is shared
/// by every request handler and every turn goroutine.
#[derive(Debug)]
pub struct Metrics {
    state: Mutex<State>,
    logger: Arc<otto_platform::log::Logger>,
    last_process_sample_warning: Mutex<Option<Instant>>,
    /// Unix seconds when this registry was created.
    started_at: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::with_logger(Arc::new(otto_platform::log::Logger::discard()))
    }

    pub fn with_logger(logger: Arc<otto_platform::log::Logger>) -> Self {
        Self {
            state: Mutex::new(State {
                turn_duration: Some(Histogram::new(TURN_TOOL_BUCKETS)),
                ..State::default()
            }),
            logger,
            last_process_sample_warning: Mutex::new(None),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
    }

    fn warn_process_sample_failures(&self, failures: &[&str]) {
        if failures.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut last_warning = self
            .last_process_sample_warning
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last_warning
            .is_some_and(|last| now.duration_since(last) < PROCESS_SAMPLE_WARNING_INTERVAL)
        {
            return;
        }
        *last_warning = Some(now);
        self.logger.warn(
            "process_metrics_sample_incomplete",
            &[("metrics", failures.join(","))],
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn http_request(&self, route: &str, method: &str, status: u16, elapsed: Duration) {
        let mut state = self.lock();
        *state
            .http_total
            .entry((route.to_string(), method.to_string(), status.to_string()))
            .or_default() += 1;
        state
            .http_duration
            .entry(route.to_string())
            .or_insert_with(|| Histogram::new(HTTP_BUCKETS))
            .observe(elapsed.as_secs_f64());
    }

    pub fn provider_api_request(&self, sample: ProviderOperationMetric<'_>) {
        debug_assert!(!sample.operation_id.as_str().is_empty());
        let mut state = self.lock();
        *state
            .provider_api_total
            .entry((
                sample.provider.to_string(),
                sample.model.to_string(),
                sample.status.to_string(),
            ))
            .or_default() += 1;
        *state
            .provider_attempts_total
            .entry((sample.provider.to_string(), sample.model.to_string()))
            .or_default() += u64::from(sample.attempts);
        *state
            .provider_outcomes_total
            .entry((
                sample.provider.to_string(),
                sample.model.to_string(),
                disposition_name(sample.outcome.disposition).to_string(),
                certainty_name(sample.outcome.effect_certainty).to_string(),
                stop_reason_name(sample.outcome.stop_reason).to_string(),
            ))
            .or_default() += 1;
        state
            .provider_api_duration
            .entry((sample.provider.to_string(), sample.model.to_string()))
            .or_insert_with(|| Histogram::new(TURN_TOOL_BUCKETS))
            .observe(sample.elapsed.as_secs_f64());
    }

    /// The gauge set is rebuilt on every scrape so a closed session's series
    /// disappears.
    pub fn replace_session_contexts(&self, samples: Vec<SessionContext>) {
        let mut next = BTreeMap::new();
        for sample in samples {
            let values = SessionContextValues {
                context_window: sample.context_window,
                context_input_tokens: if sample.context_input_tokens_present {
                    sample.context_input_tokens
                } else {
                    0
                },
                context_input_tokens_present: sample.context_input_tokens_present,
                context_input_tokens_pending: i64::from(sample.context_input_tokens_pending),
            };
            next.insert((sample.session_id, sample.provider, sample.model), values);
        }
        self.lock().session_context = next;
    }

    pub fn sessions_open(&self, delta: i64) {
        self.lock().sessions_open += delta;
    }

    /// One session closed by the idle sweeper.
    pub fn session_evicted(&self) {
        self.lock().sessions_evicted_total += 1;
    }

    pub fn turn_started(&self) {
        self.lock().turns_active += 1;
    }

    pub fn turn_finished(&self, status: &str, elapsed: Duration) {
        let mut state = self.lock();
        state.turns_active -= 1;
        *state.turns_total.entry(status.to_string()).or_default() += 1;
        if let Some(histogram) = state.turn_duration.as_mut() {
            histogram.observe(elapsed.as_secs_f64());
        }
    }

    /// One turn stopped by a limit; see [`TurnStop`].
    pub fn turn_stopped(&self, reason: TurnStop) {
        *self
            .lock()
            .turn_stops_total
            .entry(reason.as_str().to_string())
            .or_default() += 1;
    }

    pub fn tool_call(&self, tool: &str, is_error: bool, elapsed: Duration) {
        let status = if is_error { "error" } else { "ok" };
        let mut state = self.lock();
        *state
            .tool_calls_total
            .entry((tool.to_string(), status.to_string()))
            .or_default() += 1;
        state
            .tool_duration
            .entry(tool.to_string())
            .or_insert_with(|| Histogram::new(TURN_TOOL_BUCKETS))
            .observe(elapsed.as_secs_f64());
    }

    pub fn operation_attempt(&self, tool: &str) {
        *self
            .lock()
            .operation_attempts_total
            .entry(tool.to_string())
            .or_default() += 1;
    }

    pub fn operation_unknown_effect(&self, tool: &str, disposition: &str) {
        *self
            .lock()
            .operation_unknown_effects_total
            .entry((tool.to_string(), disposition.to_string()))
            .or_default() += 1;
    }

    pub fn tokens(&self, usage: &Usage) {
        let mut state = self.lock();
        if usage.input_tokens != 0 {
            *state.tokens_total.entry("input".to_string()).or_default() +=
                usage.input_tokens as u64;
        }
        if usage.output_tokens != 0 {
            *state.tokens_total.entry("output".to_string()).or_default() +=
                usage.output_tokens as u64;
        }
        if usage.cached_input_tokens != 0 {
            *state
                .tokens_total
                .entry("cached_input".to_string())
                .or_default() += usage.cached_input_tokens as u64;
        }
    }

    pub(crate) fn schedule_run(&self, outcome: &str) {
        *self
            .lock()
            .schedule_runs_total
            .entry(outcome.to_string())
            .or_default() += 1;
    }

    pub(crate) fn schedule_delivery(&self, channel: &str, outcome: &str) {
        *self
            .lock()
            .schedule_deliveries_total
            .entry((channel.to_string(), outcome.to_string()))
            .or_default() += 1;
    }

    pub fn stream_clients(&self, delta: i64) {
        self.lock().stream_clients += delta;
    }

    /// `seen` carries each task id's last-observed phase; a task that is both
    /// unseen and already finished counts as one started and one finished,
    /// because the update signal coalesces.
    pub fn diff_tasks(&self, seen: &mut BTreeMap<String, TaskPhase>, list: &[TaskSample]) {
        let mut state = self.lock();
        for task in list {
            let previous = seen.get(&task.id).copied();
            let was_running = previous == Some(TaskPhase::Running);
            let was_final = previous == Some(TaskPhase::Finished);

            if previous.is_none() {
                state.tasks_started += 1;
            }
            if task.phase == TaskPhase::Running && !was_running {
                state.tasks_running += 1;
            } else if was_running && task.phase != TaskPhase::Running {
                state.tasks_running -= 1;
            }
            if task.phase == TaskPhase::Finished && !was_final {
                *state
                    .tasks_finished_total
                    .entry(task.status.clone())
                    .or_default() += 1;
            }
            seen.insert(task.id.clone(), task.phase);
        }
    }

    pub fn replace_workflows(&self, runs: &[crate::workflow::Run]) {
        let mut run_statuses = BTreeMap::new();
        let mut step_statuses = BTreeMap::new();
        for run in runs {
            *run_statuses
                .entry(run.status.as_str().to_string())
                .or_default() += 1;
            for step in &run.steps {
                *step_statuses
                    .entry(step.status.as_str().to_string())
                    .or_default() += 1;
            }
        }
        let mut state = self.lock();
        state.workflow_runs = run_statuses;
        state.workflow_steps = step_statuses;
    }

    /// The `text/plain; version=0.0.4` body.
    pub fn render(&self) -> String {
        let process_sample = process::sample();
        self.render_with_process_sample(&process_sample)
    }

    fn render_with_process_sample(&self, process_sample: &process::Sample) -> String {
        self.warn_process_sample_failures(&process_sample.failures);
        let state = self.lock();
        let mut out = String::new();
        write_help(
            &mut out,
            "otto_build_info",
            "gauge",
            "Build version of this otto binary; the value is always 1.",
        );
        let _ = writeln!(
            out,
            "otto_build_info{{version={}}} 1",
            quote_label(crate::VERSION)
        );
        write_gauge(
            &mut out,
            "process_start_time_seconds",
            "Start time of the process in Unix seconds.",
            self.started_at as i64,
        );
        process::write(&mut out, process_sample);
        write_counter_http_requests(&mut out, &state.http_total);
        write_histogram_by_label(
            &mut out,
            "otto_http_request_duration_seconds",
            "route",
            "HTTP request duration in seconds.",
            &state.http_duration,
        );
        write_counter_provider_api_requests(&mut out, &state.provider_api_total);
        write_provider_operation_counters(
            &mut out,
            &state.provider_attempts_total,
            &state.provider_outcomes_total,
        );
        write_histogram_provider_api_requests(&mut out, &state.provider_api_duration);
        write_gauge(
            &mut out,
            "otto_sessions_open",
            "Number of sessions currently open.",
            state.sessions_open,
        );
        write_counter(
            &mut out,
            "otto_sessions_evicted_total",
            "Total sessions closed by the idle sweeper, which a later request re-opens.",
            state.sessions_evicted_total,
        );
        write_session_context_gauges(&mut out, &state.session_context);
        write_counter_by_label(
            &mut out,
            "otto_turns_total",
            "status",
            "Total turns by terminal status.",
            &state.turns_total,
        );
        write_counter_by_label(
            &mut out,
            "otto_turn_stops_total",
            "reason",
            "Total turns stopped by a limit, by reason.",
            &state.turn_stops_total,
        );
        write_gauge(
            &mut out,
            "otto_turns_active",
            "Number of turns currently active.",
            state.turns_active,
        );
        write_singleton_histogram(
            &mut out,
            "otto_turn_duration_seconds",
            "Turn duration in seconds.",
            state.turn_duration.as_ref(),
        );
        write_counter_tool_calls(&mut out, &state.tool_calls_total);
        write_operation_counters(
            &mut out,
            &state.operation_attempts_total,
            &state.operation_unknown_effects_total,
        );
        write_histogram_by_label(
            &mut out,
            "otto_tool_call_duration_seconds",
            "tool",
            "Tool call duration in seconds.",
            &state.tool_duration,
        );
        write_counter_by_label(
            &mut out,
            "otto_provider_tokens_total",
            "kind",
            "Total provider tokens by kind.",
            &state.tokens_total,
        );
        write_gauge(
            &mut out,
            "otto_event_stream_clients",
            "Number of connected event stream clients.",
            state.stream_clients,
        );
        write_counter(
            &mut out,
            "otto_tasks_started_total",
            "Total sub-agent tasks started.",
            state.tasks_started,
        );
        write_counter_by_label(
            &mut out,
            "otto_tasks_finished_total",
            "status",
            "Total sub-agent tasks finished by status.",
            &state.tasks_finished_total,
        );
        write_gauge(
            &mut out,
            "otto_tasks_running",
            "Number of sub-agent tasks currently running.",
            state.tasks_running,
        );
        write_gauge_by_label(
            &mut out,
            "otto_workflow_runs",
            "status",
            "Durable workflow runs by current status.",
            &state.workflow_runs,
        );
        write_gauge_by_label(
            &mut out,
            "otto_workflow_steps",
            "status",
            "Durable workflow steps by current status.",
            &state.workflow_steps,
        );
        write_counter_by_label(
            &mut out,
            "otto_schedule_runs_total",
            "outcome",
            "Total scheduled prompt runs by outcome.",
            &state.schedule_runs_total,
        );
        write_help(
            &mut out,
            "otto_schedule_deliveries_total",
            "counter",
            "Total scheduled deliveries by channel and outcome.",
        );
        for ((channel, outcome), value) in &state.schedule_deliveries_total {
            let _ = writeln!(
                out,
                "otto_schedule_deliveries_total{{channel={},outcome={}}} {value}",
                quote_label(channel),
                quote_label(outcome)
            );
        }
        out
    }
}

mod exposition;

use exposition::*;

#[cfg(test)]
mod tests;
