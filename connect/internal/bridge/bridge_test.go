package bridge

import (
	"fmt"
	"net/http"
	"regexp"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/connect/internal/agent/fake"
)

func TestReplyIsChunkTextInOrder(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("chunks")
	got := h.waitSent(1)
	settle()
	if got = h.plat.texts(); len(got) != 1 || got[0] != "Hello world\n\nAfter tool" {
		t.Fatalf("sent = %q, want one reply %q", got, "Hello world\n\nAfter tool")
	}
	if h.plat.sent[0].chat != "c1" || h.plat.sent[0].replyTo != "m1" {
		t.Fatalf("reply went to %+v", h.plat.sent[0])
	}
	if strings.Contains(got[0], "thinking") {
		t.Fatal("thought chunk reached the chat")
	}
}

func TestSecondMessageWaitsForFirst(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("sleep")
	h.waitCall("start:sleep")
	h.say("second")
	h.waitSent(2)
	if got := h.plat.texts(); !slices.Equal(got, []string{"echo: sleep", "echo: second"}) {
		t.Fatalf("sent = %q", got)
	}
	var seq []string
	for _, c := range fake.Calls(h.dir) {
		if strings.HasPrefix(c, "start:") || strings.HasPrefix(c, "end:") {
			seq = append(seq, c)
		}
	}
	if want := []string{"start:sleep", "end:sleep", "start:second", "end:second"}; !slices.Equal(seq, want) {
		t.Fatalf("agent saw %q, want %q", seq, want)
	}
	if n := len(h.calls("new:")); n != 1 {
		t.Fatalf("session/new called %d times, want 1", n)
	}
}

func TestQueueLimitAndStop(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	for i := 1; i <= 10; i++ {
		h.say(fmt.Sprintf("q%d", i))
	}
	h.say("q11")
	got := h.waitSent(1)
	if !strings.Contains(got[0], "Queue is full") || h.plat.sent[0].replyTo == "" {
		t.Fatalf("11th message got %+v", h.plat.sent[0])
	}
	settle()
	if n := len(h.plat.texts()); n != 1 {
		t.Fatalf("sent = %q", h.plat.texts())
	}

	h.say("/stop")
	h.waitSent(2)
	if got := h.plat.texts(); got[1] != "Stopped." {
		t.Fatalf("sent = %q, want Stopped.", got)
	}
	sid := h.store.Session("fake:c1")
	if c := h.calls("cancel:"); len(c) != 1 || c[0] != "cancel:"+sid {
		t.Fatalf("cancel calls = %q, want cancel:%s", c, sid)
	}
	settle()
	if p := h.calls("start:"); len(p) != 1 {
		t.Fatalf("queued messages reached the agent after /stop: %q", p)
	}
	if n := len(h.plat.texts()); n != 2 {
		t.Fatalf("sent = %q", h.plat.texts())
	}

	// The chat is usable again and /stop with nothing running says so.
	h.say("/stop")
	got = h.waitSent(3)
	if got[2] != "Nothing is running." {
		t.Fatalf("sent = %q", got)
	}
	h.say("after")
	if got = h.waitSent(4); got[3] != "echo: after" {
		t.Fatalf("sent = %q", got)
	}
}

func TestNewStartsNewSession(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("a")
	h.waitSent(1)
	first := h.store.Session("fake:c1")
	h.say("b")
	h.waitSent(2)
	if n := len(h.calls("new:")); n != 1 || h.store.Session("fake:c1") != first {
		t.Fatalf("second message made a new session: %q", h.calls("new:"))
	}
	h.say("/new")
	h.waitSent(3)
	h.say("c")
	h.waitSent(4)
	second := h.store.Session("fake:c1")
	if n := len(h.calls("new:")); n != 2 || second == first || second == "" {
		t.Fatalf("after /new: new calls %q, sessions %q -> %q", h.calls("new:"), first, second)
	}
}

func TestPermissionOutcomes(t *testing.T) {
	t.Parallel()
	for _, tc := range []struct {
		name    string
		reply   string // chat command; "" waits for the timeout
		timeout time.Duration
		want    string // outcome logged by the agent
	}{
		{"allow", "/allow", 0, "allow_once"},
		{"deny", "/deny", 0, "reject_once"},
		{"timeout", "", 200 * time.Millisecond, "reject_once"},
		{"stop", "/stop", 0, "cancelled"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			h := newHarness(t, setup{mod: func(o *Options) {
				if tc.timeout != 0 {
					o.PermissionTimeout = tc.timeout
				}
			}})
			h.say("perm:rm -rf build")
			got := h.waitSent(1)
			if !strings.Contains(got[0], "rm -rf build") || !strings.Contains(got[0], "Reply /allow or /deny") {
				t.Fatalf("permission message = %q", got[0])
			}
			if tc.reply != "" {
				h.say(tc.reply)
			}
			h.waitCall("perm:")
			if got := h.calls("perm:"); len(got) != 1 || got[0] != "perm:"+tc.want {
				t.Fatalf("agent got %q, want perm:%s", got, tc.want)
			}
			h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "approval card resolved")
			h.plat.mu.Lock()
			status := h.plat.statuses[0]
			h.plat.mu.Unlock()
			wantStatus := map[string]string{"allow": "Approved.", "deny": "Denied.", "timeout": "Denied / expired.", "stop": "Cancelled."}[tc.name]
			if status != wantStatus {
				t.Fatalf("status = %q, want %q", status, wantStatus)
			}
			// The chat is told the outcome.
			n := 3
			if tc.name == "stop" {
				n = 2 // the permission message and "Stopped."
			}
			h.waitFor(func() bool { return len(h.plat.texts()) >= n }, "outcome message and reply")
			texts := h.plat.texts()
			joined := strings.Join(texts, "|")
			switch tc.name {
			case "allow":
				if !strings.Contains(joined, "Allowed.") || !strings.Contains(joined, "outcome=allow_once") {
					t.Fatalf("sent = %q", texts)
				}
			case "deny":
				if !strings.Contains(joined, "Denied.") || !strings.Contains(joined, "outcome=reject_once") {
					t.Fatalf("sent = %q", texts)
				}
			case "timeout":
				if !strings.Contains(joined, "timed out") {
					t.Fatalf("sent = %q", texts)
				}
			case "stop":
				if !strings.Contains(joined, "Stopped.") {
					t.Fatalf("sent = %q", texts)
				}
			}
		})
	}
}

func TestApprovalDialogueRepliesAndQueuesMessages(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_DIALOGUE=1"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.say("Why do you need this?")
	h.say("After approval please run ls")
	h.waitFor(func() bool { return len(h.calls("approval:")) == 2 }, "approval dialogue messages")
	h.waitFor(func() bool { return len(h.plat.texts()) >= 3 }, "approval dialogue replies")
	if got := h.calls("start:"); len(got) != 1 || got[0] != "start:perm:ls" {
		t.Fatalf("queued message ran before approval: %q", got)
	}
	if got := h.plat.texts(); !slices.Contains(got, "The agent needs approval to run this tool call.") || !slices.Contains(got, "I will handle that after approval.") {
		t.Fatalf("dialogue replies = %q", got)
	}
	h.say("/deny")
	h.waitFor(func() bool { return len(h.calls("start:")) == 2 }, "queued message after permission resolves")
	if got := h.calls("start:"); !strings.Contains(got[1], "After approval please run ls") {
		t.Fatalf("queued prompt = %q", got[1])
	}
}

func TestApprovalMessageUnsupportedKeepsLegacyQueueBehavior(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{}) // the fake agent does not advertise the extension
	h.say("perm:ls")
	h.waitSent(1)
	h.say("run ls after approval")
	h.waitFor(func() bool { return len(h.plat.texts()) >= 2 }, "legacy queue notice")
	if got := h.plat.texts(); !strings.Contains(strings.Join(got, "|"), "queues messages while waiting") {
		t.Fatalf("sent = %q", got)
	}
	if got := h.calls("start:"); len(got) != 1 {
		t.Fatalf("message ran before approval: %q", got)
	}
	h.say("/deny")
	h.waitFor(func() bool { return len(h.calls("start:")) == 2 }, "queued message after denial")
	if got := h.calls("start:"); !strings.Contains(got[1], "run ls after approval") {
		t.Fatalf("queued prompt = %q", got[1])
	}
}

func TestPermissionIgnoresNonAdmittedSender(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{senders: []string{"u1", "u2"}})
	h.say("perm:ls")
	h.waitSent(1)
	h.plat.deliver(h.msg("c1", "intruder", "/allow"))
	h.plat.deliver(h.msg("c2", "u1", "/allow")) // other chat, not in the chat list
	settle()
	if c := h.calls("perm:"); len(c) != 0 {
		t.Fatalf("a non-admitted /allow answered the request: %q", c)
	}
	h.plat.deliver(h.msg("c1", "u2", "/deny")) // another admitted sender may answer
	h.waitCall("perm:")
	if c := h.calls("perm:"); len(c) != 1 || c[0] != "perm:reject_once" {
		t.Fatalf("calls = %q", c)
	}
}

func TestAllowWithNothingPending(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("/allow")
	h.say("/deny")
	got := h.waitSent(2)
	if got[0] != "No pending request." || got[1] != "No pending request." {
		t.Fatalf("sent = %q", got)
	}
	if len(fake.Calls(h.dir)) != 0 {
		t.Fatalf("commands reached the agent: %q", fake.Calls(h.dir))
	}
}

func TestLoadReplayIsNotSentToChat(t *testing.T) {
	t.Parallel()
	store := newStore(t)
	dir := t.TempDir()
	h1 := newHarness(t, setup{dir: dir, store: store})
	h1.say("one")
	h1.waitSent(1)
	sid := store.Session("fake:c1")
	h1.shutdown()

	h2 := newHarness(t, setup{dir: dir, store: store, env: []string{"FAKE_REPLAY=5000"}})
	h2.say("two")
	h2.waitSent(1)
	settle()
	if got := h2.plat.texts(); !slices.Equal(got, []string{"echo: two"}) {
		t.Fatalf("sent = %q, want only the new prompt's text", got)
	}
	if c := h2.calls("load:"); len(c) != 1 || c[0] != "load:"+sid {
		t.Fatalf("load calls = %q", c)
	}
}

func TestAgentExitMidPromptThenRestart(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("a")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	h.say("exit")
	got := h.waitSent(2)
	settle()
	if got = h.plat.texts(); len(got) != 2 {
		t.Fatalf("sent = %q, want exactly one error message", got)
	}
	if !strings.Contains(got[1], "exit status 3") || !strings.Contains(got[1], "fatal: boom") {
		t.Fatalf("error message = %q", got[1])
	}
	h.say("b")
	got = h.waitSent(3)
	if got[2] != "echo: b" {
		t.Fatalf("sent = %q", got)
	}
	if n := len(h.calls("init:")); n != 2 {
		t.Fatalf("agent started %d times, want 2", n)
	}
	if c := h.calls("load:"); len(c) != 1 || c[0] != "load:"+sid {
		t.Fatalf("load calls = %q, want load:%s", c, sid)
	}
	if h.store.Session("fake:c1") != sid {
		t.Fatal("session id changed")
	}
}

func TestAdmissionFailsClosed(t *testing.T) {
	t.Parallel()
	for _, tc := range []struct {
		name            string
		chats, senders  []string
		chat, sender    string
		wantAgentPrompt bool
	}{
		{"wrong chat", []string{"c1"}, []string{"u1"}, "other", "u1", false},
		{"wrong sender", []string{"c1"}, []string{"u1"}, "c1", "other", false},
		{"empty chats", []string{}, []string{"u1"}, "c1", "u1", false},
		{"empty senders", []string{"c1"}, []string{}, "c1", "u1", false},
		{"both match", []string{"c1"}, []string{"u1"}, "c1", "u1", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			h := newHarness(t, setup{chats: tc.chats, senders: tc.senders})
			h.plat.deliver(h.msg(tc.chat, tc.sender, "hello"))
			h.plat.deliver(h.msg(tc.chat, tc.sender, "/new"))
			h.plat.deliver(h.msg(tc.chat, tc.sender, "/allow"))
			if tc.wantAgentPrompt {
				h.waitCall("start:hello")
				return
			}
			settle()
			if got := h.plat.texts(); len(got) != 0 {
				t.Fatalf("rejected message got a reply: %q", got)
			}
			if c := fake.Calls(h.dir); len(c) != 0 {
				t.Fatalf("rejected message reached the agent: %q", c)
			}
		})
	}
}

func TestStaleMessageIsNotRun(t *testing.T) {
	t.Parallel()
	now := time.Date(2026, 10, 2, 12, 0, 0, 0, time.UTC)
	h := newHarness(t, setup{mod: func(o *Options) { o.Now = func() time.Time { return now } }})
	old := h.msg("c1", "u1", "hello")
	old.Time = now.Add(-31 * time.Minute)
	h.plat.deliver(old)
	got := h.waitSent(1)
	if !strings.Contains(got[0], "11:29:00") || !strings.Contains(got[0], "2026-10-02") {
		t.Fatalf("notice = %q, want the message time", got[0])
	}
	fresh := h.msg("c1", "u1", "fresh")
	fresh.Time = now.Add(-29 * time.Minute)
	h.plat.deliver(fresh)
	if got = h.waitSent(2); got[1] != "echo: fresh" {
		t.Fatalf("sent = %q", got)
	}
	if p := h.calls("start:"); len(p) != 1 || p[0] != "start:fresh" {
		t.Fatalf("agent prompts = %q", p)
	}
}

func TestLoadFailureStartsNewSession(t *testing.T) {
	t.Parallel()
	store := newStore(t)
	stale := strings.Repeat("ab", 16)
	if err := store.SetSession("fake:c1", stale); err != nil {
		t.Fatal(err)
	}
	h := newHarness(t, setup{store: store})
	h.say("hello")
	got := h.waitSent(2)
	if !strings.Contains(got[0], "could not be loaded") || got[1] != "echo: hello" {
		t.Fatalf("sent = %q", got)
	}
	if now := store.Session("fake:c1"); now == stale || now == "" {
		t.Fatalf("stored session = %q", now)
	}
	if len(h.calls("load:")) != 1 || len(h.calls("new:")) != 1 {
		t.Fatalf("calls = %q", fake.Calls(h.dir))
	}
}

func TestAttachmentIsIgnoredWithNotice(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	m := h.msg("c1", "u1", "")
	m.Attachment = true
	h.plat.deliver(m)
	got := h.waitSent(1)
	if !strings.Contains(got[0], "Attachments are not supported") {
		t.Fatalf("sent = %q", got)
	}
	settle()
	if len(fake.Calls(h.dir)) != 0 {
		t.Fatalf("attachment without text reached the agent: %q", fake.Calls(h.dir))
	}
	m = h.msg("c1", "u1", "see this")
	m.Attachment = true
	h.plat.deliver(m)
	got = h.waitSent(3)
	if !slices.Contains(got, "echo: see this") || !slices.ContainsFunc(got[1:], func(s string) bool { return strings.Contains(s, "Attachments") }) {
		t.Fatalf("sent = %q", got)
	}
}

func TestConfigProfilesIsHandledByConnectorNotAgent(t *testing.T) {
	client := managementServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/config/profiles" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"profiles":[{"name":"work","default":true,"provider":"openai-compatible","model":"small"}]}`))
	})
	h := newHarness(t, setup{manage: client})
	h.say("/config profiles")
	got := h.waitSent(1)
	if got[0] != "work: openai-compatible / small (default)" {
		t.Fatalf("reply = %q", got)
	}
	settle()
	if calls := fake.Calls(h.dir); len(calls) != 0 {
		t.Fatalf("management command reached agent: %q", calls)
	}
}

func TestTypingWhilePromptRuns(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{mod: func(o *Options) { o.TypingInterval = 30 * time.Millisecond }})
	h.say("sleep")
	h.waitSent(1)
	h.plat.mu.Lock()
	n := h.plat.typing
	h.plat.mu.Unlock()
	if n < 3 {
		t.Fatalf("typing sent %d times during a 300 ms prompt at a 30 ms interval", n)
	}
}

func TestShutdownCancelsRunningPrompt(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	if err := h.shutdown(); err != nil {
		t.Fatal(err)
	}
	// The SDK also sends session/cancel when the prompt's context ends.
	if c := h.calls("cancel:"); len(c) == 0 {
		t.Fatalf("no session/cancel at shutdown")
	}
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("shutdown sent to the chat: %q", got)
	}
}

func TestApprovalButtonsBoundToRequest(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("perm:first")
	h.waitSent(1)
	first := h.plat.approvalID()
	click := func(chat, sender, id, text string) {
		m := h.msg(chat, sender, text)
		m.ApprovalID = id
		h.plat.deliver(m)
	}
	click("c1", "intruder", first, "/allow")
	click("c2", "u1", first, "/allow")
	click("c1", "u1", "wrong", "/allow")
	settle()
	if len(h.calls("perm:")) != 0 {
		t.Fatal("invalid callback answered request")
	}
	click("c1", "u1", first, "/allow")
	h.waitCall("perm:")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "card completion")
	h.say("perm:second")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.approvals) == 2 }, "second card")
	second := h.plat.approvalID()
	if first == second {
		t.Fatal("request ID reused")
	}
	click("c1", "u1", first, "/allow")
	settle()
	if len(h.calls("perm:")) != 1 {
		t.Fatal("old card answered new request")
	}
	click("c1", "u1", second, "/deny")
	h.waitFor(func() bool { return len(h.calls("perm:")) == 2 }, "deny")
	if got := h.calls("perm:"); got[0] != "perm:allow_once" || got[1] != "perm:reject_once" {
		t.Fatal(got)
	}
	click("c1", "u1", second, "/allow")
	settle()
	if len(h.calls("perm:")) != 2 {
		t.Fatal("duplicate callback reached agent")
	}
}

func TestApprovalCardFailureFallsBackToCommands(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.plat.mu.Lock()
	h.plat.cardErr = fmt.Errorf("card unavailable")
	h.plat.mu.Unlock()
	h.say("perm:ls")
	got := h.waitSent(1)
	if !strings.Contains(got[0], "Reply /allow or /deny") {
		t.Fatal(got)
	}
	h.say("/allow")
	h.waitCall("perm:")
	if h.calls("perm:")[0] != "perm:allow_once" {
		t.Fatal(h.calls("perm:"))
	}
}

func TestPersistentReadPermissionUsesExistingCards(t *testing.T) {
	t.Parallel()
	for _, tc := range []struct{ name, reply, want string }{
		{"allow", "/allow", "read_grant"},
		{"deny", "/deny", "reject_once"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			h := newHarness(t, setup{})
			h.say("permread:Permanently allow reading /fixture; saved to read_paths")
			got := h.waitSent(1)
			if !strings.Contains(got[0], "Permanently allow reading /fixture") {
				t.Fatalf("card = %q", got[0])
			}
			id := h.plat.approvalID()
			msg := h.msg("c1", "u1", tc.reply)
			msg.ApprovalID = id
			h.plat.deliver(msg)
			h.waitCall("perm:")
			if got := h.calls("perm:"); len(got) != 1 || got[0] != "perm:"+tc.want {
				t.Fatalf("agent outcomes = %q", got)
			}
			h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "read card resolved")
		})
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
	for _, text := range []string{"/compact", " /compact keep the API names", "/compact\nkeep names", "/context"} {
		if got := chatPrompt(Message{Platform: "telegram", Text: text}); got != text {
			t.Errorf("chatPrompt(%q) = %q, want the text unchanged", text, got)
		}
	}
	for _, text := range []string{"/contextual question", "/stop", "please /compact"} {
		if got := chatPrompt(Message{Platform: "telegram", Text: text}); got == text {
			t.Errorf("chatPrompt(%q) has no channel context", text)
		}
	}
}

func TestMenuCommandsAreHandled(t *testing.T) {
	t.Parallel()
	name := regexp.MustCompile(`^[a-z0-9_]{1,32}$`)
	for _, c := range Commands {
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
