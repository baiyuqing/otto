package main

import (
	"flag"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"
)

func TestQuantile(t *testing.T) {
	vals := []float64{1, 2, 3, 4, 5}
	if got := quantile(vals, 0.95); got != 5 {
		t.Fatalf("q95 got %v", got)
	}
	if got := quantile(vals, 0.99); got != 5 {
		t.Fatalf("q99 got %v", got)
	}
	if got := quantile(vals, 0.5); got != 3 {
		t.Fatalf("q50 got %v", got)
	}
}

func TestHistogramQuantile(t *testing.T) {
	h := newHistogram([]float64{1, 2, 5})
	h.observe(0.8)
	h.observe(1.2)
	h.observe(10)

	s := h.snapshot()
	if s.count != 3 {
		t.Fatalf("count=%d", s.count)
	}
	if got := s.quantile(0.95); got != 10 {
		t.Fatalf("q95=%v", got)
	}
}

func TestWritePrometheusMetrics(t *testing.T) {
	m := newMetrics(time.Now().Add(-2 * time.Second))
	m.record(1200*time.Microsecond, nil)
	m.record(3*time.Millisecond, nil)
	m.record(0, assertErr{})

	rr := httptest.NewRecorder()
	writePrometheusMetrics(rr, m)
	out := rr.Body.String()

	for _, want := range []string{
		"mysqlbench_success_total 2",
		"mysqlbench_failure_total 1",
		"mysqlbench_latency_ms_count 2",
		"mysqlbench_memory_alloc_bytes",
	} {
		if !strings.Contains(out, want) {
			t.Fatalf("missing %q in output: %s", want, out)
		}
	}
}

type assertErr struct{}

func (assertErr) Error() string { return "boom" }

func TestParseFlagsConnectionMode(t *testing.T) {
	originalArgs := os.Args
	originalFlagSet := flag.CommandLine
	defer func() {
		os.Args = originalArgs
		flag.CommandLine = originalFlagSet
	}()

	flag.CommandLine = flag.NewFlagSet(os.Args[0], flag.ContinueOnError)
	os.Args = []string{"mysqlbench", "-connection-mode", connectionModePerTxn, "-duration", "1s", "-report-interval", "1s"}
	cfg, err := parseFlags()
	if err != nil {
		t.Fatalf("parseFlags returned error: %v", err)
	}
	if cfg.connectionMode != connectionModePerTxn {
		t.Fatalf("connectionMode=%q", cfg.connectionMode)
	}

	flag.CommandLine = flag.NewFlagSet(os.Args[0], flag.ContinueOnError)
	os.Args = []string{"mysqlbench", "-connection-mode", "bad-mode", "-duration", "1s", "-report-interval", "1s"}
	if _, err := parseFlags(); err == nil {
		t.Fatal("expected invalid connection-mode error")
	}
}

func TestParseFlagsDSNCombinedArgument(t *testing.T) {
	originalArgs := os.Args
	originalFlagSet := flag.CommandLine
	defer func() {
		os.Args = originalArgs
		flag.CommandLine = originalFlagSet
	}()

	wantDSN := "mock.user:mock_password@tcp(127.0.0.1:3306)/test"
	flag.CommandLine = flag.NewFlagSet(os.Args[0], flag.ContinueOnError)
	os.Args = []string{
		"mysqlbench",
		"-dsn '" + wantDSN + "'",
		"-duration", "1s",
		"-report-interval", "1s",
	}

	cfg, err := parseFlags()
	if err != nil {
		t.Fatalf("parseFlags returned error: %v", err)
	}
	if cfg.dsn != wantDSN {
		t.Fatalf("dsn=%q want=%q", cfg.dsn, wantDSN)
	}
	if gotUser := strings.SplitN(cfg.dsn, ":", 2)[0]; gotUser != "mock.user" {
		t.Fatalf("username=%q", gotUser)
	}
}

func TestParseFlagsDSNQuotedValue(t *testing.T) {
	originalArgs := os.Args
	originalFlagSet := flag.CommandLine
	defer func() {
		os.Args = originalArgs
		flag.CommandLine = originalFlagSet
	}()

	wantDSN := "mock.user:mock_password@tcp(127.0.0.1:3306)/test"
	flag.CommandLine = flag.NewFlagSet(os.Args[0], flag.ContinueOnError)
	os.Args = []string{
		"mysqlbench",
		"-dsn", "'" + wantDSN + "'",
		"-duration", "1s",
		"-report-interval", "1s",
	}

	cfg, err := parseFlags()
	if err != nil {
		t.Fatalf("parseFlags returned error: %v", err)
	}
	if cfg.dsn != wantDSN {
		t.Fatalf("dsn=%q want=%q", cfg.dsn, wantDSN)
	}
}
