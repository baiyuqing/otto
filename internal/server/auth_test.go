package server

import (
	"bytes"
	"context"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
)

const testToken = "test-token-0123456789abcdef"

func newTokenServer(t *testing.T, log *slog.Logger) (*Server, *httptest.Server) {
	t.Helper()
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "ok"})
		emit(agent.Event{Type: agent.EventAgentFinished})
		return nil
	}
	s, ts := newServerForTest(t, Options{
		Token:  testToken,
		Logger: log,
		Create: func(context.Context) (*app.Controller, error) {
			id, _ := newID()
			return newTestController(t, id, run), nil
		},
	})
	return s, ts
}

func doAuth(t *testing.T, ts *httptest.Server, method, path, authorization string) *http.Response {
	t.Helper()
	req, err := http.NewRequest(method, ts.URL+path, strings.NewReader("{}"))
	if err != nil {
		t.Fatal(err)
	}
	if authorization != "" {
		req.Header.Set("Authorization", authorization)
	}
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}

func TestTokenRequiredOnV1(t *testing.T) {
	_, ts := newTokenServer(t, nil)

	resp := doAuth(t, ts, "POST", "/v1/sessions", "")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("no token: status = %d, want 401", resp.StatusCode)
	}
	if got := resp.Header.Get("WWW-Authenticate"); got != "Bearer" {
		t.Fatalf("WWW-Authenticate = %q, want Bearer", got)
	}
	var body errorBody
	decodeJSON(t, resp, &body)
	if body.Error.Code != "unauthorized" {
		t.Fatalf("error code = %q, want unauthorized", body.Error.Code)
	}

	for _, bad := range []string{"Bearer wrong", "Bearer " + testToken + "x", "Basic " + testToken, testToken} {
		resp := doAuth(t, ts, "POST", "/v1/sessions", bad)
		resp.Body.Close()
		if resp.StatusCode != http.StatusUnauthorized {
			t.Errorf("Authorization %q: status = %d, want 401", bad, resp.StatusCode)
		}
	}

	resp = doAuth(t, ts, "POST", "/v1/sessions", "Bearer "+testToken)
	resp.Body.Close()
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("valid token: status = %d, want 201", resp.StatusCode)
	}

	// Query parameters are not an accepted carrier.
	resp = doAuth(t, ts, "GET", "/v1/info?token="+testToken, "")
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("query token: status = %d, want 401", resp.StatusCode)
	}

	for _, open := range []string{"/healthz", "/metrics", "/"} {
		resp, err := ts.Client().Get(ts.URL + open)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			t.Errorf("GET %s without token: status = %d, want 200", open, resp.StatusCode)
		}
	}
}

func TestTokenOnEventsStream(t *testing.T) {
	_, ts := newTokenServer(t, nil)
	var created struct {
		ID string `json:"id"`
	}
	decodeJSON(t, doAuth(t, ts, "POST", "/v1/sessions", "Bearer "+testToken), &created)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+created.ID+"/turns", strings.NewReader(`{"text":"hi","stream":true}`))
	req.Header.Set("Authorization", "Bearer "+testToken)
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("stream turn: status = %d, want 200", resp.StatusCode)
	}
	frames := readAllSSEFrames(t, resp.Body)
	if len(frames) == 0 || frames[len(frames)-1].event != "agent_finished" {
		t.Fatalf("frames = %+v, want a stream ending in agent_finished", frames)
	}
}

func TestNoTokenMeansOpen(t *testing.T) {
	_, ts := newServerForTest(t, Options{Create: func(context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, noopRun), nil
	}})
	resp := doAuth(t, ts, "POST", "/v1/sessions", "")
	resp.Body.Close()
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status = %d, want 201", resp.StatusCode)
	}
}

func TestUnauthorizedIsLoggedWithRoute(t *testing.T) {
	var logBuf bytes.Buffer
	_, ts := newTokenServer(t, slog.New(slog.NewTextHandler(&logBuf, nil)))
	resp := doAuth(t, ts, "POST", "/v1/sessions", "Bearer wrong-"+testToken)
	resp.Body.Close()

	log := logBuf.String()
	if !strings.Contains(log, `route="POST /v1/sessions"`) || !strings.Contains(log, "status=401") {
		t.Fatalf("log lacks route/status:\n%s", log)
	}
	if strings.Contains(log, testToken) {
		t.Fatalf("log contains the token:\n%s", log)
	}
}
