package tui

import (
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
)

type overlayKind uint8

const (
	overlayNone overlayKind = iota
	overlayHelp
	overlaySession
)

type showHelpOverlayMsg struct{}

type showSessionOverlayMsg struct{}

type hideOverlayMsg struct{}

type toggleDetailsMsg struct{}

// commitFlushedMsg signals that the in-flight tea.Println for the current
// pendingPrints chunk has completed, so the next queued chunk (if any) may
// now be sent. See flushNextPrintCmd.
type commitFlushedMsg struct{}

type turnEnvelope struct {
	event                 *agent.Event
	compactionResult      *agent.CompactionResult
	applicationAck        *turnApplicationAck
	aggregateUsage        otmodel.Usage
	aggregateUsagePresent bool
	err                   error
	done                  bool
	usesRegularEventSlot  bool
}

type turnApplicationAck struct {
	done chan struct{}
	once sync.Once
}

func newTurnApplicationAck() *turnApplicationAck {
	return &turnApplicationAck{done: make(chan struct{})}
}

func (a *turnApplicationAck) acknowledge() {
	if a == nil {
		return
	}
	a.once.Do(func() {
		close(a.done)
	})
}

type activeOperation struct {
	stream     *turnStream
	cancel     func()
	cancelOnce sync.Once
	wake       *app.WakeOperation
}

func (o *activeOperation) cancelContext() {
	if o == nil {
		return
	}
	o.cancelOnce.Do(func() {
		if o.wake != nil {
			o.wake.Cancel()
		}
		if o.cancel != nil {
			o.cancel()
		}
	})
}

func (o *activeOperation) abandon() {
	if o == nil {
		return
	}
	o.cancelContext()
	o.stream.abandon()
}

type operationCleanup struct {
	mu      sync.Mutex
	current *activeOperation
	closed  bool
}

func newOperationCleanup() *operationCleanup {
	return &operationCleanup{}
}

func (c *operationCleanup) register(stream *turnStream, cancel func(), wakes ...*app.WakeOperation) *activeOperation {
	var wake *app.WakeOperation
	if len(wakes) > 0 {
		wake = wakes[0]
	}
	operation := &activeOperation{stream: stream, cancel: cancel, wake: wake}
	if c == nil {
		return operation
	}
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		operation.abandon()
		return operation
	}
	previous := c.current
	c.current = operation
	c.mu.Unlock()
	previous.abandon()
	return operation
}

func (c *operationCleanup) finish(operation *activeOperation) {
	if operation == nil {
		return
	}
	if c != nil {
		c.mu.Lock()
		if c.current == operation {
			c.current = nil
		}
		c.mu.Unlock()
	}
	operation.cancelContext()
}

func (c *operationCleanup) abandon(operation *activeOperation) {
	if operation == nil {
		return
	}
	if c != nil {
		c.mu.Lock()
		if c.current == operation {
			c.current = nil
		}
		c.mu.Unlock()
	}
	operation.abandon()
}

func (c *operationCleanup) cleanup() {
	if c == nil {
		return
	}
	c.mu.Lock()
	operation := c.current
	c.current = nil
	c.closed = true
	c.mu.Unlock()
	operation.abandon()
}

type turnStream struct {
	channel           chan turnEnvelope
	regularEventSlots chan struct{}
	generation        uint64
	abandonSignal     chan struct{}
	abandonOnce       sync.Once
}

func (s *turnStream) abandon() {
	if s == nil {
		return
	}
	s.abandonOnce.Do(func() {
		close(s.abandonSignal)
	})
}

type turnMsg struct {
	channel    <-chan turnEnvelope
	stream     *turnStream
	generation uint64
	value      turnEnvelope
}

type renderStreamingMsg struct {
	generation uint64
}

type newSessionResultMsg struct {
	generation uint64
	err        error
}

type ctrlCArmExpiredMsg struct {
	generation uint64
}
