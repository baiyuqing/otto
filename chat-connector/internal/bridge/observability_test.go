package bridge

import (
	"bytes"
	"log/slog"
	"net/http"
	"regexp"
	"strings"
	"sync"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/metrics"
)

type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.String()
}

// captureLogs routes slog.Default to a buffer for the test.
func captureLogs(t *testing.T) *syncBuffer {
	t.Helper()
	buf := &syncBuffer{}
	old := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(buf, nil)))
	t.Cleanup(func() { slog.SetDefault(old) })
	return buf
}

func renderMetrics(m *metrics.Metrics) string {
	var b strings.Builder
	m.Render(&b)
	return b.String()
}

func logLinesWith(logs, event string) []string {
	var out []string
	for _, l := range strings.Split(logs, "\n") {
		if strings.Contains(l, "msg="+event+" ") || strings.HasSuffix(l, "msg="+event) {
			out = append(out, l)
		}
	}
	return out
}

// An admitted message carries one request_id through every log event of its
// turn and into the ACP prompt, also when the agent has no channelContext.
func TestRequestIDThroughLogsAndPrompt(t *testing.T) {
	for _, env := range [][]string{nil, {"FAKE_NO_CHANNEL_CONTEXT=1"}} {
		logs := captureLogs(t)
		m := metrics.New()
		h := newHarness(t, setup{platform: "telegram", env: env, mod: func(o *Options) { o.Metrics = m }})
		h.say("hello")
		h.waitSent(1)
		h.waitFor(func() bool { return len(logLinesWith(logs.String(), "chat_reply_sent")) == 1 }, "chat_reply_sent")

		line := logLinesWith(logs.String(), "chat_received")
		if len(line) != 1 {
			t.Fatalf("chat_received lines = %q", line)
		}
		id := regexp.MustCompile(`request_id=([0-9a-f]{32})\b`).FindStringSubmatch(line[0])
		if id == nil {
			t.Fatalf("chat_received has no 32-hex request_id: %q", line[0])
		}
		for _, ev := range []string{"turn_started", "turn_finished", "chat_reply_sent"} {
			ls := logLinesWith(logs.String(), ev)
			if len(ls) != 1 || !strings.Contains(ls[0], "request_id="+id[1]) || !strings.Contains(ls[0], "platform=telegram") || !strings.Contains(ls[0], "chat_id=c1") {
				t.Errorf("%s lines = %q, want request_id=%s", ev, ls, id[1])
			}
		}
		if got := h.calls("request_id:"); len(got) != 1 || got[0] != "request_id:"+id[1] {
			t.Errorf("prompt request id calls = %q, want %s", got, id[1])
		}
		if ls := logLinesWith(logs.String(), "turn_finished"); len(ls) == 1 && !strings.Contains(ls[0], "stop_reason=end_turn") {
			t.Errorf("turn_finished = %q", ls[0])
		}
		out := renderMetrics(m)
		for _, want := range []string{
			`otto_connect_messages_total{platform="telegram",outcome="accepted"} 1`,
			`otto_connect_turns_total{platform="telegram",stop_reason="end_turn"} 1`,
			`otto_connect_turn_duration_seconds_count{platform="telegram"} 1`,
			`otto_connect_replies_total{platform="telegram",outcome="ok"} 1`,
		} {
			if !strings.Contains(out, want) {
				t.Errorf("metrics lack %q:\n%s", want, out)
			}
		}
		if strings.Contains(logs.String(), "hello") {
			t.Errorf("message text was logged:\n%s", logs.String())
		}
	}
}

func TestRejectedMessageLogsAndCounts(t *testing.T) {
	logs := captureLogs(t)
	m := metrics.New()
	h := newHarness(t, setup{platform: "telegram", mod: func(o *Options) { o.Metrics = m }})
	h.plat.deliver(h.msg("other", "u1", "secret text"))
	settle()
	ls := logLinesWith(logs.String(), "chat_rejected")
	if len(ls) != 1 || !strings.Contains(ls[0], "chat_id=other") || !strings.Contains(ls[0], "sender_id=u1") || strings.Contains(ls[0], "request_id") {
		t.Fatalf("chat_rejected lines = %q", ls)
	}
	if strings.Contains(logs.String(), "secret text") {
		t.Fatal("rejected message text was logged")
	}
	if out := renderMetrics(m); !strings.Contains(out, `otto_connect_messages_total{platform="telegram",outcome="rejected"} 1`) {
		t.Fatalf("metrics:\n%s", out)
	}
	if len(h.plat.texts()) != 0 {
		t.Fatalf("rejected message produced sends: %q", h.plat.texts())
	}
}

// A management command uses the request id of the message that carried it:
// the same id appears in chat_received, the management log and the
// X-Request-Id header sent to otto serve.
func TestManagementUsesMessageRequestID(t *testing.T) {
	logs := captureLogs(t)
	header := make(chan string, 1)
	client := managementServer(t, func(w http.ResponseWriter, r *http.Request) {
		header <- r.Header.Get("X-Request-Id")
		w.WriteHeader(http.StatusInternalServerError)
	})
	h := newHarness(t, setup{platform: "telegram", manage: client})
	h.say("/config use work")
	h.waitSent(1)
	id := <-header
	if !regexp.MustCompile(`^[0-9a-f]{32}$`).MatchString(id) {
		t.Fatalf("header request id = %q", id)
	}
	for _, ev := range []string{"chat_received", "management_failed"} {
		ls := logLinesWith(logs.String(), ev)
		if len(ls) != 1 || !strings.Contains(ls[0], "request_id="+id) || !strings.Contains(ls[0], "chat_id=c1") {
			t.Errorf("%s lines = %q, want request_id=%s", ev, ls, id)
		}
	}
}
