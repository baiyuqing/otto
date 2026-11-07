use super::*;
use std::sync::Arc;

#[derive(Clone)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("UTF-8 log")
    }
}

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
    let operation_id = OperationId::new("provider_op").expect("operation id");
    let outcome = OperationOutcome {
        disposition: OperationDisposition::DeadlineExceeded,
        effect_certainty: EffectCertainty::Unknown,
        stop_reason: Some(OperationStopReason::Deadline),
    };
    metrics.provider_api_request(ProviderOperationMetric {
        provider: "openai-compatible",
        model: "test-model",
        status: "error",
        elapsed: millis(1500),
        operation_id: &operation_id,
        attempts: 2,
        outcome: &outcome,
    });
    assert_lines(
        &metrics.render(),
        &[
            r#"otto_provider_api_requests_total{provider="openai-compatible",model="test-model",status="error"} 1"#,
            r#"otto_provider_attempts_total{provider="openai-compatible",model="test-model"} 2"#,
            r#"otto_provider_operations_total{provider="openai-compatible",model="test-model",disposition="deadline_exceeded",certainty="unknown",stop_reason="deadline"} 1"#,
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
fn build_info_and_start_time_render_with_their_documented_names() {
    let body = Metrics::new().render();
    assert_lines(
        &body,
        &[
            "# TYPE otto_build_info gauge",
            &format!("otto_build_info{{version=\"{}\"}} 1", crate::VERSION),
            "# TYPE process_start_time_seconds gauge",
        ],
    );
    let start = body
        .lines()
        .find_map(|line| line.strip_prefix("process_start_time_seconds "))
        .and_then(|value| value.parse::<u64>().ok())
        .expect("start time sample");
    assert!(start > 1_700_000_000, "{start}");
}

#[test]
fn process_resource_metrics_render_with_their_documented_names() {
    let body = Metrics::new().render();
    for name in [
        "process_resident_memory_bytes",
        "process_virtual_memory_bytes",
        "process_cpu_seconds_total",
        "process_threads",
        "process_open_fds",
    ] {
        assert_lines(&body, &[&format!("# TYPE {name} ")]);
        let value = body
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("missing numeric sample for {name}:\n{body}"));
        assert!(value >= 0.0, "{name}={value}");
    }
}

#[test]
fn process_sample_failures_log_once_during_the_warning_interval() {
    let captured = CapturedLog(Arc::new(Mutex::new(Vec::new())));
    let metrics = Metrics::with_logger(Arc::new(crate::log::Logger::new(Box::new(
        captured.clone(),
    ))));
    let sample = process::Sample {
        failures: vec!["resident_memory", "open_fds"],
        ..process::Sample::default()
    };

    metrics.render_with_process_sample(&sample);
    metrics.render_with_process_sample(&sample);

    let log = captured.text();
    assert_eq!(
        log.matches("msg=process_metrics_sample_incomplete").count(),
        1
    );
    assert!(log.contains("metrics=resident_memory,open_fds"));
}

#[test]
fn schedule_counters_render_with_their_documented_names_and_labels() {
    let metrics = Metrics::new();
    metrics.schedule_run("reply");
    metrics.schedule_run("reply");
    metrics.schedule_delivery("telegram", "error");
    assert_lines(
        &metrics.render(),
        &[
            "# TYPE otto_schedule_runs_total counter",
            "otto_schedule_runs_total{outcome=\"reply\"} 2",
            "# TYPE otto_schedule_deliveries_total counter",
            "otto_schedule_deliveries_total{channel=\"telegram\",outcome=\"error\"} 1",
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
fn the_evicted_sessions_counter_only_grows() {
    let metrics = Metrics::new();
    metrics.session_evicted();
    metrics.session_evicted();
    metrics.sessions_open(-2);
    assert_lines(
        &metrics.render(),
        &["otto_sessions_evicted_total 2", "otto_sessions_open -2"],
    );
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
        ..Default::default()
    });
    let process_sample = process::sample();
    assert_eq!(
        metrics.render_with_process_sample(&process_sample),
        metrics.render_with_process_sample(&process_sample)
    );
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
        created_at: "2026-01-01T00:00:00Z".into(),
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
            source: String::new(),
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
