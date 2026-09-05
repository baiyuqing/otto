package app

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/session"
)

type wakeContractRunner struct {
	tasks       *agent.Tasks
	runs        atomic.Int32
	closes      atomic.Int32
	started     chan struct{}
	startedOnce sync.Once
	release     chan struct{}
	releaseOnce sync.Once
}

func (r *wakeContractRunner) Run(ctx context.Context, text string, _ func(agent.Event)) error {
	if text != "" {
		return errors.New("wake text must be empty")
	}
	r.runs.Add(1)
	r.tasks.Notifications().Drain()
	r.startedOnce.Do(func() { close(r.started) })
	select {
	case <-r.release:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (*wakeContractRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func (r *wakeContractRunner) Tasks() *agent.Tasks { return r.tasks }
func (r *wakeContractRunner) Release()            { r.releaseOnce.Do(func() { close(r.release) }) }
func (r *wakeContractRunner) Close() error {
	r.closes.Add(1)
	r.Release()
	return nil
}

type wakeContractSession struct {
	session.Session
	closes atomic.Int32
}

func (s *wakeContractSession) Close() error {
	s.closes.Add(1)
	return s.Session.Close()
}

func newWakeContractFixture(t *testing.T) (*Controller, *wakeContractRunner, *wakeContractSession) {
	t.Helper()
	runner := &wakeContractRunner{tasks: agent.NewTasks(), started: make(chan struct{}), release: make(chan struct{})}
	current := &wakeContractSession{Session: session.NewMemory(session.Header{
		Version: 1, ID: "wake", Workspace: t.TempDir(), Provider: "test", Model: "test", CreatedAt: time.Now().UTC(),
	})}
	controller, err := New(SessionReplacement{Session: current, Runner: runner})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		runner.Release()
		_ = controller.Close()
	})
	return controller, runner, current
}

func addWakeContractNotification(runner *wakeContractRunner) {
	runner.tasks.Notifications().Push(agent.Notification{TaskID: "t1", Kind: agent.NotificationTaskFinished, Text: "finished"})
}

func waitWakeContract(t *testing.T, event <-chan struct{}) {
	t.Helper()
	select {
	case <-event:
	case <-time.After(time.Second):
		t.Fatal("timed out waiting for wake event")
	}
}

func TestWakePrepareWithoutPendingNotifications(t *testing.T) {
	controller, _, _ := newWakeContractFixture(t)
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake != nil {
		t.Fatalf("PrepareWake() = %#v, %v; want nil, nil", wake, err)
	}
}

func TestWakeConcurrentPrepareClaimsOnlyOne(t *testing.T) {
	controller, runner, _ := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	start := make(chan struct{})
	results := make(chan struct {
		wake *WakeOperation
		err  error
	}, 2)
	var group sync.WaitGroup
	for range 2 {
		group.Add(1)
		go func() {
			defer group.Done()
			<-start
			wake, err := controller.PrepareWake(context.Background())
			results <- struct {
				wake *WakeOperation
				err  error
			}{wake, err}
		}()
	}
	close(start)
	group.Wait()
	close(results)
	claimed := 0
	var activeErr error
	for result := range results {
		if result.wake != nil {
			claimed++
			result.wake.Cancel()
		}
		if result.err != nil {
			activeErr = result.err
		}
	}
	if claimed != 1 || !errors.Is(activeErr, ErrPromptActive) {
		t.Fatalf("claims=%d err=%v, want one claim and ErrPromptActive", claimed, activeErr)
	}
}

func TestWakeRunOnceAndDrainsOnlyWhenRunStarts(t *testing.T) {
	controller, runner, _ := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	if got := runner.tasks.Pending(); got != 1 {
		t.Fatalf("pending before Run = %d, want 1", got)
	}
	done := make(chan error, 1)
	go func() { done <- wake.Run(context.Background(), nil) }()
	waitWakeContract(t, runner.started)
	if got := runner.tasks.Pending(); got != 0 {
		t.Fatalf("pending after Run starts = %d, want 0", got)
	}
	if err := wake.Run(context.Background(), nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("second Run() = %v, want context.Canceled", err)
	}
	runner.Release()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if got := runner.runs.Load(); got != 1 {
		t.Fatalf("runner runs = %d, want 1", got)
	}
}

func TestWakeCancelIsIdempotentAndReleasesClaim(t *testing.T) {
	controller, runner, _ := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	wake.Cancel()
	wake.Cancel()
	next, err := controller.PrepareWake(context.Background())
	if err != nil || next == nil {
		t.Fatalf("PrepareWake after Cancel() = %#v, %v", next, err)
	}
	next.Cancel()
	if got := runner.runs.Load(); got != 0 {
		t.Fatalf("runner runs = %d, want 0", got)
	}
}

func TestWakeCanceledReservationAndRunDoNotInvokeRunner(t *testing.T) {
	controller, runner, _ := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	claimCtx, cancelClaim := context.WithCancel(context.Background())
	wake, err := controller.PrepareWake(claimCtx)
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	cancelClaim()
	if err := wake.Run(context.Background(), nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("canceled reservation Run() = %v, want context.Canceled", err)
	}
	next, err := controller.PrepareWake(context.Background())
	if err != nil || next == nil {
		t.Fatalf("PrepareWake after canceled reservation = %#v, %v", next, err)
	}
	runCtx, cancelRun := context.WithCancel(context.Background())
	cancelRun()
	if err := next.Run(runCtx, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("canceled run Run() = %v, want context.Canceled", err)
	}
	if got := runner.runs.Load(); got != 0 {
		t.Fatalf("runner runs = %d, want 0", got)
	}
}

func TestWakeCanceledClaimReleasesWithoutExplicitCancel(t *testing.T) {
	controller, runner, _ := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	claimCtx, cancelClaim := context.WithCancel(context.Background())
	wake, err := controller.PrepareWake(claimCtx)
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	cancelClaim()

	deadline := time.NewTimer(time.Second)
	defer deadline.Stop()
	for {
		next, prepareErr := controller.PrepareWake(context.Background())
		if next != nil {
			next.Cancel()
			break
		}
		if !errors.Is(prepareErr, ErrPromptActive) {
			t.Fatalf("PrepareWake after canceled claim = %#v, %v", next, prepareErr)
		}
		select {
		case <-deadline.C:
			t.Fatal("canceled claim was not released")
		default:
		}
	}
	if got := runner.runs.Load(); got != 0 {
		t.Fatalf("runner runs = %d, want 0", got)
	}
}

func TestWakeRequestCloseReleasesWithoutClosingResources(t *testing.T) {
	controller, runner, current := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	controller.RequestClose()
	if got := current.closes.Load(); got != 0 {
		t.Fatalf("session closes after RequestClose = %d, want 0", got)
	}
	if got := runner.closes.Load(); got != 0 {
		t.Fatalf("runner closes after RequestClose = %d, want 0", got)
	}
	if err := wake.Run(context.Background(), nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("Run after RequestClose = %v, want ErrClosed", err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if current.closes.Load() != 1 || runner.closes.Load() != 1 {
		t.Fatalf("close counts = session %d runner %d, want one each", current.closes.Load(), runner.closes.Load())
	}
}

func TestWakeCloseReleasesAbandonedClaimAndClosesOnce(t *testing.T) {
	controller, runner, current := newWakeContractFixture(t)
	addWakeContractNotification(runner)
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %#v, %v", wake, err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if got := current.closes.Load(); got != 1 {
		t.Fatalf("session closes = %d, want 1", got)
	}
	if got := runner.closes.Load(); got != 1 {
		t.Fatalf("runner closes = %d, want 1", got)
	}
	if err := wake.Run(context.Background(), nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("Run after Close() = %v, want ErrClosed", err)
	}
}

var _ Runner = (*wakeContractRunner)(nil)
var _ TaskLister = (*Controller)(nil)
