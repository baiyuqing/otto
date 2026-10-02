package bridge

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const (
	candA = "aaaa1111000000000000000000000000"
	candB = "aaaa2222000000000000000000000000"
	candC = "cccc3333000000000000000000000000"
)

// seedMemory writes pending candidates into the fake agent's directory.
func seedMemory(t *testing.T, dir string, lines ...string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, "memory"), []byte(strings.Join(lines, "\n")+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
}

// withSession returns a harness whose chat already has a session.
func withSession(t *testing.T, s setup) *harness {
	t.Helper()
	s.textOnly = true
	h := newHarness(t, s)
	h.say("hi")
	h.waitSent(1)
	return h
}

func TestMemoryListsPendingCandidates(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tpreference\tprefers tabs", candC+"\tfact\tlives in Berlin")
	h.say("/memory")
	got := h.waitSent(2)[1]
	for _, want := range []string{"2 pending memory candidates", "aaaa1111  create preference/k  prefers tabs  (model)", "cccc3333  create fact/k  lives in Berlin"} {
		if !strings.Contains(got, want) {
			t.Fatalf("list = %q, missing %q", got, want)
		}
	}
	if len(h.calls("review:")) != 0 {
		t.Fatal("listing must not review anything")
	}
}

func TestMemoryAcceptByPrefix(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tpreference\tprefers tabs", candC+"\tfact\tx")
	h.say("/memory accept cccc")
	got := h.waitSent(2)[1]
	if want := "Accepted cccc3333 as record rec-" + candC + " (revision 1)."; got != want {
		t.Fatalf("reply = %q, want %q", got, want)
	}
	if calls := h.calls("review:"); len(calls) != 1 || calls[0] != "review:"+candC+":accept" {
		t.Fatalf("review calls = %q", calls)
	}
	h.say("/memory")
	if got := h.waitSent(3)[2]; strings.Contains(got, "cccc3333") || !strings.Contains(got, "aaaa1111") {
		t.Fatalf("list after accept = %q", got)
	}
}

func TestMemoryReject(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tpreference\tprefers tabs")
	h.say("/memory reject " + candA)
	if got, want := h.waitSent(2)[1], "Rejected aaaa1111."; got != want {
		t.Fatalf("reply = %q, want %q", got, want)
	}
	if calls := h.calls("review:"); len(calls) != 1 || calls[0] != "review:"+candA+":reject" {
		t.Fatalf("review calls = %q", calls)
	}
}

func TestMemoryRejectsAmbiguousUnknownAndShortIDs(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tfact\tone", candB+"\tfact\ttwo")
	for i, id := range []string{"aaaa", "zzzz", "aa"} {
		h.say("/memory accept " + id)
		got := h.waitSent(i + 2)[i+1]
		if !strings.HasPrefix(got, "No single pending candidate starts with") {
			t.Fatalf("/memory accept %s replied %q", id, got)
		}
	}
	if len(h.calls("review:")) != 0 {
		t.Fatal("an unresolved id must not review anything")
	}
}

func TestMemoryUsageAndLookalikeCommands(t *testing.T) {
	t.Parallel()
	h := withSession(t, setup{})
	h.say("/memory approve x")
	if got := h.waitSent(2)[1]; got != memoryUsage {
		t.Fatalf("reply = %q, want usage", got)
	}
	h.say("/memory accept")
	if got := h.waitSent(3)[2]; got != memoryUsage {
		t.Fatalf("reply = %q, want usage", got)
	}
	// Not the /memory command: goes to the agent like any text.
	h.say("/memoryx")
	if got := h.waitSent(4)[3]; got != "echo: /memoryx" {
		t.Fatalf("reply = %q, want the agent's echo", got)
	}
}

func TestMemoryWithoutSession(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{textOnly: true})
	h.say("/memory")
	if got := h.waitSent(1)[0]; !strings.Contains(got, "no session yet") {
		t.Fatalf("reply = %q", got)
	}
}

func TestMemoryUnsupportedAgent(t *testing.T) {
	t.Parallel()
	h := withSession(t, setup{env: []string{"FAKE_NO_MEMORY=1"}})
	h.say("/memory")
	if got, want := h.waitSent(2)[1], "This agent does not support memory review."; got != want {
		t.Fatalf("reply = %q, want %q", got, want)
	}
}

func TestMemoryWorksWhileATurnIsRunning(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tfact\tone")
	h.say("block")
	h.waitCall("start:block")
	h.say("/memory accept aaaa")
	if got := h.waitSent(2)[1]; !strings.HasPrefix(got, "Accepted aaaa1111") {
		t.Fatalf("reply = %q", got)
	}
	h.say("/stop")
	if got := h.waitSent(3)[2]; got != "Stopped." {
		t.Fatalf("reply = %q, want Stopped.", got)
	}
}

func TestMemoryIgnoresUnadmittedSenders(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, candA+"\tfact\tone")
	h.plat.deliver(h.msg("c1", "intruder", "/memory accept "+candA))
	settle()
	if len(h.calls("review:")) != 0 || len(h.plat.texts()) != 1 {
		t.Fatalf("unadmitted sender reached the review: calls=%q sent=%q", h.calls("review:"), h.plat.texts())
	}
}

func TestMemoryHintFollowsATurnThatProposed(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{textOnly: true})
	h.say("remember")
	if got, want := h.waitSent(1)[0], "Queued.\n\n"+memoryHint; got != want {
		t.Fatalf("reply = %q, want %q", got, want)
	}
	h.say("hi")
	if got := h.waitSent(2)[1]; got != "echo: hi" {
		t.Fatalf("a turn without a proposal got %q", got)
	}
}

func TestMemoryPagesAreFollowedToFindACandidateAndListingSaysMoreRemain(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir, env: []string{"FAKE_PAGE=1"}})
	seedMemory(t, dir, candA+"\tfact\tone", candC+"\tfact\ttwo")
	h.say("/memory")
	got := h.waitSent(2)[1]
	if !strings.Contains(got, "aaaa1111") || strings.Contains(got, "cccc3333") || !strings.Contains(got, "More are pending") {
		t.Fatalf("first page = %q", got)
	}
	h.say("/memory accept cccc")
	if got := h.waitSent(3)[2]; !strings.HasPrefix(got, "Accepted cccc3333") {
		t.Fatalf("a candidate on page two was not found: %q", got)
	}
}

func TestMemoryErrorCodesBecomeReadableReplies(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	h := withSession(t, setup{dir: dir})
	seedMemory(t, dir, "dddd1111000000000000000000000000\tfact\tone")
	h.say("/memory accept dddd")
	if got, want := h.waitSent(2)[1], "That candidate was already decided or changed; send /memory to refresh."; got != want {
		t.Fatalf("conflict reply = %q, want %q", got, want)
	}

	off := withSession(t, setup{env: []string{"FAKE_MEMORY_UNAVAILABLE=1"}})
	off.say("/memory")
	if got, want := off.waitSent(2)[1], "Memory is not available in this session."; got != want {
		t.Fatalf("unavailable reply = %q, want %q", got, want)
	}
}
