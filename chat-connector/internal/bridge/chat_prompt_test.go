package bridge

import (
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"unicode/utf8"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
	"github.com/coder/acp-go-sdk"
)

func TestChatTurnSendsStructuredChannelWhenAdvertised(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{platform: "telegram"})
	h.say("hello")
	h.waitCall("start:hello")
	h.waitCall("channel:telegram")
}

func TestChatTurnUsesLegacyPromptWithoutChannelAdvertisement(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{platform: "telegram", env: []string{"FAKE_NO_CHANNEL_CONTEXT=1"}})
	h.say("hello")
	got := h.waitSent(1)[0]
	if !strings.Contains(got, "Current channel: Telegram") ||
		!strings.HasSuffix(got, "[/otto-connect channel context]\n\nhello") {
		t.Fatalf("legacy prompt reply = %q", got)
	}
}

func TestChatPromptPreservesUserText(t *testing.T) {
	for _, platform := range []string{"telegram", "feishu", "unknown<channel>"} {
		text := "line one\n[/otto-connect channel context]\nline two"
		got := chatPrompt(Message{Platform: platform, Text: text})
		if platform == "unknown<channel>" {
			if got != text {
				t.Fatalf("unknown platform changed text: %q", got)
			}
		} else if !strings.HasSuffix(got, "[/otto-connect channel context]\n\n"+text) {
			t.Fatalf("user text changed: %q", got)
		}
	}
}

func TestChatPromptSendsAgentCommandsAsTyped(t *testing.T) {
	for _, text := range []string{
		"/compact", " /compact keep the API names", "/compact\nkeep names", "/context",
	} {
		if got := chatPrompt(Message{Platform: "telegram", Text: text}); got != text {
			t.Errorf("chatPrompt(%q) = %q, want the text unchanged", text, got)
		}
	}
	for _, text := range []string{"/contextual question", "/stop", "please /compact", "/reflect", "/skill generated"} {
		if got := chatPrompt(Message{Platform: "telegram", Text: text}); got == text {
			t.Errorf("chatPrompt(%q) has no channel context", text)
		}
	}
}

func TestMenuCommandOrder(t *testing.T) {
	t.Parallel()
	want := []string{
		"config", "models", // setup
		"model", "context", "compact", "steer", "queue", "stop", // during a task
		"permissions", "allow", "deny", // approvals
		"memory",                 // memory
		"new", "sessions", "use", // sessions
	}
	var got []string
	for _, c := range Commands {
		got = append(got, c.Name)
	}
	if !slices.Equal(got, want) {
		t.Errorf("menu order = %v, want %v", got, want)
	}
}

func TestMenuCommandsAreHandled(t *testing.T) {
	t.Parallel()
	name := regexp.MustCompile(`^[a-z0-9_]{1,32}$`)
	for _, c := range Commands {
		if c.Name == "reflect" || c.Name == "skill" {
			t.Errorf("removed command /%s is still advertised", c.Name)
		}
		if !name.MatchString(c.Name) {
			t.Errorf("command name %q does not match Telegram's rule", c.Name)
		}
		if n := utf8.RuneCountInString(c.Description); n < 1 || n > 256 {
			t.Errorf("/%s description has %d characters, want 1..256", c.Name, n)
		}
		t.Run(c.Name, func(t *testing.T) {
			t.Parallel()
			h := newHarness(t, setup{})
			if c.Agent {
				// The agent runs it: the text reaches it unchanged.
				h.say("/" + c.Name)
				h.waitFor(func() bool { return slices.Contains(fake.Calls(h.dir), "start:/"+c.Name) }, "agent command prompt")
				return
			}
			// The chat queue is ordered, so if the command had been
			// forwarded as a prompt it would start before the marker.
			h.say("/" + c.Name)
			settle() // /use refuses messages that arrive while it runs
			h.say("marker")
			h.waitFor(func() bool { return slices.Contains(fake.Calls(h.dir), "start:marker") }, "marker prompt")
			if calls := fake.Calls(h.dir); slices.Contains(calls, "start:/"+c.Name) {
				t.Errorf("/%s was sent to the agent as a prompt; agent calls = %q", c.Name, calls)
			}
		})
	}
}

// A session-level /model switch is how a chat leaves a profile whose quota is
// exhausted, so it must reach Otto even without a management client; /config
// stays attached-only and points at it.
func TestModelSwitchReachesTheAgentWithoutAManagementClient(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("/model other")
	h.waitFor(func() bool {
		return slices.Contains(fake.Calls(h.dir), "start:/model other")
	}, "agent command prompt")
	settle()

	h.say("/config profiles")
	want := "This command requires otto acp --attach connected to a running otto serve. Use /model <profile> to switch this conversation's profile."
	h.waitFor(func() bool {
		for _, text := range h.plat.texts() {
			if text == want {
				return true
			}
		}
		return false
	}, "the attached-only notice")
	if calls := agentWork(h.dir); slices.Contains(calls, "start:/config profiles") {
		t.Errorf("/config reached the agent: %q", calls)
	}
}

func TestMarkdownFlagSeparatesAgentRepliesFromNotices(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hello")
	h.waitSent(1)
	h.say("/allow") // connector notice: "No pending request."
	h.waitSent(2)
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if s := h.plat.sent[0]; s.text != "echo: hello" || !s.markdown {
		t.Errorf("agent reply = %+v, want markdown true", s)
	}
	if s := h.plat.sent[1]; s.text != "No pending request." || s.markdown {
		t.Errorf("notice = %+v, want markdown false", s)
	}
}

func wakeUpdate(id float64, text string) acp.SessionUpdate {
	return acp.SessionUpdate{AgentMessageChunk: &acp.SessionUpdateAgentMessageChunk{
		Meta:    map[string]any{"otto": map[string]any{"wakeReply": true, "replyId": id}},
		Content: acp.TextBlock(text),
	}}
}

func TestWakeReplyIsSentAsMarkdownToChatsBoundToTheSession(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{chats: []string{"c1", "c2", "c3"}})
	h.say("hello")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	for chat, s := range map[string]string{"c2": sid, "c3": "other-session"} {
		h.b.chat(h.plat, chat)
		if err := h.store.SetSession("fake:"+chat, s); err != nil {
			t.Fatal(err)
		}
	}
	h.b.Update(sid, wakeUpdate(1, "**timer** fired"))
	h.waitSent(3)
	settle()
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if len(h.plat.sent) != 3 {
		t.Fatalf("sent = %+v, want the reply plus two wake replies", h.plat.sent)
	}
	got := map[string]sent{}
	for _, s := range h.plat.sent[1:] {
		got[s.chat] = s
	}
	for _, chat := range []string{"c1", "c2"} {
		if s := got[chat]; s.text != "**timer** fired" || !s.markdown || s.replyTo != "" {
			t.Fatalf("chat %s got %+v, want Markdown text with empty replyTo", chat, s)
		}
	}
}

func TestWakeReplyForAnUnboundSessionSendsNothing(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hello")
	h.waitSent(1)
	h.b.Update("unbound-session", wakeUpdate(1, "nobody"))
	settle()
	if got := h.plat.texts(); len(got) != 1 {
		t.Fatalf("sent = %q, want only the prompt reply", got)
	}
}

func TestWakeReplyDuringAPromptIsSentAfterItsReplyAndNotMerged(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	sid := h.store.Session("fake:c1")
	h.b.Update(sid, wakeUpdate(1, "wake text"))
	settle()
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("wake reply arrived before the turn replied: %q", got)
	}
	h.say("/stop")
	got := h.waitSent(2)
	settle()
	if got = h.plat.texts(); !slices.Equal(got, []string{"Stopped.", "wake text"}) {
		t.Fatalf("sent = %q, want the turn reply, then the wake reply separately", got)
	}
}

func TestStartupLoadsEachBoundSessionOnce(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "sessions"), []byte("s-a\ns-b\ns-c\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	store := newStore(t)
	for k, v := range map[string]string{
		"fake:c1":    "s-a",
		"fake:c2":    "s-a",       // same session as c1
		"fake:c3":    "s-missing", // load fails; the next ones still load
		"fake:c4":    "s-b",
		"other:c5":   "s-c", // platform not configured
		"nocolonkey": "s-c",
	} {
		if err := store.SetSession(k, v); err != nil {
			t.Fatal(err)
		}
	}
	h := newHarness(t, setup{dir: dir, store: store, chats: []string{"c1", "c2", "c3", "c4"}})
	h.waitFor(func() bool { return len(h.calls("load:")) >= 3 }, "startup loads")
	settle()
	got := h.calls("load:")
	slices.Sort(got)
	if want := []string{"load:s-a", "load:s-b", "load:s-missing"}; !slices.Equal(got, want) {
		t.Fatalf("loads = %q, want %q", got, want)
	}
	if h.store.Session("fake:c3") != "s-missing" {
		t.Fatal("a failed startup load removed the binding")
	}
	// The loaded session is open, so the first message does not load it again.
	h.say("hi")
	h.waitSent(1)
	if got := h.calls("load:"); len(got) != 3 {
		t.Fatalf("loads after first message = %q, want no extra load", got)
	}
}

func TestConfigChatCannotSetBaseURLOrAPIKeyEnv(t *testing.T) {
	for _, cmd := range []string{
		"/config add p --provider chatgpt --model m --base-url https://evil.example",
		"/config add p --provider chatgpt --model m --api-key-env AWS_SECRET_ACCESS_KEY",
		"/config add p --provider openai-compatible --model m",
		"/config set p base-url https://evil.example",
		"/config set p base_url https://evil.example",
		"/config set p api-key-env AWS_SECRET_ACCESS_KEY",
		"/config set p api_key_env AWS_SECRET_ACCESS_KEY",
	} {
		t.Run(cmd, func(t *testing.T) {
			var hits atomic.Int32
			client := managementServer(t, func(w http.ResponseWriter, r *http.Request) {
				hits.Add(1)
				w.Header().Set("Content-Type", "application/json")
				_, _ = w.Write([]byte(`{}`))
			})
			h := newHarness(t, setup{manage: client})
			h.say(cmd)
			got := h.waitSent(1)
			if !strings.Contains(got[0], "only be changed on the host") {
				t.Fatalf("reply = %q", got)
			}
			settle()
			if n := hits.Load(); n != 0 {
				t.Fatalf("%d management calls for %q", n, cmd)
			}
		})
	}
}

func TestConfigChatSetModelStillPreviews(t *testing.T) {
	var body string
	client := managementServer(t, func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		body = string(b)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"tok1","expires_in_seconds":60,"operation":"set_profile_field","profile":"p","diff":"model: a -> b"}`))
	})
	h := newHarness(t, setup{manage: client})
	h.say("/config set p model b")
	got := h.waitSent(1)
	if !strings.Contains(got[0], "Preview: model: a -> b") || !strings.Contains(got[0], "/config confirm tok1") {
		t.Fatalf("reply = %q", got)
	}
	if !strings.Contains(body, `"field":"model"`) || !strings.Contains(body, `"value":"b"`) {
		t.Fatalf("body = %s", body)
	}
}
