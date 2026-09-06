package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"strings"
	"sync"
	"testing"
	"time"
)

// lockedBuffer lets the test read serve's stdout while the server goroutine
// is still writing to it.
type lockedBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *lockedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *lockedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

func TestRunListenFlagRequiresServeSubcommand(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	env := testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})

	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", workspace, "--listen", "127.0.0.1:0"}, strings.NewReader(""), &stdout, &stderr, env)
	if want := "otto: --listen requires the serve subcommand\n"; code != 2 || stderr.String() != want {
		t.Fatalf("code = %d, stderr = %q, want code 2, stderr %q", code, stderr.String(), want)
	}
}

func TestRunServeRejectsSocketWithListen(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	env := testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})

	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"serve", "--config", configPath, "--cwd", workspace, "--socket", "/tmp/x.sock", "--listen", "127.0.0.1:0"}, strings.NewReader(""), &stdout, &stderr, env)
	if want := "otto: --socket and --listen cannot be used together\n"; code != 2 || stderr.String() != want {
		t.Fatalf("code = %d, stderr = %q, want code 2, stderr %q", code, stderr.String(), want)
	}
}

func TestRunServeRejectsNonLoopbackListen(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	env := testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})
	deps := deterministicRunDependencies(t)
	deps.subscribeTerminate = func() interruptSubscription { return interruptSubscription{stop: func() {}} }

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"serve", "--config", configPath, "--cwd", workspace, "--listen", "0.0.0.0:0"}, strings.NewReader(""), &stdout, &stderr, env, deps)
	if code != 1 || !strings.Contains(stderr.String(), "loopback") {
		t.Fatalf("code = %d, stderr = %q, want code 1 mentioning loopback", code, stderr.String())
	}
	if stdout.Len() != 0 {
		t.Fatalf("stdout = %q, want nothing printed", stdout.String())
	}
}

// startTCPServe runs `otto serve` with extra args on a loopback port and
// returns the base URL and token parsed from its stdout line.
func startTCPServe(t *testing.T, ctx context.Context, configPath, workspace string, extra ...string) (base, token string, done <-chan int) {
	t.Helper()
	home := t.TempDir()
	env := testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})
	deps := deterministicRunDependencies(t)
	deps.subscribeTerminate = func() interruptSubscription { return interruptSubscription{stop: func() {}} }
	stdout := &lockedBuffer{}
	exited := make(chan int, 1)
	args := append([]string{"serve", "--config", configPath, "--cwd", workspace}, extra...)
	go func() {
		exited <- runWithDependencies(ctx, args, strings.NewReader(""), stdout, io.Discard, env, deps)
	}()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if line, ok := strings.CutPrefix(stdout.String(), "otto serve: "); ok && strings.HasSuffix(line, "\n") {
			u, err := url.Parse(strings.TrimSpace(line))
			if err != nil {
				t.Fatalf("parse startup URL %q: %v", line, err)
			}
			token = u.Query().Get("token")
			if u.Scheme != "http" || !strings.HasPrefix(u.Host, "127.0.0.1:") || u.Path != "/" || len(token) != 32 {
				t.Fatalf("startup URL %q: want http://127.0.0.1:PORT/?token=<32 hex>", line)
			}
			return "http://" + u.Host, token, exited
		}
		select {
		case code := <-exited:
			t.Fatalf("serve exited early with code %d; stdout %q", code, stdout.String())
		default:
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("no startup line on stdout; got %q", stdout.String())
	return "", "", nil
}

func TestRunServeListensOnLoopbackTCP(t *testing.T) {
	workspace := t.TempDir()
	provider := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"content":"served"},"finish_reason":"stop"}]}`)
	}))
	defer provider.Close()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", provider.URL)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	base, token, done := startTCPServe(t, ctx, configPath, workspace, "--listen", "127.0.0.1:0")
	client := &http.Client{}

	resp, err := client.Get(base + "/healthz")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("/healthz without token: status = %d, want 200", resp.StatusCode)
	}

	resp, err = client.Post(base+"/v1/sessions", "application/json", strings.NewReader(`{}`))
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("POST /v1/sessions without token: status = %d, want 401", resp.StatusCode)
	}

	authed := func(method, path, body string) *http.Response {
		req, err := http.NewRequest(method, base+path, strings.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		req.Header.Set("Authorization", "Bearer "+token)
		req.Header.Set("Content-Type", "application/json")
		resp, err := client.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		return resp
	}

	resp = authed("POST", "/v1/sessions", `{}`)
	var created struct {
		ID string `json:"id"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&created); err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusCreated || created.ID == "" {
		t.Fatalf("POST /v1/sessions with token: status = %d id = %q, want 201 and an id", resp.StatusCode, created.ID)
	}

	resp = authed("POST", "/v1/sessions/"+created.ID+"/turns", `{"text":"hello","stream":false}`)
	var turn struct {
		Text string `json:"text"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&turn); err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if turn.Text != "served" {
		t.Fatalf("turn text = %q, want served", turn.Text)
	}

	resp = authed("POST", "/v1/sessions", `{"resume":"`+created.ID+`"}`)
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("resume: status = %d, want 200", resp.StatusCode)
	}

	cancel()
	select {
	case code := <-done:
		if code != 0 {
			t.Fatalf("serve exit code = %d, want 0", code)
		}
	case <-time.After(7 * time.Second):
		t.Fatal("serve did not stop")
	}
}

func TestRunServeListenFromConfig(t *testing.T) {
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	content, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(configPath, append(content, []byte("[server]\nlisten = \"localhost:0\"\n")...), 0o600); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	base, _, done := startTCPServe(t, ctx, configPath, workspace)

	resp, err := http.Get(base + "/")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /: status = %d, want 200", resp.StatusCode)
	}

	cancel()
	select {
	case code := <-done:
		if code != 0 {
			t.Fatalf("serve exit code = %d, want 0", code)
		}
	case <-time.After(7 * time.Second):
		t.Fatal("serve did not stop")
	}
}
