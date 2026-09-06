package server

import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
)

func TestSandboxReloadAppliesAndReportsNewState(t *testing.T) {
	calls := atomic.Int64{}
	_, ts := newServerForTest(t, Options{
		Create: func(context.Context) (*app.Controller, error) { return newTestController(t, "s1", noopRun), nil },
		ReloadSandbox: func(context.Context) (app.SandboxInfo, error) {
			calls.Add(1)
			return app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true}, nil
		},
	})

	resp := doJSON(t, ts, "POST", "/v1/sandbox/reload", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	var got sandboxWire
	decodeJSON(t, resp, &got)
	want := sandboxWire{Mode: "seatbelt", Network: "denied", BashAvailable: true, Summary: "seatbelt · workspace-write · network denied"}
	if got != want {
		t.Fatalf("body = %#v, want %#v", got, want)
	}
	if calls.Load() != 1 {
		t.Fatalf("reload calls = %d, want 1", calls.Load())
	}
}

func TestSandboxReloadWithoutCapabilityIsNotImplemented(t *testing.T) {
	_, ts := newServerForTest(t, Options{
		Create: func(context.Context) (*app.Controller, error) { return newTestController(t, "s1", noopRun), nil },
	})

	resp := doJSON(t, ts, "POST", "/v1/sandbox/reload", nil)
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusNotImplemented {
		t.Fatalf("status = %d, want 501", resp.StatusCode)
	}
}

func TestSandboxReloadFailureIsReportedAsConflict(t *testing.T) {
	_, ts := newServerForTest(t, Options{
		Create: func(context.Context) (*app.Controller, error) { return newTestController(t, "s1", noopRun), nil },
		ReloadSandbox: func(context.Context) (app.SandboxInfo, error) {
			return app.SandboxInfo{}, errors.New("sandbox reload failed: self-test-failed")
		},
	})

	resp := doJSON(t, ts, "POST", "/v1/sandbox/reload", nil)
	if resp.StatusCode != http.StatusConflict {
		t.Fatalf("status = %d, want 409", resp.StatusCode)
	}
	var body struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	decodeJSON(t, resp, &body)
	if body.Error.Code != "sandbox_reload_failed" || !strings.Contains(body.Error.Message, "self-test-failed") {
		t.Fatalf("error = %#v, want the reload failure", body.Error)
	}
}

// Replacing the sandbox swaps the executor a running bash command holds, so
// the endpoint refuses while any session has a turn in flight.
func TestSandboxReloadRejectedWhileATurnIsActive(t *testing.T) {
	release := make(chan struct{})
	started := make(chan struct{})
	calls := atomic.Int64{}
	_, ts := newServerForTest(t, Options{
		Create: func(context.Context) (*app.Controller, error) {
			return newTestController(t, "s1", func(context.Context, string, func(agent.Event)) error {
				close(started)
				<-release
				return nil
			}), nil
		},
		ReloadSandbox: func(context.Context) (app.SandboxInfo, error) {
			calls.Add(1)
			return app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true}, nil
		},
	})
	defer close(release)

	var created struct {
		ID string `json:"id"`
	}
	decodeJSON(t, doJSON(t, ts, "POST", "/v1/sessions", nil), &created)
	// The turn request only returns once the turn ends, so it runs alongside
	// the reload attempt.
	go func() {
		resp := doJSON(t, ts, "POST", "/v1/sessions/"+created.ID+"/turns", map[string]any{"text": "hello", "stream": false})
		_, _ = io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}()
	<-started

	resp := doJSON(t, ts, "POST", "/v1/sandbox/reload", nil)
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusConflict {
		t.Fatalf("status = %d, want 409", resp.StatusCode)
	}
	if calls.Load() != 0 {
		t.Fatalf("reload calls = %d, want 0", calls.Load())
	}
}

func TestOpenAPIDocumentsSandboxReload(t *testing.T) {
	_, ts := newServerForTest(t, Options{})
	body := getBody(t, ts, "/v1/openapi.yaml")
	if !strings.Contains(body, "/v1/sandbox/reload") {
		t.Fatalf("openapi.yaml does not document /v1/sandbox/reload")
	}
}
