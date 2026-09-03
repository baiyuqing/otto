package server

import (
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

func render(t *testing.T, m *metrics) string {
	t.Helper()
	rec := httptest.NewRecorder()
	m.ServeHTTP(rec, httptest.NewRequest("GET", "/metrics", nil))
	if ct := rec.Header().Get("Content-Type"); ct != "text/plain; version=0.0.4; charset=utf-8" {
		t.Fatalf("Content-Type = %q", ct)
	}
	return rec.Body.String()
}

func TestMetricsHTTPRequests(t *testing.T) {
	m := newMetrics()
	m.httpRequest("/v1/sessions", "POST", 201, 12*time.Millisecond)
	body := render(t, m)

	wantLines := []string{
		`otto_http_requests_total{route="/v1/sessions",method="POST",status="201"} 1`,
		`otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="0.025"} 1`,
		`otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="+Inf"} 1`,
		`otto_http_request_duration_seconds_count{route="/v1/sessions"} 1`,
	}
	for _, want := range wantLines {
		if !strings.Contains(body, want) {
			t.Errorf("missing line %q in body:\n%s", want, body)
		}
	}
	// A bucket below the observed value must not have been incremented.
	if strings.Contains(body, `otto_http_request_duration_seconds_bucket{route="/v1/sessions",le="0.01"} 1`) {
		t.Errorf("bucket le=0.01 should not count a 0.012s observation:\n%s", body)
	}
}

func TestMetricsTurns(t *testing.T) {
	m := newMetrics()
	m.turnStarted()
	body := render(t, m)
	if !strings.Contains(body, "otto_turns_active 1") {
		t.Errorf("expected otto_turns_active 1, got:\n%s", body)
	}

	m.turnFinished("ok", 1500*time.Millisecond)
	body = render(t, m)
	wantLines := []string{
		"otto_turns_active 0",
		`otto_turns_total{status="ok"} 1`,
		`otto_turn_duration_seconds_bucket{le="2.5"} 1`,
		`otto_turn_duration_seconds_bucket{le="+Inf"} 1`,
		"otto_turn_duration_seconds_count 1",
	}
	for _, want := range wantLines {
		if !strings.Contains(body, want) {
			t.Errorf("missing line %q in body:\n%s", want, body)
		}
	}
}

func TestMetricsToolCalls(t *testing.T) {
	m := newMetrics()
	m.toolCall("read", false, 50*time.Millisecond)
	m.toolCall("bash", true, 200*time.Millisecond)
	body := render(t, m)
	wantLines := []string{
		`otto_tool_calls_total{tool="read",status="ok"} 1`,
		`otto_tool_calls_total{tool="bash",status="error"} 1`,
		`otto_tool_call_duration_seconds_count{tool="read"} 1`,
		`otto_tool_call_duration_seconds_count{tool="bash"} 1`,
	}
	for _, want := range wantLines {
		if !strings.Contains(body, want) {
			t.Errorf("missing line %q in body:\n%s", want, body)
		}
	}
}

func TestMetricsTokensSkipZero(t *testing.T) {
	m := newMetrics()
	m.tokens(model.Usage{InputTokens: 12})
	body := render(t, m)
	if !strings.Contains(body, `otto_provider_tokens_total{kind="input"} 12`) {
		t.Errorf("missing input tokens line:\n%s", body)
	}
	if strings.Contains(body, `kind="output"`) || strings.Contains(body, `kind="cached_input"`) {
		t.Errorf("zero-valued kinds must be skipped:\n%s", body)
	}
}

func TestMetricsGaugesRenderAtZero(t *testing.T) {
	m := newMetrics()
	body := render(t, m)
	for _, want := range []string{"otto_sessions_open 0", "otto_turns_active 0", "otto_event_stream_clients 0"} {
		if !strings.Contains(body, want) {
			t.Errorf("missing gauge line %q in body:\n%s", want, body)
		}
	}
}

func TestMetricsCounterFamilyWithNoSamplesOnlyHelpType(t *testing.T) {
	m := newMetrics()
	body := render(t, m)
	if strings.Contains(body, "otto_http_requests_total{") {
		t.Errorf("untouched counter family must have no samples:\n%s", body)
	}
	if !strings.Contains(body, "# HELP otto_http_requests_total") || !strings.Contains(body, "# TYPE otto_http_requests_total counter") {
		t.Errorf("untouched counter family must still emit HELP/TYPE:\n%s", body)
	}
}

func TestMetricsSessionsOpenGauge(t *testing.T) {
	m := newMetrics()
	m.sessionsOpen(1)
	m.sessionsOpen(1)
	m.sessionsOpen(-1)
	body := render(t, m)
	if !strings.Contains(body, "otto_sessions_open 1") {
		t.Errorf("expected otto_sessions_open 1, got:\n%s", body)
	}
}

func TestMetricsLabelValueEscaping(t *testing.T) {
	m := newMetrics()
	m.httpRequest(`/v1/sessions/"weird"`, "GET", 200, time.Millisecond)
	body := render(t, m)
	if !strings.Contains(body, `route="/v1/sessions/\"weird\""`) {
		t.Errorf("expected escaped quote in label value, got:\n%s", body)
	}
}

func TestMetricsDeterministicOutput(t *testing.T) {
	m := newMetrics()
	m.httpRequest("/v1/sessions", "POST", 201, 10*time.Millisecond)
	m.httpRequest("/v1/sessions/{id}", "GET", 200, 5*time.Millisecond)
	m.toolCall("read", false, 20*time.Millisecond)
	m.tokens(model.Usage{InputTokens: 5, OutputTokens: 3, CachedInputTokens: 1})
	a := render(t, m)
	b := render(t, m)
	if a != b {
		t.Fatalf("rendering is not deterministic:\n--- a ---\n%s\n--- b ---\n%s", a, b)
	}
}

func TestMetricsHTTPRequestRace(t *testing.T) {
	m := newMetrics()
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func(n int) {
			defer wg.Done()
			for i := 0; i < 50; i++ {
				m.httpRequest("/v1/sessions", "POST", 201, time.Millisecond)
			}
		}(g)
	}
	wg.Wait()
	body := render(t, m)
	if !strings.Contains(body, `otto_http_requests_total{route="/v1/sessions",method="POST",status="201"} 400`) {
		t.Errorf("expected 400 total requests, got:\n%s", body)
	}
}
