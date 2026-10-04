package metrics

import (
	"context"
	"io"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"
)

func TestRenderEveryFamily(t *testing.T) {
	m := New()
	m.Message("telegram", "rejected")
	m.Turn("telegram", "end_turn", 3*time.Second)
	m.Reply("telegram", "ok")
	m.AgentRestart()
	m.AgentUp(1)
	var b strings.Builder
	m.Render(&b)
	out := b.String()
	for _, want := range []string{
		"# HELP otto_connect_build_info ", "# TYPE otto_connect_build_info gauge",
		`otto_connect_messages_total{platform="telegram",outcome="rejected"} 1`,
		"# TYPE otto_connect_messages_total counter",
		`otto_connect_turns_total{platform="telegram",stop_reason="end_turn"} 1`,
		"# TYPE otto_connect_turn_duration_seconds histogram",
		`otto_connect_turn_duration_seconds_bucket{platform="telegram",le="2.5"} 0`,
		`otto_connect_turn_duration_seconds_bucket{platform="telegram",le="5"} 1`,
		`otto_connect_turn_duration_seconds_bucket{platform="telegram",le="+Inf"} 1`,
		`otto_connect_turn_duration_seconds_sum{platform="telegram"} 3`,
		`otto_connect_replies_total{platform="telegram",outcome="ok"} 1`,
		"otto_connect_agent_restarts_total 1", "# TYPE otto_connect_agent_up gauge", "otto_connect_agent_up 1",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("missing %q in:\n%s", want, out)
		}
	}
}

func TestServeMetricsAndHealthz(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	ln.Close()
	m := New()
	if err := m.Serve(ctx, addr); err != nil {
		t.Fatal(err)
	}
	get := func(path string) (int, string) {
		resp, err := http.Get("http://" + addr + path)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		b, _ := io.ReadAll(resp.Body)
		return resp.StatusCode, string(b)
	}
	if code, body := get("/healthz"); code != 200 || body != `{"status":"ok"}` {
		t.Fatalf("healthz = %d %q", code, body)
	}
	if code, body := get("/metrics"); code != 200 || !strings.Contains(body, "otto_connect_build_info") {
		t.Fatalf("metrics = %d %q", code, body)
	}
}

func TestServeBusyPortReturnsError(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	if err := New().Serve(context.Background(), ln.Addr().String()); err == nil {
		t.Fatal("Serve on a busy port returned nil")
	}
}
