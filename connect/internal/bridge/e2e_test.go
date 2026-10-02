package bridge

import (
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/connect/internal/agent"
)

// lastUserText returns the text of the last user message of a chat
// completions request body.
func lastUserText(body []byte) string {
	var req struct {
		Messages []struct {
			Role    string          `json:"role"`
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if json.Unmarshal(body, &req) != nil {
		return ""
	}
	for i := len(req.Messages) - 1; i >= 0; i-- {
		m := req.Messages[i]
		if m.Role != "user" {
			continue
		}
		var s string
		if json.Unmarshal(m.Content, &s) == nil {
			return s
		}
		var parts []struct {
			Text string `json:"text"`
		}
		if json.Unmarshal(m.Content, &parts) == nil {
			var out []string
			for _, p := range parts {
				out = append(out, p.Text)
			}
			return strings.Join(out, "\n")
		}
	}
	return ""
}

// fakeProvider is a fake OpenAI-compatible provider that answers "echo: <last
// user text>" and records the user texts in arrival order.
type fakeProvider struct {
	*httptest.Server
	mu    sync.Mutex
	texts []string
}

func (p *fakeProvider) seen() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return slices.Clone(p.texts)
}

// newFakeProvider starts the provider; delay, when not nil, returns how long
// to hold the response to a request with the given user text.
func newFakeProvider(t *testing.T, delay func(text string) time.Duration) *fakeProvider {
	t.Helper()
	p := &fakeProvider{}
	p.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/chat/completions" {
			http.NotFound(w, r)
			return
		}
		body, _ := io.ReadAll(r.Body)
		user := lastUserText(body)
		p.mu.Lock()
		p.texts = append(p.texts, user)
		p.mu.Unlock()
		if delay != nil {
			time.Sleep(delay(user))
		}
		text, _ := json.Marshal("echo: " + user)
		w.Header().Set("Content-Type", "text/event-stream")
		w.Write([]byte(`data: {"choices":[{"delta":{"content":` + string(text) + `}}]}` + "\n\n" +
			`data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" +
			"data: [DONE]\n\n"))
	}))
	t.Cleanup(p.Close)
	return p
}

// ottoHome returns a HOME whose otto config points at the provider.
func ottoHome(t *testing.T, providerURL string) string {
	t.Helper()
	home := t.TempDir()
	for _, d := range []string{"Library/Caches", ".config/otto"} {
		if err := os.MkdirAll(filepath.Join(home, d), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	cfg := "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"" + providerURL +
		"\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
	if err := os.WriteFile(filepath.Join(home, ".config/otto/config.toml"), []byte(cfg), 0o600); err != nil {
		t.Fatal(err)
	}
	return home
}

func ottoEnv(home string) []string {
	return []string{
		"HOME=" + home,
		"OTTO_API_KEY=sk-e2e-not-a-real-key",
		"PATH=/usr/bin:/bin:/usr/sbin:/sbin",
		"TMPDIR=" + os.Getenv("TMPDIR"),
	}
}

// ottoBin returns the absolute path of OTTO_BIN, or skips the test.
func ottoBin(t *testing.T) string {
	t.Helper()
	bin := os.Getenv("OTTO_BIN")
	if bin == "" {
		t.Skip("OTTO_BIN is not set")
	}
	if _, err := os.Stat(bin); err != nil {
		t.Fatalf("OTTO_BIN is set but unusable: %v", err)
	}
	bin, _ = filepath.Abs(bin)
	return bin
}

// TestEndToEndWithOtto runs the real `otto acp` against a fake
// OpenAI-compatible provider. Set OTTO_BIN to the otto binary to run it.
func TestEndToEndWithOtto(t *testing.T) {
	bin := ottoBin(t)

	provider := newFakeProvider(t, nil)
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}

	h := newHarness(t, setup{agent: &agent.Options{
		Command: []string{bin, "acp", "--sandbox", "off"},
		Dir:     workspace,
		Env:     ottoEnv(home),
	}})

	h.say("first question")
	h.say("second question")
	got := h.waitSent(2)
	if !slices.Equal(got, []string{"echo: first question", "echo: second question"}) {
		t.Fatalf("replies = %q", got)
	}
	sid := h.store.Session("fake:c1")
	if len(sid) != 32 {
		t.Fatalf("stored session id = %q", sid)
	}

	h.agent.Kill()
	h.say("third question")
	got = h.waitSent(3)
	settle()
	got = h.plat.texts()
	if !slices.Equal(got, []string{"echo: first question", "echo: second question", "echo: third question"}) {
		t.Fatalf("replies after restart = %q (replayed history must not reach the chat)", got)
	}
	if h.store.Session("fake:c1") != sid {
		t.Fatalf("session id changed from %s to %s: the session was not resumed with session/load", sid, h.store.Session("fake:c1"))
	}
}

// TestEndToEndSharedSession binds two chats to one session held by `otto
// serve`, through `otto acp --attach`, and checks that their turns run one
// after the other and that each chat receives only its own reply.
func TestEndToEndSharedSession(t *testing.T) {
	bin := ottoBin(t)
	provider := newFakeProvider(t, func(text string) time.Duration {
		if text == "from one" {
			return 700 * time.Millisecond
		}
		return 0
	})
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	// A short path: unix socket paths are limited to about 100 bytes.
	sockDir, err := os.MkdirTemp("", "otto-e2e")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(sockDir) })
	sock := filepath.Join(sockDir, "s")

	serve := exec.Command(bin, "serve", "--socket", sock, "--cwd", workspace, "--sandbox", "off")
	serve.Dir = workspace
	serve.Env = ottoEnv(home)
	errFile, err := os.Create(filepath.Join(sockDir, "serve.err"))
	if err != nil {
		t.Fatal(err)
	}
	defer errFile.Close()
	serve.Stderr = errFile
	serveStderr := func() string { b, _ := os.ReadFile(errFile.Name()); return string(b) }
	if err := serve.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = serve.Process.Kill()
		_ = serve.Wait()
	})
	deadline := time.Now().Add(20 * time.Second)
	for {
		c, err := net.Dial("unix", sock)
		if err == nil {
			c.Close()
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("otto serve did not listen on %s: %v; stderr:\n%s", sock, err, serveStderr())
		}
		time.Sleep(20 * time.Millisecond)
	}

	h := newHarness(t, setup{
		chats: []string{"c1", "c2"},
		agent: &agent.Options{
			Command: []string{bin, "acp", "--attach", "--socket", sock},
			Dir:     workspace,
			Env:     ottoEnv(home),
		},
	})
	say := func(chat, text string) { h.plat.deliver(h.msg(chat, "u1", text)) }
	repliesOf := func(chat string) []string {
		h.plat.mu.Lock()
		defer h.plat.mu.Unlock()
		var out []string
		for _, s := range h.plat.sent {
			if s.chat == chat {
				out = append(out, s.text)
			}
		}
		return out
	}

	say("c1", "setup")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	if len(sid) != 32 {
		t.Fatalf("stored session id of c1 = %q; sent = %q; serve stderr:\n%s", sid, h.plat.texts(), serveStderr())
	}
	say("c2", "/use "+sid[:8])
	h.waitSent(2)
	if got := repliesOf("c2"); len(got) != 1 || !strings.HasPrefix(got[0], "Using session "+sid) {
		t.Fatalf("c2 reply to /use = %q", got)
	}
	if h.store.Session("fake:c2") != sid {
		t.Fatalf("c2 is bound to %q, want %q", h.store.Session("fake:c2"), sid)
	}

	say("c1", "from one") // the provider holds this response for 700 ms
	h.waitFor(func() bool { return slices.Contains(provider.seen(), "from one") }, "provider request from one")
	say("c2", "from two")
	h.waitSent(4)

	if got, want := repliesOf("c1"), []string{"echo: setup", "echo: from one"}; !slices.Equal(got, want) {
		t.Fatalf("c1 replies = %q, want %q", got, want)
	}
	if got, want := repliesOf("c2")[1:], []string{"echo: from two"}; !slices.Equal(got, want) {
		t.Fatalf("c2 replies = %q, want %q", got, want)
	}
	if got, want := provider.seen(), []string{"setup", "from one", "from two"}; !slices.Equal(got, want) {
		t.Fatalf("provider saw %q, want %q", got, want)
	}
	// c1's reply arrives before c2's: the second turn waits for the first.
	var order []string
	h.plat.mu.Lock()
	for _, s := range h.plat.sent[2:] {
		order = append(order, s.text)
	}
	h.plat.mu.Unlock()
	if !slices.Equal(order, []string{"echo: from one", "echo: from two"}) {
		t.Fatalf("reply order = %q", order)
	}
}
