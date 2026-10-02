package bridge

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/connect/internal/agent/fake"
	"github.com/baiyuqing/otto/connect/internal/state"
)

// seedSessions writes sessions "<id>\t<updatedAt>\t<title>" into the fake
// agent's directory, oldest first.
func seedSessions(t *testing.T, dir string, lines ...string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, "sessions"), []byte(strings.Join(lines, "\n")+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
}

func sid(prefix string) string { return prefix + strings.Repeat("0", 32-len(prefix)) }

func seedLine(id, title string) string { return id + "\t2026-10-02T10:30:00Z\t" + title }

func TestSessionsListsTenNewestWithMarker(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	var lines []string
	for i := 1; i <= 12; i++ {
		lines = append(lines, seedLine(sid(fmt.Sprintf("%08x", i)), fmt.Sprintf("title %d", i)))
	}
	seedSessions(t, dir, lines...)
	store := newStore(t)
	if err := store.SetSession("fake:c1", sid("0000000b")); err != nil {
		t.Fatal(err)
	}
	h := newHarness(t, setup{dir: dir, store: store})
	h.say("/sessions")
	got := strings.Split(h.waitSent(1)[0], "\n")
	if len(got) != 10 {
		t.Fatalf("listed %d sessions, want 10: %q", len(got), got)
	}
	if want := "  0000000c  2026-10-02 10:30  title 12"; got[0] != want {
		t.Fatalf("first line = %q, want %q", got[0], want)
	}
	if want := "* 0000000b  2026-10-02 10:30  title 11"; got[1] != want {
		t.Fatalf("second line = %q, want %q", got[1], want)
	}
	if strings.Contains(strings.Join(got, "\n"), "*") && strings.Count(strings.Join(got, "\n"), "* ") != 1 {
		t.Fatalf("more than one marked line: %q", got)
	}
}

func TestSessionsWithoutTitleOrTime(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	seedSessions(t, dir, sid("abcdef12"))
	h := newHarness(t, setup{dir: dir})
	h.say("/sessions")
	if got := h.waitSent(1)[0]; got != "  abcdef12  -  (untitled)" {
		t.Fatalf("reply = %q", got)
	}
}

func TestUse(t *testing.T) {
	t.Parallel()
	a, b1, b2 := sid("aaaa1111"), sid("bbbb2222"), sid("bbbb3333")
	for _, tc := range []struct {
		name  string
		arg   string
		want  string // reply
		bound string // stored session afterwards
		loads int
	}{
		{"full id", a, "Using session " + a + ": alpha", a, 1},
		{"unique prefix", "aaaa", "Using session " + a + ": alpha", a, 1},
		{"ambiguous prefix", "bbbb", `"bbbb" matches 2 sessions; use more characters.`, "", 0},
		{"unknown prefix", "cccc", `No listed session starts with "cccc".`, "", 0},
		{"short prefix", "aaa", "A session id prefix needs at least 4 characters.", "", 0},
		{"missing argument", "", "Usage: /use <session id>", "", 0},
		{"unknown full id", sid("dddd4444"), "Error: could not load session " + sid("dddd4444") + ": ", "", 1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			dir := t.TempDir()
			seedSessions(t, dir, seedLine(a, "alpha"), seedLine(b1, "beta"), seedLine(b2, "gamma"))
			h := newHarness(t, setup{dir: dir})
			h.say(strings.TrimSpace("/use " + tc.arg))
			got := h.waitSent(1)[0]
			if !strings.HasPrefix(got, tc.want) {
				t.Fatalf("reply = %q, want prefix %q", got, tc.want)
			}
			if bound := h.store.Session("fake:c1"); bound != tc.bound {
				t.Fatalf("stored session = %q, want %q", bound, tc.bound)
			}
			if n := len(h.calls("load:")); n != tc.loads {
				t.Fatalf("session/load called %d times, want %d: %q", n, tc.loads, fake.Calls(dir))
			}
		})
	}
}

func TestUseThenPromptRunsInBoundSession(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	a := sid("aaaa1111")
	seedSessions(t, dir, seedLine(a, "alpha"))
	h := newHarness(t, setup{dir: dir})
	h.say("/use aaaa1111")
	h.waitSent(1)
	h.say("hello")
	if got := h.waitSent(2); got[1] != "echo: hello" {
		t.Fatalf("sent = %q", got)
	}
	if n := len(h.calls("new:")); n != 0 {
		t.Fatalf("session/new called %d times after /use", n)
	}
}

func TestUseRefusedDuringRunningPrompt(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	a := sid("aaaa1111")
	seedSessions(t, dir, seedLine(a, "alpha"))
	h := newHarness(t, setup{dir: dir})
	h.say("block")
	h.waitCall("start:block")
	h.say("/use aaaa1111")
	got := h.waitSent(1)
	if !strings.Contains(got[0], "running or queued") {
		t.Fatalf("reply = %q", got[0])
	}
	settle()
	if bound := h.store.Session("fake:c1"); bound == a {
		t.Fatal("binding changed during a running prompt")
	}
	if n := len(h.calls("load:")); n != 0 {
		t.Fatalf("session/load called %d times", n)
	}
	h.say("/stop")
	h.waitSent(2)
}

func TestUseBindingSurvivesRestart(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	statePath := filepath.Join(t.TempDir(), "state.json")
	a := sid("aaaa1111")
	seedSessions(t, dir, seedLine(a, "alpha"))

	store1, err := state.Open(statePath)
	if err != nil {
		t.Fatal(err)
	}
	h1 := newHarness(t, setup{dir: dir, store: store1})
	h1.say("/use aaaa")
	h1.waitSent(1)
	h1.shutdown()

	store2, err := state.Open(statePath)
	if err != nil {
		t.Fatal(err)
	}
	h2 := newHarness(t, setup{dir: dir, store: store2})
	h2.say("hello")
	if got := h2.waitSent(1); got[0] != "echo: hello" {
		t.Fatalf("sent = %q", got)
	}
	if got := h2.calls("load:"); !slices.Equal(got, []string{"load:" + a, "load:" + a}) {
		t.Fatalf("load calls = %q, want one before and one after the restart", got)
	}
	if n := len(h2.calls("new:")); n != 0 {
		t.Fatalf("session/new called %d times after the restart", n)
	}
}

func TestCancelledPermissionRequestSendsNotice(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("permcancel:rm -rf build")
	got := h.waitSent(2)
	if !strings.Contains(got[0], "rm -rf build") || got[1] != "permission request answered elsewhere" {
		t.Fatalf("sent = %q", got)
	}
	// The agent's request was cancelled, so the turn ends cancelled.
	if got = h.waitSent(3); got[2] != "Stopped." {
		t.Fatalf("sent = %q", got)
	}
}

func TestNoAnsweredElsewhereNoticeOnStopOrShutdown(t *testing.T) {
	t.Parallel()
	t.Run("stop", func(t *testing.T) {
		t.Parallel()
		h := newHarness(t, setup{})
		h.say("perm:ls")
		h.waitSent(1)
		h.say("/stop")
		h.waitSent(2)
		settle()
		if got := strings.Join(h.plat.texts(), "|"); strings.Contains(got, "elsewhere") {
			t.Fatalf("sent = %q", got)
		}
	})
	t.Run("shutdown", func(t *testing.T) {
		t.Parallel()
		h := newHarness(t, setup{})
		h.say("perm:ls")
		h.waitSent(1)
		h.shutdown()
		if got := strings.Join(h.plat.texts(), "|"); strings.Contains(got, "elsewhere") {
			t.Fatalf("sent = %q", got)
		}
	})
}

func TestTwoChatsOnOneSessionKeepTheirOwnReplies(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	a := sid("aaaa1111")
	seedSessions(t, dir, seedLine(a, "alpha"))
	h := newHarness(t, setup{dir: dir, chats: []string{"c1", "c2"}})
	for _, c := range []string{"c1", "c2"} {
		h.plat.deliver(h.msg(c, "u1", "/use aaaa"))
	}
	h.waitSent(2)
	h.plat.deliver(h.msg("c1", "u1", "sleep"))
	h.waitCall("start:sleep")
	h.plat.deliver(h.msg("c2", "u1", "second"))
	h.waitSent(4)
	byChat := map[string][]string{}
	h.plat.mu.Lock()
	for _, s := range h.plat.sent[2:] {
		byChat[s.chat] = append(byChat[s.chat], s.text)
	}
	h.plat.mu.Unlock()
	if !slices.Equal(byChat["c1"], []string{"echo: sleep"}) || !slices.Equal(byChat["c2"], []string{"echo: second"}) {
		t.Fatalf("replies by chat = %q", byChat)
	}
}
