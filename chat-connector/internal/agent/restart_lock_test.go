package agent

import (
	"context"
	"io"
	"testing"
	"time"

	"github.com/coder/acp-go-sdk"
)

// A closed ACP connection with a running child counts as done, so the next
// call starts a new process.
func TestClosedConnectionIsDoneWhileChildRuns(t *testing.T) {
	agentOutR, agentOutW := io.Pipe()
	p := &process{exited: make(chan struct{})}
	p.conn = acp.NewClientSideConnection(&client{p: p}, io.Discard, agentOutR)
	if p.isDone() {
		t.Fatal("isDone = true before the connection closed")
	}
	agentOutW.Close()
	select {
	case <-p.conn.Done():
	case <-time.After(5 * time.Second):
		t.Fatal("connection did not close")
	}
	if !p.isDone() {
		t.Fatal("isDone = false after the connection closed with the child still running")
	}
}

// A child that closes stdout but keeps running is stopped, so the call fails
// with an ExitError well before exitWait.
func TestClosedStdoutStopsChild(t *testing.T) {
	a := New(Options{
		Command: []string{"sh", "-c", "exec 1>&-; exec sleep 30"},
		Dir:     t.TempDir(),
		Sleep:   func(context.Context, time.Duration) error { return nil },
	})
	defer a.Close()
	start := time.Now()
	_, err := a.New(context.Background())
	if err == nil {
		t.Fatal("New succeeded")
	}
	if took := time.Since(start); took > exitWait/2 {
		t.Fatalf("New took %v, want the child stopped on connection close", took)
	}
}

// While a restart delay is pending, a.mu is free for other callers.
func TestRestartDelayDoesNotHoldAgentLock(t *testing.T) {
	sleeping := make(chan struct{})
	release := make(chan struct{})
	a := New(Options{
		Command: []string{"sh", "-c", "exit 1"},
		Dir:     t.TempDir(),
		Now:     func() time.Time { return time.Unix(1000, 0) },
		Sleep: func(ctx context.Context, _ time.Duration) error {
			close(sleeping)
			select {
			case <-release:
			case <-ctx.Done():
			}
			return nil
		},
	})
	defer a.Close()
	if _, err := a.New(context.Background()); err == nil {
		t.Fatal("first New succeeded")
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	second := make(chan error, 1)
	go func() { _, err := a.New(ctx); second <- err }()
	select {
	case <-sleeping:
	case <-time.After(5 * time.Second):
		t.Fatal("restart delay did not start")
	}
	liveDone := make(chan struct{})
	go func() { a.live(); close(liveDone) }()
	select {
	case <-liveDone:
	case <-time.After(time.Second):
		t.Fatal("a.live blocked while the restart delay was pending")
	}
	close(release)
	<-second
}
