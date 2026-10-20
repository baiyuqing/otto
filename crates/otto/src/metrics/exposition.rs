//! Prometheus text exposition writers for `Metrics::render`.

use super::*;

pub(super) fn write_help(out: &mut String, name: &str, metric_type: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {metric_type}");
}

pub(super) fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub(super) fn quote_label(value: &str) -> String {
    format!("\"{}\"", escape_label_value(value))
}

/// The shortest round-trip decimal, switching to exponent form when the leading
/// digit's power of ten is below -4 or at least 21.
pub(super) fn format_float(value: f64) -> String {
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

pub(super) fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    write_help(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

pub(super) fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    write_help(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

pub(super) fn write_counter_by_label(
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

pub(super) fn write_gauge_by_label(
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

pub(super) fn write_counter_http_requests(out: &mut String, data: &BTreeMap<HttpKey, u64>) {
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

pub(super) fn write_counter_provider_api_requests(
    out: &mut String,
    data: &BTreeMap<ProviderApiKey, u64>,
) {
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

pub(super) fn disposition_name(value: OperationDisposition) -> &'static str {
    match value {
        OperationDisposition::Succeeded => "succeeded",
        OperationDisposition::Error => "error",
        OperationDisposition::Cancelled => "cancelled",
        OperationDisposition::DeadlineExceeded => "deadline_exceeded",
        OperationDisposition::Interrupted => "interrupted",
    }
}

pub(super) fn certainty_name(value: EffectCertainty) -> &'static str {
    match value {
        EffectCertainty::NotStarted => "not_started",
        EffectCertainty::KnownNoEffect => "known_no_effect",
        EffectCertainty::Completed => "completed",
        EffectCertainty::Unknown => "unknown",
    }
}

pub(super) fn stop_reason_name(value: Option<OperationStopReason>) -> &'static str {
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

pub(super) fn write_provider_operation_counters(
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

pub(super) fn write_counter_tool_calls(out: &mut String, data: &BTreeMap<ToolKey, u64>) {
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

pub(super) fn write_operation_counters(
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

pub(super) fn session_context_labels(key: &SessionContextKey) -> String {
    format!(
        "session_id={},provider={},model={}",
        quote_label(&key.0),
        quote_label(&key.1),
        quote_label(&key.2)
    )
}

pub(super) fn write_session_context_gauges(
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

pub(super) fn write_histogram_samples(
    out: &mut String,
    name: &str,
    labels: &str,
    histogram: &Histogram,
) {
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

pub(super) fn write_singleton_histogram(
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

pub(super) fn write_histogram_by_label(
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

pub(super) fn write_histogram_provider_api_requests(
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
