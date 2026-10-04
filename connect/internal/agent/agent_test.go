package agent

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/connect/internal/agent/fake"
	"github.com/coder/acp-go-sdk"
)

func TestMain(m *testing.M) {
	if fake.Main() {
		return
	}
	os.Exit(m.Run())
}

type recorder struct {
	mu      sync.Mutex
	updates []string // "<session>:<kind>"
}

func (r *recorder) Update(sid string, u acp.SessionUpdate) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.updates = append(r.updates, sid)
}

func (r *recorder) count() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return len(r.updates)
}

func (r *recorder) Permission(context.Context, string, acp.RequestPermissionRequest) acp.RequestPermissionResponse {
	return acp.RequestPermissionResponse{Outcome: acp.NewRequestPermissionOutcomeCancelled()}
}

func fakeAgent(t *testing.T, extra ...string) (*Agent, *recorder, string) {
	t.Helper()
	dir := t.TempDir()
	a := New(Options{
		Command: []string{os.Args[0]},
		Dir:     dir,
		Env:     fake.Env(dir, extra...),
		Sleep:   func(context.Context, time.Duration) error { return nil },
	})
	rec := &recorder{}
	a.SetHandler(rec)
	t.Cleanup(func() { a.Close() })
	return a, rec, dir
}

func TestSessionUpdateSuppressesLoadReplayExceptReflectionNotice(t *testing.T) {
	rec := &recorder{}
	p := &process{loading: map[string]bool{"s1": true}}
	c := &client{p: p, handler: rec}
	send := func(update acp.SessionUpdate) {
		if err := c.SessionUpdate(context.Background(), acp.SessionNotification{
			SessionId: acp.SessionId("s1"),
			Update:    update,
		}); err != nil {
			t.Fatal(err)
		}
	}

	send(acp.UpdateAgentMessageText("history"))
	if rec.count() != 0 {
		t.Fatalf("ordinary load replay was delivered")
	}

	for _, noticeID := range []float64{0, 1.5} {
		invalid := acp.UpdateAgentMessageText("invalid notice")
		invalid.AgentMessageChunk.Meta = map[string]any{"otto": map[string]any{
			"reflectionNotice": true,
			"noticeId":         noticeID,
		}}
		send(invalid)
		if rec.count() != 0 {
			t.Fatalf("invalid reflection notice %v was delivered", noticeID)
		}
	}

	notice := acp.UpdateAgentMessageText("new notice")
	notice.AgentMessageChunk.Meta = map[string]any{"otto": map[string]any{
		"reflectionNotice": true,
		"noticeId":         float64(1),
	}}
	send(notice)
	if rec.count() != 1 {
		t.Fatalf("valid reflection notice count = %d, want 1", rec.count())
	}
}

func TestRestartDelay(t *testing.T) {
	var d time.Duration
	var got []time.Duration
	for i := 0; i < 9; i++ {
		d = restartDelay(d, time.Second)
		got = append(got, d)
	}
	want := []time.Duration{1, 2, 4, 8, 16, 32, 60, 60, 60}
	for i := range want {
		if got[i] != want[i]*time.Second {
			t.Fatalf("delays = %v, want %v seconds", got, want)
		}
	}
	if d := restartDelay(60*time.Second, 60*time.Second); d != 0 {
		t.Fatalf("delay after a 60 s run = %v, want 0", d)
	}
	if d := restartDelay(60*time.Second, 59*time.Second); d != 60*time.Second {
		t.Fatalf("delay after a 59 s run = %v, want 60s", d)
	}
}

// A command that exits at once doubles the delay before each restart, and
// the ExitError carries the status and stderr.
func TestRestartBackoffAndExitError(t *testing.T) {
	var slept []time.Duration
	now := time.Unix(1000, 0)
	a := New(Options{
		Command: []string{"sh", "-c", "echo line1 >&2; echo line2 >&2; exit 7"},
		Dir:     t.TempDir(),
		Now:     func() time.Time { return now }, // frozen: every run lasts 0 s
		Sleep:   func(_ context.Context, d time.Duration) error { slept = append(slept, d); return nil },
	})
	defer a.Close()
	for i := 0; i < 4; i++ {
		_, err := a.New(context.Background())
		var ee *ExitError
		if !errors.As(err, &ee) {
			t.Fatalf("call %d: err = %v, want *ExitError", i, err)
		}
		if ee.Status != "exit status 7" || strings.Join(ee.Stderr, "|") != "line1|line2" {
			t.Fatalf("call %d: ExitError = %+v", i, ee)
		}
	}
	want := []time.Duration{time.Second, 2 * time.Second, 4 * time.Second}
	if len(slept) != len(want) {
		t.Fatalf("sleeps = %v, want %v", slept, want)
	}
	for i := range want {
		if slept[i] != want[i] {
			t.Fatalf("sleeps = %v, want %v", slept, want)
		}
	}
}

func TestStderrTailKeepsLast20Lines(t *testing.T) {
	tl := &tail{level: -4}
	for i := 1; i <= 25; i++ {
		tl.Write([]byte("l" + string(rune('a'+i%26)) + "\n"))
	}
	tl.Write([]byte("partial"))
	got := tl.lines()
	if len(got) != 20 || got[19] != "partial" || got[0] != "l"+string(rune('a'+7%26)) {
		t.Fatalf("lines = %q", got)
	}
}

func TestLoadDropsReplayedUpdates(t *testing.T) {
	a, rec, _ := fakeAgent(t, "FAKE_REPLAY=5000")
	ctx := context.Background()
	id, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	// A second agent object on the same dir cannot share the process, so
	// restart by killing: close stdin of the first process only.
	_ = a.cur.stdin.Close()
	<-a.cur.exited
	if err := a.Load(ctx, id); err != nil {
		t.Fatal(err)
	}
	if n := rec.count(); n != 0 {
		t.Fatalf("%d replayed updates reached the handler", n)
	}
	if _, err := a.Prompt(ctx, id, "chunks"); err != nil {
		t.Fatal(err)
	}
	if n := rec.count(); n != 5 {
		t.Fatalf("updates after prompt = %d, want 5", n)
	}
}

func TestConcurrentLoadAfterReconnectReplaysOneSessionOnce(t *testing.T) {
	a, rec, dir := fakeAgent(t, "FAKE_REPLAY=5000")
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	id, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	a.Kill()
	results := make(chan error, 32)
	var callers sync.WaitGroup
	for range 32 {
		callers.Add(1)
		go func() { defer callers.Done(); results <- a.Load(ctx, id) }()
	}
	callers.Wait()
	close(results)
	for err := range results {
		if err != nil {
			t.Fatal(err)
		}
	}
	loads := 0
	for _, call := range fake.Calls(dir) {
		if call == "load:"+id {
			loads++
		}
	}
	if loads != 1 {
		t.Fatalf("session replayed %d times", loads)
	}
	if rec.count() != 0 {
		t.Fatal("replayed updates reached the chat handler")
	}
}

func TestUnifiedApprovalCapabilityRequiresTheSupportedVersion(t *testing.T) {
	for _, tc := range []struct {
		meta      string
		supported bool
	}{
		{`{"otto":{"approvals":{"version":1}}}`, true},
		{`{"otto":{"approvals":{"version":2}}}`, false},
		{`{"otto":{"approvals":{"version":"1"}}}`, false},
		{`{"otto":{"approvals":{}}}`, false},
		{`{}`, false},
	} {
		var meta map[string]any
		if err := json.Unmarshal([]byte(tc.meta), &meta); err != nil {
			t.Fatal(err)
		}
		if advertisesSharedApprovals(meta) != tc.supported {
			t.Fatalf("incorrect capability negotiation: %s", tc.meta)
		}
	}
}

func TestPromptAfterRestartNeedsLoad(t *testing.T) {
	a, _, _ := fakeAgent(t)
	ctx := context.Background()
	id, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	_ = a.cur.stdin.Close()
	<-a.cur.exited
	if _, err := a.Prompt(ctx, id, "x"); !errors.Is(err, ErrNotOpen) {
		t.Fatalf("Prompt on a dead process: %v, want ErrNotOpen", err)
	}
	if err := a.Load(ctx, id); err != nil {
		t.Fatal(err)
	}
	if _, err := a.Prompt(ctx, id, "x"); err != nil {
		t.Fatal(err)
	}
}

func TestApprovalMessageRequiresCapabilityAndReturnsNullOutsideApproval(t *testing.T) {
	t.Run("unsupported", func(t *testing.T) {
		a, _, _ := fakeAgent(t)
		id, err := a.New(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if _, err := a.ApprovalMessage(context.Background(), id, "hello"); !errors.Is(err, ErrApprovalUnsupported) {
			t.Fatalf("ApprovalMessage error = %v, want ErrApprovalUnsupported", err)
		}
	})
	t.Run("idle", func(t *testing.T) {
		a, _, _ := fakeAgent(t, "FAKE_APPROVAL_DIALOGUE=1")
		id, err := a.New(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		reply, err := a.ApprovalMessage(context.Background(), id, "hello")
		if err != nil || reply != nil {
			t.Fatalf("ApprovalMessage = (%+v, %v), want (nil, nil)", reply, err)
		}
	})
}

func TestTaskControlRequiresCapabilityAndTargetsOnlyActiveTurns(t *testing.T) {
	t.Run("unsupported", func(t *testing.T) {
		a, _, _ := fakeAgent(t, "FAKE_NO_TASK_CONTROL=1")
		id, err := a.New(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if _, err := a.ControlMessageWithChannel(context.Background(), id, "guide", "telegram"); !errors.Is(err, ErrTaskControlUnsupported) {
			t.Fatalf("ControlMessageWithChannel error = %v, want unsupported", err)
		}
		if err := a.Steer(context.Background(), id, "guide"); !errors.Is(err, ErrTaskControlUnsupported) {
			t.Fatalf("Steer error = %v, want unsupported", err)
		}
	})

	t.Run("advertised capability with old server methods", func(t *testing.T) {
		a, _, dir := fakeAgent(t, "FAKE_NO_TASK_CONTROL_METHODS=1")
		id, err := a.New(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if _, err := a.ControlMessageWithChannel(context.Background(), id, "guide", "telegram"); !errors.Is(err, ErrTaskControlUnsupported) {
			t.Fatalf("method-not-found control error = %v, want unsupported", err)
		}
		if err := a.Steer(context.Background(), id, "guide"); !errors.Is(err, ErrTaskControlUnsupported) {
			t.Fatalf("method-not-found steer error = %v, want unsupported", err)
		}
		for _, call := range fake.Calls(dir) {
			if strings.HasPrefix(call, "start:") {
				t.Fatalf("unsupported extension was retried as an ordinary prompt: %q", call)
			}
		}
	})

	t.Run("active and idle", func(t *testing.T) {
		a, _, dir := fakeAgent(t)
		ctx := context.Background()
		id, err := a.New(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if reply, err := a.ControlMessageWithChannel(ctx, id, "guide", "telegram"); err != nil || reply != nil {
			t.Fatalf("idle control = (%+v, %v), want (nil, nil)", reply, err)
		}
		if err := a.Steer(ctx, id, "guide"); err == nil {
			t.Fatal("idle Steer succeeded")
		}

		promptDone := make(chan error, 1)
		go func() {
			_, err := a.Prompt(ctx, id, "block")
			promptDone <- err
		}()
		waitForCall(t, dir, "start:block")

		reply, err := a.ControlMessageWithChannel(ctx, id, "guide", "telegram")
		if err != nil || reply == nil || reply.Queued || reply.Text != "Guidance accepted." {
			t.Fatalf("ControlMessageWithChannel = (%+v, %v)", reply, err)
		}
		queued, err := a.ControlMessageWithChannel(ctx, id, "follow-up: next", "feishu")
		if err != nil || queued == nil || !queued.Queued {
			t.Fatalf("queued control = (%+v, %v)", queued, err)
		}
		if err := a.Steer(ctx, id, "focus on the failing test"); err != nil {
			t.Fatal(err)
		}
		calls := fake.Calls(dir)
		for _, want := range []string{
			"task_control:guide",
			"task_control_channel:telegram",
			"task_control:follow-up: next",
			"task_control_channel:feishu",
			"steer:focus on the failing test",
		} {
			if !slices.Contains(calls, want) {
				t.Errorf("fake call %q missing from %q", want, calls)
			}
		}
		if err := a.Cancel(ctx, id); err != nil {
			t.Fatal(err)
		}
		if err := <-promptDone; err != nil {
			t.Fatal(err)
		}
	})
}

func waitForCall(t *testing.T, dir, want string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if slices.Contains(fake.Calls(dir), want) {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatalf("timed out waiting for %q in %q", want, fake.Calls(dir))
}

func TestLoadUnknownSessionIsNotExitError(t *testing.T) {
	a, _, _ := fakeAgent(t)
	err := a.Load(context.Background(), strings.Repeat("a", 32))
	var ee *ExitError
	if err == nil || errors.As(err, &ee) {
		t.Fatalf("err = %v, want a plain agent error", err)
	}
}

func TestExitDuringPromptReturnsExitError(t *testing.T) {
	a, _, _ := fakeAgent(t)
	ctx := context.Background()
	id, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	_, err = a.Prompt(ctx, id, "exit")
	var ee *ExitError
	if !errors.As(err, &ee) || ee.Status != "exit status 3" || len(ee.Stderr) == 0 || ee.Stderr[len(ee.Stderr)-1] != "fatal: boom" {
		t.Fatalf("err = %#v", err)
	}
}

func TestCloseKillsProcessThatIgnoresStdinEOF(t *testing.T) {
	a, _, _ := fakeAgent(t, "FAKE_NO_EXIT=1")
	a.opts.CloseWait = 200 * time.Millisecond
	if _, err := a.New(context.Background()); err != nil {
		t.Fatal(err)
	}
	p := a.cur
	if err := a.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-p.exited:
	default:
		t.Fatal("process still running after Close")
	}
	if p.waitErr == nil || !strings.Contains(p.waitErr.Error(), "killed") {
		t.Fatalf("waitErr = %v, want killed", p.waitErr)
	}
	if _, err := a.New(context.Background()); !errors.Is(err, ErrClosed) {
		t.Fatalf("after Close: %v", err)
	}
}

// A process that answers initialize, then closes its stdin and exits 300 ms
// later: the next call's failed write is reported as the exit, not as the
// SDK's internal error.
func TestWriteAfterStdinClosedIsExitError(t *testing.T) {
	init := `{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}`
	a := New(Options{
		Command: []string{"sh", "-c", "read line; exec 0<&-; echo '" + init + "'; sleep 0.3; echo gone >&2; exit 5"},
		Dir:     t.TempDir(),
	})
	defer a.Close()
	_, err := a.New(context.Background())
	var ee *ExitError
	if !errors.As(err, &ee) || ee.Status != "exit status 5" || strings.Join(ee.Stderr, "|") != "gone" {
		t.Fatalf("err = %v, want *ExitError with exit status 5 and stderr gone", err)
	}
}

func wakeChunk(meta map[string]any, content acp.ContentBlock) acp.SessionUpdate {
	return acp.SessionUpdate{AgentMessageChunk: &acp.SessionUpdateAgentMessageChunk{Meta: meta, Content: content}}
}

func TestWakeReplyShape(t *testing.T) {
	otto := func(m map[string]any) map[string]any { return map[string]any{"otto": m} }
	valid := otto(map[string]any{"wakeReply": true, "replyId": float64(1)})
	if text, ok := WakeReply(wakeChunk(valid, acp.TextBlock("**hi**"))); !ok || text != "**hi**" {
		t.Fatalf("valid wake reply = %q, %v", text, ok)
	}
	for name, u := range map[string]acp.SessionUpdate{
		"missing flag":    wakeChunk(otto(map[string]any{"replyId": float64(1)}), acp.TextBlock("x")),
		"false flag":      wakeChunk(otto(map[string]any{"wakeReply": false, "replyId": float64(1)}), acp.TextBlock("x")),
		"missing id":      wakeChunk(otto(map[string]any{"wakeReply": true}), acp.TextBlock("x")),
		"string id":       wakeChunk(otto(map[string]any{"wakeReply": true, "replyId": "1"}), acp.TextBlock("x")),
		"zero id":         wakeChunk(otto(map[string]any{"wakeReply": true, "replyId": float64(0)}), acp.TextBlock("x")),
		"fractional id":   wakeChunk(otto(map[string]any{"wakeReply": true, "replyId": 1.5}), acp.TextBlock("x")),
		"non-text":        wakeChunk(valid, acp.ImageBlock("", "image/png")),
		"reflection only": wakeChunk(otto(map[string]any{"reflectionNotice": true, "noticeId": float64(1)}), acp.TextBlock("x")),
		"no meta":         acp.UpdateAgentMessageText("x"),
	} {
		if _, ok := WakeReply(u); ok {
			t.Errorf("%s was accepted", name)
		}
	}
}

func TestSessionUpdateDeliversWakeReplyDuringLoad(t *testing.T) {
	rec := &recorder{}
	c := &client{p: &process{loading: map[string]bool{"s1": true}}, handler: rec}
	u := wakeChunk(map[string]any{"otto": map[string]any{"wakeReply": true, "replyId": float64(1)}}, acp.TextBlock("timer"))
	if err := c.SessionUpdate(context.Background(), acp.SessionNotification{SessionId: "s1", Update: u}); err != nil {
		t.Fatal(err)
	}
	u.AgentMessageChunk.Meta = map[string]any{"otto": map[string]any{"wakeReply": true, "replyId": float64(0)}}
	_ = c.SessionUpdate(context.Background(), acp.SessionNotification{SessionId: "s1", Update: u})
	if rec.count() != 1 {
		t.Fatalf("delivered %d updates, want only the valid wake reply", rec.count())
	}
}
