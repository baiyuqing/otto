package bridge

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

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

// TestEndToEndWithOtto runs the real `otto acp` against a fake
// OpenAI-compatible provider. Set OTTO_BIN to the otto binary to run it.
func TestEndToEndWithOtto(t *testing.T) {
	bin := os.Getenv("OTTO_BIN")
	if bin == "" {
		t.Skip("OTTO_BIN is not set")
	}
	if _, err := os.Stat(bin); err != nil {
		t.Fatalf("OTTO_BIN is set but unusable: %v", err)
	}
	bin, _ = filepath.Abs(bin)

	provider := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/chat/completions" {
			http.NotFound(w, r)
			return
		}
		body := make([]byte, 0, 1<<16)
		buf := make([]byte, 4096)
		for {
			n, err := r.Body.Read(buf)
			body = append(body, buf[:n]...)
			if err != nil {
				break
			}
		}
		text, _ := json.Marshal("echo: " + lastUserText(body))
		w.Header().Set("Content-Type", "text/event-stream")
		w.Write([]byte(`data: {"choices":[{"delta":{"content":` + string(text) + `}}]}` + "\n\n" +
			`data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" +
			"data: [DONE]\n\n"))
	}))
	defer provider.Close()

	home := t.TempDir()
	for _, d := range []string{"Library/Caches", ".config/otto"} {
		if err := os.MkdirAll(filepath.Join(home, d), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	cfg := "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"" + provider.URL +
		"\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
	if err := os.WriteFile(filepath.Join(home, ".config/otto/config.toml"), []byte(cfg), 0o600); err != nil {
		t.Fatal(err)
	}
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}

	h := newHarness(t, setup{agent: &agent.Options{
		Command: []string{bin, "acp", "--sandbox", "off"},
		Dir:     workspace,
		Env: []string{
			"HOME=" + home,
			"OTTO_API_KEY=sk-e2e-not-a-real-key",
			"PATH=/usr/bin:/bin:/usr/sbin:/sbin",
			"TMPDIR=" + os.Getenv("TMPDIR"),
		},
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
