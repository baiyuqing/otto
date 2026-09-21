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

use otto_core::model::Usage;

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

#[derive(Debug, Clone, Default)]
struct SessionContextValues {
    context_window: i64,
    context_input_tokens: i64,
    context_input_tokens_present: bool,
    context_input_tokens_pending: i64,
}

type HttpKey = (String, String, String);
type ToolKey = (String, String);
type ProviderApiKey = (String, String, String);
type ProviderApiDurationKey = (String, String);
type SessionContextKey = (String, String, String);

#[derive(Debug, Default)]
struct State {
    http_total: BTreeMap<HttpKey, u64>,
    http_duration: BTreeMap<String, Histogram>,

    provider_api_total: BTreeMap<ProviderApiKey, u64>,
    provider_api_duration: BTreeMap<ProviderApiDurationKey, Histogram>,

    sessions_open: i64,
    turns_active: i64,

    turns_total: BTreeMap<String, u64>,
    turn_duration: Option<Histogram>,

    tool_calls_total: BTreeMap<ToolKey, u64>,
    tool_duration: BTreeMap<String, Histogram>,

    tokens_total: BTreeMap<String, u64>,

    session_context: BTreeMap<SessionContextKey, SessionContextValues>,

    stream_clients: i64,

    tasks_started: u64,
    tasks_finished_total: BTreeMap<String, u64>,
    tasks_running: i64,
    workflow_runs: BTreeMap<String, i64>,
    workflow_steps: BTreeMap<String, i64>,
}

/// The whole metric registry. Every method is `&self`: one registry is shared
/// by every request handler and every turn goroutine.
#[derive(Debug)]
pub struct Metrics {
    state: Mutex<State>,
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

    pub fn provider_api_request(
        &self,
        provider: &str,
        model: &str,
        status: &str,
        elapsed: Duration,
    ) {
        let mut state = self.lock();
        *state
            .provider_api_total
            .entry((provider.to_string(), model.to_string(), status.to_string()))
            .or_default() += 1;
        state
            .provider_api_duration
            .entry((provider.to_string(), model.to_string()))
            .or_insert_with(|| Histogram::new(TURN_TOOL_BUCKETS))
            .observe(elapsed.as_secs_f64());
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
        write_counter_http_requests(&mut out, &state.http_total);
        write_histogram_by_label(
            &mut out,
            "otto_http_request_duration_seconds",
            "route",
            "HTTP request duration in seconds.",
            &state.http_duration,
        );
        write_counter_provider_api_requests(&mut out, &state.provider_api_total);
        write_histogram_provider_api_requests(&mut out, &state.provider_api_duration);
        write_gauge(
            &mut out,
            "otto_sessions_open",
            "Number of sessions currently open.",
            state.sessions_open,
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
mod tests {
    use super::*;
    use std::sync::Arc;

    fn millis(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[track_caller]
    fn assert_lines(body: &str, wanted: &[&str]) {
        for line in wanted {
            assert!(
                body.contains(line),
                "missing line {line:?} in body:\n{body}"
            );
        }
    }

    #[test]
    fn provider_api_requests_count_and_bucket() {
        let metrics = Metrics::new();
        metrics.provider_api_request("openai-compatible", "test-model", "ok", millis(1500));
        assert_lines(
            &metrics.render(),
            &[
                r#"otto_provider_api_requests_total{provider="openai-compatible",model="test-model",status="ok"} 1"#,
                r#"otto_provider_api_request_duration_seconds_bucket{provider="openai-compatible",model="test-model",le="2.5"} 1"#,
                r#"otto_provider_api_request_duration_seconds_bucket{provider="openai-compatible",model="test-model",le="+Inf"} 1"#,
                r#"otto_provider_api_request_duration_seconds_count{provider="openai-compatible",model="test-model"} 1"#,
            ],
        );
    }

    #[test]
    fn session_context_renders_one_gauge_per_field() {
        let metrics = Metrics::new();
        metrics.replace_session_contexts(vec![SessionContext {
            session_id: "session-1".to_string(),
            provider: "openai-compatible".to_string(),
            model: "test-model".to_string(),
            context_window: 128000,
            context_input_tokens: 42000,
            context_input_tokens_present: true,
            context_input_tokens_pending: true,
        }]);
        let labels = r#"{session_id="session-1",provider="openai-compatible",model="test-model"}"#;
        assert_lines(
            &metrics.render(),
            &[
                &format!("otto_session_context_window_tokens{labels} 128000"),
                &format!("otto_session_context_input_tokens{labels} 42000"),
                &format!("otto_session_context_input_tokens_pending{labels} 1"),
            ],
        );
    }

    #[test]
    fn http_requests_skip_buckets_below_the_observation() {
        let metrics = Metrics::new();
        metrics.http_request("/v1/sessions", "POST", 201, millis(12));
        let body = metrics.render();
        assert_lines(
            &body,
            &[
                r#"otto_http_requests_total{route="/v1/sessions",method="POST",status="201"} 1"#,
                r#"otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="0.025"} 1"#,
                r#"otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="+Inf"} 1"#,
                r#"otto_http_request_duration_seconds_count{route="/v1/sessions"} 1"#,
            ],
        );
        assert!(
            !body.contains(
                r#"otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="0.01"} 1"#
            ),
            "bucket le=0.01 must not count a 0.012s observation:\n{body}"
        );
    }

    #[test]
    fn a_turn_raises_then_lowers_the_active_gauge() {
        let metrics = Metrics::new();
        metrics.turn_started();
        assert_lines(&metrics.render(), &["otto_turns_active 1"]);

        metrics.turn_finished("ok", millis(1500));
        assert_lines(
            &metrics.render(),
            &[
                "otto_turns_active 0",
                r#"otto_turns_total{status="ok"} 1"#,
                r#"otto_turn_duration_seconds_bucket{le="2.5"} 1"#,
                r#"otto_turn_duration_seconds_bucket{le="+Inf"} 1"#,
                "otto_turn_duration_seconds_count 1",
            ],
        );
    }

    #[test]
    fn tool_calls_are_labelled_by_tool_and_status() {
        let metrics = Metrics::new();
        metrics.tool_call("read", false, millis(50));
        metrics.tool_call("bash", true, millis(200));
        assert_lines(
            &metrics.render(),
            &[
                r#"otto_tool_calls_total{tool="read",status="ok"} 1"#,
                r#"otto_tool_calls_total{tool="bash",status="error"} 1"#,
                r#"otto_tool_call_duration_seconds_count{tool="read"} 1"#,
                r#"otto_tool_call_duration_seconds_count{tool="bash"} 1"#,
            ],
        );
    }

    #[test]
    fn zero_valued_token_kinds_are_skipped() {
        let metrics = Metrics::new();
        metrics.tokens(&Usage {
            input_tokens: 12,
            ..Usage::default()
        });
        let body = metrics.render();
        assert_lines(&body, &[r#"otto_provider_tokens_total{kind="input"} 12"#]);
        assert!(
            !body.contains(r#"kind="output""#) && !body.contains(r#"kind="cached_input""#),
            "zero-valued kinds must be skipped:\n{body}"
        );
    }

    #[test]
    fn gauges_render_at_zero() {
        let body = Metrics::new().render();
        assert_lines(
            &body,
            &[
                "otto_sessions_open 0",
                "otto_turns_active 0",
                "otto_event_stream_clients 0",
            ],
        );
    }

    #[test]
    fn an_untouched_counter_family_emits_only_help_and_type() {
        let body = Metrics::new().render();
        assert!(
            !body.contains("otto_http_requests_total{"),
            "untouched counter family must have no samples:\n{body}"
        );
        assert_lines(
            &body,
            &[
                "# HELP otto_http_requests_total",
                "# TYPE otto_http_requests_total counter",
            ],
        );
    }

    #[test]
    fn the_open_sessions_gauge_sums_its_deltas() {
        let metrics = Metrics::new();
        metrics.sessions_open(1);
        metrics.sessions_open(1);
        metrics.sessions_open(-1);
        assert_lines(&metrics.render(), &["otto_sessions_open 1"]);
    }

    #[test]
    fn label_values_escape_quotes() {
        let metrics = Metrics::new();
        metrics.http_request(r#"/v1/sessions/"weird""#, "GET", 200, millis(1));
        assert_lines(&metrics.render(), &[r#"route="/v1/sessions/\"weird\"""#]);
    }

    #[test]
    fn rendering_is_deterministic() {
        let metrics = Metrics::new();
        metrics.http_request("/v1/sessions", "POST", 201, millis(10));
        metrics.http_request("/v1/sessions/{id}", "GET", 200, millis(5));
        metrics.tool_call("read", false, millis(20));
        metrics.tokens(&Usage {
            input_tokens: 5,
            output_tokens: 3,
            cached_input_tokens: 1,
        });
        assert_eq!(metrics.render(), metrics.render());
    }

    #[test]
    fn concurrent_http_requests_are_all_counted() {
        let metrics = Arc::new(Metrics::new());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let metrics = Arc::clone(&metrics);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        metrics.http_request("/v1/sessions", "POST", 201, millis(1));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker");
        }
        assert_lines(
            &metrics.render(),
            &[r#"otto_http_requests_total{route="/v1/sessions",method="POST",status="201"} 400"#],
        );
    }

    #[test]
    fn workflow_gauges_are_rebuilt_from_durable_state() {
        let metrics = Metrics::new();
        metrics.replace_workflows(&[crate::workflow::Run {
            id: "run-1".into(),
            workflow: "review".into(),
            workspace: "/w".into(),
            profile: "default".into(),
            provider: "openai-compatible".into(),
            model: "model".into(),
            input: String::new(),
            forked_from_run_id: None,
            forked_from_event_seq: None,
            forked_from_step_id: None,
            status: crate::workflow::RunStatus::Waiting,
            steps: vec![crate::workflow::StepRecord {
                id: "approve".into(),
                kind: crate::workflow::StepKind::Approval,
                agent: String::new(),
                prompt: "Ship?".into(),
                needs: Vec::new(),
                status: crate::workflow::StepStatus::Waiting,
                attempt: 0,
                result: String::new(),
                error: String::new(),
                transcript_path: String::new(),
                source_run_id: None,
                source_step_id: None,
                source_attempt: None,
            }],
            definition: crate::workflow::Definition {
                name: "review".into(),
                description: String::new(),
                steps: Vec::new(),
                agents: Vec::new(),
                hash: "0".repeat(64),
            },
        }]);
        assert_lines(
            &metrics.render(),
            &[
                r#"otto_workflow_runs{status="waiting"} 1"#,
                r#"otto_workflow_steps{status="waiting"} 1"#,
            ],
        );
    }
}
