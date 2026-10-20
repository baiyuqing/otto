//! Prometheus text exposition for `otto serve`.
//!
//! Every counter, gauge and histogram is hand-rolled: the output is the
//! `version=0.0.4` text format, written in the order and with the label sets
//! the previously released binary used, so an existing scrape config keeps
//! working.
//!
//! Ordering: each label-key map is written in sorted key order. The maps here
//! are [`BTreeMap`]s, so iteration order already gives that without a sort
//! step.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationId, OperationOutcome, OperationStopReason,
    Usage,
};

use crate::app::TaskStatus;

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
        Self {
            state: Mutex::new(State {
                turn_duration: Some(Histogram::new(TURN_TOOL_BUCKETS)),
                ..State::default()
            }),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
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

    pub fn schedule_run(&self, outcome: &str) {
        *self
            .lock()
            .schedule_runs_total
            .entry(outcome.to_string())
            .or_default() += 1;
    }

    pub fn schedule_delivery(&self, channel: &str, outcome: &str) {
        *self
            .lock()
            .schedule_deliveries_total
            .entry((channel.to_string(), outcome.to_string()))
            .or_default() += 1;
    }

    pub fn stream_clients(&self, delta: i64) {
        self.lock().stream_clients += delta;
    }

    /// `seen` carries each task id's last-observed status; a task that is both
    /// unseen and already final counts as one started and one finished, because
    /// the update signal coalesces.
    pub fn diff_tasks(&self, seen: &mut BTreeMap<String, TaskStatus>, list: &[crate::app::Task]) {
        let mut state = self.lock();
        for task in list {
            let previous = seen.get(&task.id).copied();
            let was_running = previous == Some(TaskStatus::Running);
            let was_final = previous.is_some_and(TaskStatus::final_status);

            if previous.is_none() {
                state.tasks_started += 1;
            }
            if task.status == TaskStatus::Running && !was_running {
                state.tasks_running += 1;
            } else if was_running && task.status != TaskStatus::Running {
                state.tasks_running -= 1;
            }
            if task.status.final_status() && !was_final {
                *state
                    .tasks_finished_total
                    .entry(task.status.as_str().to_string())
                    .or_default() += 1;
            }
            seen.insert(task.id.clone(), task.status);
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

fn write_help(out: &mut String, name: &str, metric_type: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {metric_type}");
}

fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn quote_label(value: &str) -> String {
    format!("\"{}\"", escape_label_value(value))
}

/// The shortest round-trip decimal, switching to exponent form when the leading
/// digit's power of ten is below -4 or at least 21.
fn format_float(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !value.is_finite() {
        return format!("{value}");
    }
    let exponent = value.abs().log10().floor() as i32;
    if (-4..21).contains(&exponent) {
        return format!("{value}");
    }
    let mantissa = value / 10f64.powi(exponent);
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    write_help(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    write_help(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

fn write_counter_by_label(
    out: &mut String,
    name: &str,
    label_name: &str,
    help: &str,
    data: &BTreeMap<String, u64>,
) {
    write_help(out, name, "counter", help);
    for (key, value) in data {
        let _ = writeln!(out, "{name}{{{label_name}={}}} {value}", quote_label(key));
    }
}

fn write_gauge_by_label(
    out: &mut String,
    name: &str,
    label_name: &str,
    help: &str,
    data: &BTreeMap<String, i64>,
) {
    write_help(out, name, "gauge", help);
    for (key, value) in data {
        let _ = writeln!(out, "{name}{{{label_name}={}}} {value}", quote_label(key));
    }
}

fn write_counter_http_requests(out: &mut String, data: &BTreeMap<HttpKey, u64>) {
    const NAME: &str = "otto_http_requests_total";
    write_help(out, NAME, "counter", "Total HTTP requests.");
    for ((route, method, status), value) in data {
        let _ = writeln!(
            out,
            "{NAME}{{route={},method={},status={}}} {value}",
            quote_label(route),
            quote_label(method),
            quote_label(status)
        );
    }
}

fn write_counter_provider_api_requests(out: &mut String, data: &BTreeMap<ProviderApiKey, u64>) {
    const NAME: &str = "otto_provider_api_requests_total";
    write_help(
        out,
        NAME,
        "counter",
        "Total provider API requests by provider, model, and status.",
    );
    for ((provider, model, status), value) in data {
        let _ = writeln!(
            out,
            "{NAME}{{provider={},model={},status={}}} {value}",
            quote_label(provider),
            quote_label(model),
            quote_label(status)
        );
    }
}

fn disposition_name(value: OperationDisposition) -> &'static str {
    match value {
        OperationDisposition::Succeeded => "succeeded",
        OperationDisposition::Error => "error",
        OperationDisposition::Cancelled => "cancelled",
        OperationDisposition::DeadlineExceeded => "deadline_exceeded",
        OperationDisposition::Interrupted => "interrupted",
    }
}

fn certainty_name(value: EffectCertainty) -> &'static str {
    match value {
        EffectCertainty::NotStarted => "not_started",
        EffectCertainty::KnownNoEffect => "known_no_effect",
        EffectCertainty::Completed => "completed",
        EffectCertainty::Unknown => "unknown",
    }
}

fn stop_reason_name(value: Option<OperationStopReason>) -> &'static str {
    match value {
        None => "none",
        Some(OperationStopReason::UserCancellation) => "user_cancellation",
        Some(OperationStopReason::Deadline) => "deadline",
        Some(OperationStopReason::Shutdown) => "shutdown",
        Some(OperationStopReason::Migration) => "migration",
        Some(OperationStopReason::TransportLost) => "transport_lost",
        Some(OperationStopReason::ProcessLost) => "process_lost",
    }
}

fn write_provider_operation_counters(
    out: &mut String,
    attempts: &BTreeMap<ProviderApiDurationKey, u64>,
    outcomes: &BTreeMap<ProviderOutcomeKey, u64>,
) {
    const ATTEMPTS: &str = "otto_provider_attempts_total";
    write_help(
        out,
        ATTEMPTS,
        "counter",
        "Total provider transport attempts.",
    );
    for ((provider, model), value) in attempts {
        let _ = writeln!(
            out,
            "{ATTEMPTS}{{provider={},model={}}} {value}",
            quote_label(provider),
            quote_label(model)
        );
    }

    const OUTCOMES: &str = "otto_provider_operations_total";
    write_help(
        out,
        OUTCOMES,
        "counter",
        "Logical provider operations by typed terminal outcome.",
    );
    for ((provider, model, disposition, certainty, stop_reason), value) in outcomes {
        let _ = writeln!(
            out,
            "{OUTCOMES}{{provider={},model={},disposition={},certainty={},stop_reason={}}} {value}",
            quote_label(provider),
            quote_label(model),
            quote_label(disposition),
            quote_label(certainty),
            quote_label(stop_reason)
        );
    }
}

fn write_counter_tool_calls(out: &mut String, data: &BTreeMap<ToolKey, u64>) {
    const NAME: &str = "otto_tool_calls_total";
    write_help(out, NAME, "counter", "Total tool calls by tool and status.");
    for ((tool, status), value) in data {
        let _ = writeln!(
            out,
            "{NAME}{{tool={},status={}}} {value}",
            quote_label(tool),
            quote_label(status)
        );
    }
}

fn write_operation_counters(
    out: &mut String,
    attempts: &BTreeMap<String, u64>,
    unknown: &BTreeMap<OperationUnknownKey, u64>,
) {
    const ATTEMPTS: &str = "otto_operation_attempts_total";
    write_help(
        out,
        ATTEMPTS,
        "counter",
        "Durably recorded operation attempts by kind and tool.",
    );
    for (tool, value) in attempts {
        let _ = writeln!(
            out,
            "{ATTEMPTS}{{kind=\"tool_call\",tool={}}} {value}",
            quote_label(tool)
        );
    }

    const UNKNOWN: &str = "otto_operation_unknown_effects_total";
    write_help(
        out,
        UNKNOWN,
        "counter",
        "Terminal operations with unknown external effects by kind, tool, and disposition.",
    );
    for ((tool, disposition), value) in unknown {
        let _ = writeln!(
            out,
            "{UNKNOWN}{{kind=\"tool_call\",tool={},disposition={}}} {value}",
            quote_label(tool),
            quote_label(disposition)
        );
    }
}

fn session_context_labels(key: &SessionContextKey) -> String {
    format!(
        "session_id={},provider={},model={}",
        quote_label(&key.0),
        quote_label(&key.1),
        quote_label(&key.2)
    )
}

fn write_session_context_gauges(
    out: &mut String,
    data: &BTreeMap<SessionContextKey, SessionContextValues>,
) {
    write_help(
        out,
        "otto_session_context_window_tokens",
        "gauge",
        "Configured context window tokens for each open session.",
    );
    for (key, values) in data {
        let _ = writeln!(
            out,
            "otto_session_context_window_tokens{{{}}} {}",
            session_context_labels(key),
            values.context_window
        );
    }

    write_help(
        out,
        "otto_session_context_input_tokens",
        "gauge",
        "Current context input tokens for each open session when available.",
    );
    for (key, values) in data {
        if !values.context_input_tokens_present {
            continue;
        }
        let _ = writeln!(
            out,
            "otto_session_context_input_tokens{{{}}} {}",
            session_context_labels(key),
            values.context_input_tokens
        );
    }

    write_help(
        out,
        "otto_session_context_input_tokens_pending",
        "gauge",
        "Whether context input token computation is pending for each open session.",
    );
    for (key, values) in data {
        let _ = writeln!(
            out,
            "otto_session_context_input_tokens_pending{{{}}} {}",
            session_context_labels(key),
            values.context_input_tokens_pending
        );
    }
}

fn write_histogram_samples(out: &mut String, name: &str, labels: &str, histogram: &Histogram) {
    let mut cumulative = 0u64;
    for (index, upper) in histogram.buckets.iter().enumerate() {
        cumulative += histogram.bin_counts[index];
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}le={}}} {cumulative}",
            quote_label(&format_float(*upper))
        );
    }
    let _ = writeln!(
        out,
        "{name}_bucket{{{labels}le=\"+Inf\"}} {}",
        histogram.count
    );
    let trimmed = labels.strip_suffix(',').unwrap_or(labels);
    let _ = writeln!(
        out,
        "{name}_sum{{{trimmed}}} {}",
        format_float(histogram.sum)
    );
    let _ = writeln!(out, "{name}_count{{{trimmed}}} {}", histogram.count);
}

fn write_singleton_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    histogram: Option<&Histogram>,
) {
    write_help(out, name, "histogram", help);
    let Some(histogram) = histogram else {
        return;
    };
    if histogram.count == 0 {
        return;
    }
    let mut cumulative = 0u64;
    for (index, upper) in histogram.buckets.iter().enumerate() {
        cumulative += histogram.bin_counts[index];
        let _ = writeln!(
            out,
            "{name}_bucket{{le={}}} {cumulative}",
            quote_label(&format_float(*upper))
        );
    }
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", histogram.count);
    let _ = writeln!(out, "{name}_sum {}", format_float(histogram.sum));
    let _ = writeln!(out, "{name}_count {}", histogram.count);
}

fn write_histogram_by_label(
    out: &mut String,
    name: &str,
    label_name: &str,
    help: &str,
    data: &BTreeMap<String, Histogram>,
) {
    write_help(out, name, "histogram", help);
    for (key, histogram) in data {
        let labels = format!("{label_name}={},", quote_label(key));
        write_histogram_samples(out, name, &labels, histogram);
    }
}

fn write_histogram_provider_api_requests(
    out: &mut String,
    data: &BTreeMap<ProviderApiDurationKey, Histogram>,
) {
    const NAME: &str = "otto_provider_api_request_duration_seconds";
    write_help(
        out,
        NAME,
        "histogram",
        "Provider API request duration in seconds.",
    );
    for ((provider, model), histogram) in data {
        let labels = format!(
            "provider={},model={},",
            quote_label(provider),
            quote_label(model)
        );
        write_histogram_samples(out, NAME, &labels, histogram);
    }
}

#[cfg(test)]
mod tests;
