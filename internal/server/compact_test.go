package server

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

// compactRunner is runnerFunc with a configurable Compact.
type compactRunner struct {
	run     func(context.Context, string, func(agent.Event)) error
	compact func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)
}

func (r compactRunner) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	return r.run(ctx, text, emit)
}

func (r compactRunner) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	return r.compact(ctx, focus, emit)
}

func newCompactServer(t *testing.T, runner compactRunner) (*Server, string, *httptest.Server) {
	t.Helper()
	if runner.run == nil {
		runner.run = noopRun
	}
	s, ts := newServerForTest(t, Options{Create: func(context.Context) (*app.Controller, error) {
		return newTestControllerRunner(t, "s1", runner), nil
	}})
	var created struct {
		ID string `json:"id"`
	}
	decodeJSON(t, doJSON(t, ts, "POST", "/v1/sessions", nil), &created)
	return s, created.ID, ts
}

func TestCompactReturnsSharedPayload(t *testing.T) {
	var gotFocus atomic.Value
	_, id, ts := newCompactServer(t, compactRunner{compact: func(_ context.Context, focus string, _ func(agent.Event)) (agent.CompactionResult, error) {
		gotFocus.Store(focus)
		return agent.CompactionResult{
			CheckpointID: "cp-1", Reason: agent.CompactionManual, TokensBefore: 900, EstimatedTokensAfter: 300,
			Usage: model.Usage{InputTokens: 10, OutputTokens: 4}, UsagePresent: true,
		}, nil
	}})

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", map[string]any{"focus": "keep the plan"})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	var got wireCompaction
	decodeJSON(t, resp, &got)
	if got.CheckpointID != "cp-1" || got.Reason != string(agent.CompactionManual) || got.TokensBefore != 900 || got.EstimatedTokensAfter != 300 || got.Noop {
		t.Fatalf("payload = %+v", got)
	}
	if got.Usage == nil || got.Usage.InputTokens != 10 {
		t.Fatalf("usage = %+v, want input_tokens 10", got.Usage)
	}
	if f, _ := gotFocus.Load().(string); f != "keep the plan" {
		t.Fatalf("focus = %q, want it passed through", f)
	}
}

func TestCompactNoopAndEmptyBody(t *testing.T) {
	_, id, ts := newCompactServer(t, compactRunner{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{Noop: true}, nil
	}})
	resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	var got wireCompaction
	decodeJSON(t, resp, &got)
	if !got.Noop {
		t.Fatalf("payload = %+v, want noop", got)
	}
}

func TestCompactFailure(t *testing.T) {
	_, id, ts := newCompactServer(t, compactRunner{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{}, errors.New("summary-failed")
	}})
	resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", nil)
	if resp.StatusCode != http.StatusConflict {
		t.Fatalf("status = %d, want 409", resp.StatusCode)
	}
	var body errorBody
	decodeJSON(t, resp, &body)
	if body.Error.Code != "compaction_failed" || !strings.Contains(body.Error.Message, "summary-failed") {
		t.Fatalf("error = %+v", body.Error)
	}
}

func TestCompactUnknownSession404(t *testing.T) {
	_, ts := newServerForTest(t, Options{})
	resp := doJSON(t, ts, "POST", "/v1/sessions/nope/compact", nil)
	resp.Body.Close()
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", resp.StatusCode)
	}
}

func TestCompactRejectedWhileTurnActive(t *testing.T) {
	release := make(chan struct{})
	started := make(chan struct{})
	var compactCalls atomic.Int64
	_, id, ts := newCompactServer(t, compactRunner{
		run: func(context.Context, string, func(agent.Event)) error {
			close(started)
			<-release
			return nil
		},
		compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
			compactCalls.Add(1)
			return agent.CompactionResult{Noop: true}, nil
		},
	})
	defer close(release)
	go func() {
		resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/turns", map[string]any{"text": "hello", "stream": false})
		_, _ = io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}()
	<-started

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", nil)
	var body errorBody
	decodeJSON(t, resp, &body)
	if resp.StatusCode != http.StatusConflict || body.Error.Code != "turn_active" {
		t.Fatalf("status = %d error = %+v, want 409 turn_active", resp.StatusCode, body.Error)
	}
	if compactCalls.Load() != 0 {
		t.Fatalf("compact calls = %d, want 0", compactCalls.Load())
	}
}

func TestStartTurnRejectedWhileCompacting(t *testing.T) {
	release := make(chan struct{})
	started := make(chan struct{})
	var runCalls atomic.Int64
	_, id, ts := newCompactServer(t, compactRunner{
		run: func(context.Context, string, func(agent.Event)) error {
			runCalls.Add(1)
			return nil
		},
		compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
			close(started)
			<-release
			return agent.CompactionResult{Noop: true}, nil
		},
	})
	defer close(release)
	go func() {
		resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", nil)
		_, _ = io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}()
	<-started

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/turns", map[string]any{"text": "hello", "stream": false})
	var body errorBody
	decodeJSON(t, resp, &body)
	if resp.StatusCode != http.StatusConflict || body.Error.Code != "turn_active" {
		t.Fatalf("status = %d error = %+v, want 409 turn_active", resp.StatusCode, body.Error)
	}
	if runCalls.Load() != 0 {
		t.Fatalf("run calls = %d, want 0", runCalls.Load())
	}
}

func TestDeleteSessionCancelsCompaction(t *testing.T) {
	started := make(chan struct{})
	canceled := make(chan struct{})
	_, id, ts := newCompactServer(t, compactRunner{compact: func(ctx context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
		close(started)
		<-ctx.Done()
		close(canceled)
		return agent.CompactionResult{}, ctx.Err()
	}})
	go func() {
		resp := doJSON(t, ts, "POST", "/v1/sessions/"+id+"/compact", nil)
		_, _ = io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}()
	<-started

	resp := doJSON(t, ts, "DELETE", "/v1/sessions/"+id, nil)
	resp.Body.Close()
	if resp.StatusCode != http.StatusNoContent {
		t.Fatalf("delete status = %d, want 204", resp.StatusCode)
	}
	select {
	case <-canceled:
	case <-time.After(2 * time.Second):
		t.Fatal("compaction context was not canceled by DELETE")
	}
}

func TestOpenAPIDocumentsCompact(t *testing.T) {
	_, ts := newServerForTest(t, Options{})
	body := getBody(t, ts, "/v1/openapi.yaml")
	for _, want := range []string{"/v1/sessions/{id}/compact", "Compaction:", "securitySchemes:"} {
		if !strings.Contains(body, want) {
			t.Errorf("openapi.yaml does not contain %q", want)
		}
	}
}
